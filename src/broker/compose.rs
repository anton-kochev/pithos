//! Pure policy for a deliberately restricted Compose subset.
//!
//! [`parse`] accepts only services and logical named-volume declarations. It does
//! not read files or the environment, contact Docker, or render executable Compose.
//! Paths are checked **lexically only**: a later authority boundary must secure
//! filesystem traversal, symlinks, snapshots and races before building anything.
//!
//! Models and errors redact caller data in `Debug`; getters deliberately expose
//! literal values and must not be logged indiscriminately. Dollar signs (including
//! `$$`) are rejected, not expanded or unescaped. Acceptance is not authorization
//! to adopt existing Docker resources, execute builds, or run images.

use saphyr::{LoadableYamlNode, Mapping, Scalar, Yaml};
use saphyr_parser::{Event, Parser, ScalarStyle};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

const MAX_INPUT_BYTES: usize = 65_536;
const MAX_DEPTH: usize = 16;
const MAX_EVENTS: usize = 4096;

/// Static error categories; no input strings or parser error sources are retained.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ComposeError {
    /// Missing/unknown fields, wrong YAML types, or invalid field combinations.
    #[error("invalid Compose structure")]
    Structure,
    /// Names must match `[a-z][a-z0-9-]{0,39}` and not name broker infrastructure.
    #[error("invalid or reserved logical name")]
    Name,
    /// An input, parser, collection or string budget was exceeded.
    #[error("Compose policy limit exceeded")]
    Limit,
    /// Invalid syntax, duplicate/merge keys, aliases, anchors, tags or documents.
    #[error("unsupported or ambiguous YAML")]
    Yaml,
    /// A literal explicit-tag or SHA-256 image reference was required.
    #[error("invalid image reference")]
    Image,
    /// A path violated the lexical build or absolute mount-target policy.
    #[error("invalid lexical path")]
    Path,
    /// Literal argv/environment values must not contain dollar signs or NUL.
    #[error("expected a literal string without interpolation or NUL")]
    Literal,
    /// Dependencies must be unique, declared, non-self and acyclic.
    #[error("invalid, undeclared or cyclic dependency")]
    Dependency,
    /// A mount was undeclared, malformed, or duplicated a target.
    #[error("invalid or undeclared named volume mount")]
    Volume,
}

/// Owned validated model, constructible only through [`parse`].
pub struct Compose {
    services: BTreeMap<String, Service>,
    volumes: BTreeSet<String>,
}

impl fmt::Debug for Compose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compose")
            .field("service_count", &self.services.len())
            .field("volume_count", &self.volumes.len())
            .finish_non_exhaustive()
    }
}

impl Compose {
    /// Declared logical volumes; these are not physical Docker volume names.
    pub fn volumes(&self) -> &BTreeSet<String> {
        &self.volumes
    }

    /// Services keyed by validated logical identity, in lexical order.
    pub fn services(&self) -> &BTreeMap<String, Service> {
        &self.services
    }
}

/// Validated service with exactly one of [`Self::image`] and [`Self::build`].
pub struct Service {
    image: Option<String>,
    build: Option<Build>,
    command: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    environment: BTreeMap<String, String>,
    depends_on: Vec<String>,
    volumes: Vec<Mount>,
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

impl Service {
    /// Named-volume mounts, in input order.
    pub fn volumes(&self) -> &[Mount] {
        &self.volumes
    }

    /// Declared, unique dependencies, in input order (not a startup schedule).
    pub fn depends_on(&self) -> &[String] {
        &self.depends_on
    }

    /// Literal values, potentially secret. No host-environment inheritance.
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    /// `None` means absent; `Some([])` explicitly clears the image command.
    pub fn command(&self) -> Option<&[String]> {
        self.command.as_deref()
    }
    /// `None` means absent; `Some([])` explicitly clears the image entrypoint.
    pub fn entrypoint(&self) -> Option<&[String]> {
        self.entrypoint.as_deref()
    }

    /// Lexically relative build inputs, if no image was supplied.
    pub fn build(&self) -> Option<&Build> {
        self.build.as_ref()
    }

    /// Literal explicit-tag or SHA-256 reference; tags need not be immutable.
    pub fn image(&self) -> Option<&str> {
        self.image.as_deref()
    }
}

/// Lexical build inputs; acceptance proves neither existence nor FS containment.
pub struct Build {
    context: String,
    dockerfile: String,
}

impl fmt::Debug for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Build").finish_non_exhaustive()
    }
}

impl Build {
    /// Context relative to the project root; `.` denotes that root.
    pub fn context(&self) -> &str {
        &self.context
    }
    /// Dockerfile relative to the context; defaults to `Dockerfile`.
    pub fn dockerfile(&self) -> &str {
        &self.dockerfile
    }
}

/// A declared logical volume mounted at a unique absolute container target.
pub struct Mount {
    source: String,
    target: String,
    read_only: bool,
}

impl fmt::Debug for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mount").finish_non_exhaustive()
    }
}

impl Mount {
    /// Logical source identity, not authority to adopt a physical volume.
    pub fn source(&self) -> &str {
        &self.source
    }
    /// Absolute POSIX container path, without root/dot/parent/empty components.
    pub fn target(&self) -> &str {
        &self.target
    }
    /// True for `:ro`; omitted mode and `:rw` are read-write.
    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

/// Parse one UTF-8 YAML document without any external side effects.
///
/// Before loading a YAML tree, reject more than 64 KiB, 16 nested collections or
/// 4096 parser events, and ambiguous/unsupported YAML constructs. Services are
/// limited to 1..=32 and volume declarations to 32. Per-service limits are 64
/// argv elements, 128 environment pairs, 32 dependencies and 32 mounts. Literal
/// values are at most 4096 bytes, paths 1024, image references 512, logical names
/// 40, and environment keys/image tags 128. String bounds count UTF-8 bytes.
///
/// # Errors
/// Returns a redacted [`ComposeError`] for malformed, unsupported or over-budget
/// input. Unknown fields always fail closed. Volume declarations may use `{}` or
/// null as an empty declaration; other supplied fields must have their exact type.
pub fn parse(input: &str) -> Result<Compose, ComposeError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(ComposeError::Limit);
    }
    // The parser uses raw NUL as an end-of-stream sentinel. Reject it before
    // parsing so a valid prefix cannot hide unvalidated trailing bytes.
    if input.contains('\0') {
        return Err(ComposeError::Yaml);
    }
    preflight(input)?;
    let documents = Yaml::load_from_str(input).map_err(|_| ComposeError::Structure)?;
    let root = documents.first().ok_or(ComposeError::Structure)?;
    fields(root, &["services", "volumes"])?;
    let volumes = root
        .as_mapping_get("volumes")
        .map(parse_volumes)
        .transpose()?
        .unwrap_or_default();
    let raw_services = mapping(required(root, "services")?)?;
    if raw_services.is_empty() {
        return Err(ComposeError::Structure);
    }
    if raw_services.len() > 32 {
        return Err(ComposeError::Limit);
    }
    let mut services = BTreeMap::new();
    for (name, value) in raw_services {
        fields(
            value,
            &[
                "image",
                "build",
                "command",
                "entrypoint",
                "environment",
                "depends_on",
                "volumes",
            ],
        )?;
        let name = string(name)?;
        logical_name(name)?;
        let (image, build) = match (value.as_mapping_get("image"), value.as_mapping_get("build")) {
            (Some(image), None) => {
                let image = string(image)?;
                image_reference(image)?;
                (Some(image.to_owned()), None)
            }
            (None, Some(build)) => (None, Some(parse_build(build)?)),
            _ => return Err(ComposeError::Structure),
        };
        let command = value
            .as_mapping_get("command")
            .map(parse_argv)
            .transpose()?;
        let entrypoint = value
            .as_mapping_get("entrypoint")
            .map(parse_argv)
            .transpose()?;
        let environment = value
            .as_mapping_get("environment")
            .map(parse_environment)
            .transpose()?
            .unwrap_or_default();
        let depends_on = value
            .as_mapping_get("depends_on")
            .map(parse_dependencies)
            .transpose()?
            .unwrap_or_default();
        let mounts = value
            .as_mapping_get("volumes")
            .map(|value| parse_mounts(value, &volumes))
            .transpose()?
            .unwrap_or_default();
        services.insert(
            name.to_owned(),
            Service {
                image,
                build,
                command,
                entrypoint,
                environment,
                depends_on,
                volumes: mounts,
            },
        );
    }
    check_dependencies(&services)?;
    Ok(Compose { services, volumes })
}

fn parse_mounts(value: &Yaml<'_>, declared: &BTreeSet<String>) -> Result<Vec<Mount>, ComposeError> {
    let mut mounts = Vec::new();
    let mut targets = BTreeSet::new();
    for value in sequence(value, 32)? {
        let text = string(value)?;
        let mut parts = text.split(':');
        let source = parts.next().ok_or(ComposeError::Volume)?;
        let target = parts.next().ok_or(ComposeError::Volume)?;
        let read_only = match parts.next() {
            None | Some("rw") => false,
            Some("ro") => true,
            _ => return Err(ComposeError::Volume),
        };
        if parts.next().is_some() || !declared.contains(source) {
            return Err(ComposeError::Volume);
        }
        if target.len() > 1024 {
            return Err(ComposeError::Limit);
        }
        let relative = target.strip_prefix('/').ok_or(ComposeError::Path)?;
        relative_path(relative, true)?;
        if relative.split('/').any(|part| part == ".") || !targets.insert(target) {
            return Err(ComposeError::Volume);
        }
        mounts.push(Mount {
            source: source.to_owned(),
            target: target.to_owned(),
            read_only,
        });
    }
    Ok(mounts)
}

fn parse_volumes(value: &Yaml<'_>) -> Result<BTreeSet<String>, ComposeError> {
    let declarations = mapping(value)?;
    if declarations.len() > 32 {
        return Err(ComposeError::Limit);
    }
    let mut names = BTreeSet::new();
    for (name, options) in declarations {
        let name = string(name)?;
        logical_name(name)?;
        if !options.is_null() {
            fields(options, &[])?;
        }
        names.insert(name.to_owned());
    }
    Ok(names)
}

fn parse_dependencies(value: &Yaml<'_>) -> Result<Vec<String>, ComposeError> {
    let mut names = Vec::new();
    for value in sequence(value, 32)? {
        let name = string(value)?;
        logical_name(name)?;
        if names.iter().any(|existing| existing == name) {
            return Err(ComposeError::Dependency);
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn check_dependencies(services: &BTreeMap<String, Service>) -> Result<(), ComposeError> {
    let mut resolved = BTreeSet::new();
    while resolved.len() < services.len() {
        let before = resolved.len();
        for (name, service) in services {
            if service.depends_on.iter().all(|dep| resolved.contains(dep)) {
                resolved.insert(name);
            }
        }
        // An unknown dependency, self-edge or cycle can never become resolved.
        if resolved.len() == before {
            return Err(ComposeError::Dependency);
        }
    }
    Ok(())
}

fn parse_environment(value: &Yaml<'_>) -> Result<BTreeMap<String, String>, ComposeError> {
    let values = mapping(value)?;
    if values.len() > 128 {
        return Err(ComposeError::Limit);
    }
    let mut environment = BTreeMap::new();
    for (key, value) in values {
        let key = string(key)?;
        if key.is_empty()
            || key.len() > 128
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || key.as_bytes()[0].is_ascii_digit()
        {
            return Err(ComposeError::Structure);
        }
        environment.insert(key.to_owned(), literal(value)?.to_owned());
    }
    Ok(environment)
}

fn parse_argv(value: &Yaml<'_>) -> Result<Vec<String>, ComposeError> {
    let values = sequence(value, 64)?;
    let mut argv = Vec::with_capacity(values.len());
    for value in values {
        argv.push(literal(value)?.to_owned());
    }
    if argv.first().is_some_and(String::is_empty) {
        return Err(ComposeError::Structure);
    }
    Ok(argv)
}

fn literal<'a>(value: &'a Yaml<'_>) -> Result<&'a str, ComposeError> {
    let text = string(value)?;
    if text.len() > 4096 {
        return Err(ComposeError::Limit);
    }
    if text.contains(['$', '\0']) {
        return Err(ComposeError::Literal);
    }
    Ok(text)
}

fn sequence<'a, 'input>(
    value: &'a Yaml<'input>,
    max: usize,
) -> Result<&'a [Yaml<'input>], ComposeError> {
    let values = value.as_sequence().ok_or(ComposeError::Structure)?;
    if values.len() > max {
        return Err(ComposeError::Limit);
    }
    Ok(values)
}

fn parse_build(value: &Yaml<'_>) -> Result<Build, ComposeError> {
    let (context, dockerfile) = if let Some(context) = value.as_str() {
        (context, "Dockerfile")
    } else {
        fields(value, &["context", "dockerfile"])?;
        (
            string(required(value, "context")?)?,
            match value.as_mapping_get("dockerfile") {
                Some(value) => string(value)?,
                None => "Dockerfile",
            },
        )
    };
    relative_path(context, false)?;
    relative_path(dockerfile, true)?;
    Ok(Build {
        context: context.to_owned(),
        dockerfile: dockerfile.to_owned(),
    })
}

fn relative_path(path: &str, file: bool) -> Result<(), ComposeError> {
    if path.len() > 1024 {
        return Err(ComposeError::Limit);
    }
    if path.is_empty()
        || path.starts_with(['/', '~'])
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '$'))
        || path.split('/').any(|part| part.is_empty() || part == "..")
        || file && path.rsplit('/').next() == Some(".")
    {
        return Err(ComposeError::Path);
    }
    Ok(())
}

enum Frame {
    Mapping {
        keys: BTreeSet<String>,
        key_next: bool,
    },
    Sequence,
}

fn preflight(input: &str) -> Result<(), ComposeError> {
    let mut stack = Vec::new();
    let mut documents = 0;
    // Pull events so rejection stops parsing immediately. Do not use a receiver
    // that merely records an error while the parser continues expanding input.
    for (count, event) in Parser::new_from_str(input).enumerate() {
        if count >= MAX_EVENTS {
            return Err(ComposeError::Limit);
        }
        let (event, _) = event.map_err(|_| ComposeError::Yaml)?;
        match event {
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err(ComposeError::Yaml);
                }
            }
            Event::Alias(_) => return Err(ComposeError::Yaml),
            Event::Scalar(value, style, anchor, tag) => {
                // Saphyr does not resolve every core bool/null spelling, and
                // radix integers outside i64 fall back to strings. Require
                // quotes rather than let numeric range affect string typing.
                let radix_integer = value.strip_prefix("0x").is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit())
                }) || value.strip_prefix("0o").is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| (b'0'..=b'7').contains(&b))
                });
                if style == ScalarStyle::Plain
                    && (radix_integer
                        || matches!(
                            value.as_ref(),
                            "True" | "TRUE" | "False" | "FALSE" | "Null" | "NULL"
                        ))
                {
                    return Err(ComposeError::Yaml);
                }
                if anchor != 0 || tag.is_some() {
                    return Err(ComposeError::Yaml);
                }
                if let Some(Frame::Mapping { keys, key_next }) = stack.last_mut() {
                    if *key_next {
                        let Some(Scalar::String(key)) =
                            Scalar::parse_from_cow_and_metadata(value, style, None)
                        else {
                            return Err(ComposeError::Yaml);
                        };
                        if key == "<<" || !keys.insert(key.into_owned()) {
                            return Err(ComposeError::Yaml);
                        }
                    }
                    *key_next = !*key_next;
                }
            }
            Event::MappingStart(anchor, ref tag) | Event::SequenceStart(anchor, ref tag) => {
                if anchor != 0 || tag.is_some() {
                    return Err(ComposeError::Yaml);
                }
                if let Some(Frame::Mapping { key_next, .. }) = stack.last_mut() {
                    if *key_next {
                        return Err(ComposeError::Yaml);
                    }
                    *key_next = true;
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(ComposeError::Limit);
                }
                stack.push(if matches!(event, Event::MappingStart(..)) {
                    Frame::Mapping {
                        keys: BTreeSet::new(),
                        key_next: true,
                    }
                } else {
                    Frame::Sequence
                });
            }
            Event::MappingEnd | Event::SequenceEnd => {
                stack.pop();
            }
            _ => {}
        }
    }
    Ok(())
}

fn image_reference(image: &str) -> Result<(), ComposeError> {
    if image.len() > 512 {
        return Err(ComposeError::Limit);
    }
    let (reference, digest) = match image.split_once('@') {
        Some((reference, digest)) => {
            let hash = digest.strip_prefix("sha256:").ok_or(ComposeError::Image)?;
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(ComposeError::Image);
            }
            (reference, true)
        }
        None => (image, false),
    };
    let last = reference.rsplit('/').next().ok_or(ComposeError::Image)?;
    let repository = if let Some((_, tag)) = last.split_once(':') {
        if tag.is_empty()
            || tag.len() > 128
            || !tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            || !tag.as_bytes()[0].is_ascii_alphanumeric() && tag.as_bytes()[0] != b'_'
        {
            return Err(ComposeError::Image);
        }
        &reference[..reference.len() - tag.len() - 1]
    } else if digest {
        reference
    } else {
        return Err(ComposeError::Image);
    };
    for (index, part) in repository.split('/').enumerate() {
        let name = if index == 0 && repository.contains('/') && part.contains(':') {
            let (host, port) = part.split_once(':').ok_or(ComposeError::Image)?;
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ComposeError::Image);
            }
            host
        } else {
            part
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            || !name.as_bytes()[0].is_ascii_alphanumeric()
            || !name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
        {
            return Err(ComposeError::Image);
        }
    }
    Ok(())
}

fn logical_name(name: &str) -> Result<(), ComposeError> {
    if name.is_empty()
        || name.len() > 40
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || matches!(name, "browser" | "pithos-app")
    {
        return Err(ComposeError::Name);
    }
    Ok(())
}

fn required<'a, 'input>(
    value: &'a Yaml<'input>,
    key: &str,
) -> Result<&'a Yaml<'input>, ComposeError> {
    value.as_mapping_get(key).ok_or(ComposeError::Structure)
}

fn fields(value: &Yaml<'_>, allowed: &[&str]) -> Result<(), ComposeError> {
    for key in mapping(value)?.keys() {
        if !allowed.contains(&string(key)?) {
            return Err(ComposeError::Structure);
        }
    }
    Ok(())
}

fn mapping<'a, 'input>(value: &'a Yaml<'input>) -> Result<&'a Mapping<'input>, ComposeError> {
    value.as_mapping().ok_or(ComposeError::Structure)
}

fn string<'a>(value: &'a Yaml<'_>) -> Result<&'a str, ComposeError> {
    value.as_str().ok_or(ComposeError::Structure)
}

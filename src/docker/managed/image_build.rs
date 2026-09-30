//! Guarded host-owned build; never a launch token or daemon-side cancellation claim.
use super::{BuildStep, ImmutableImageId, ManagedDocker, PreflightError, image_cache};
use crate::{
    docker::HostIdentity,
    dockerfile, embed,
    lifecycle::{CapturedOutput, Outcome},
};
use saphyr::YamlOwned;
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

/// A project app build request. Paths are workspace-relative.
pub struct AppBuild<'a> {
    pub workspace: &'a Path,
    pub run_id: &'a str,
    pub app: &'a str,
    pub dockerfile: &'a str,
    pub context: &'a str,
}

fn trusted_directory(path: &Path, private: bool) -> Result<(), PreflightError> {
    let invalid = |_| PreflightError::InvalidSelection;
    // Canonical equality rejects lexical aliases and symlinks in any component.
    if !path.is_absolute() || fs::canonicalize(path).map_err(invalid)? != path {
        return Err(PreflightError::InvalidSelection);
    }
    // SAFETY: scalar process query with no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor).map_err(invalid)?;
        let sticky_root = ancestor != path && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        if !meta.is_dir()
            || ![0, uid].contains(&meta.uid())
            || (meta.mode() & 0o022 != 0 && !sticky_root)
            || (ancestor == path
                && (meta.uid() != uid || (private && meta.mode() & 0o777 != 0o700)))
        {
            return Err(PreflightError::InvalidSelection);
        }
    }
    Ok(())
}

fn private_root<'a>(root: &'a Path, workspace: &Path) -> Result<&'a Path, PreflightError> {
    trusted_directory(workspace, false)?;
    trusted_directory(root, true)?;
    if root.starts_with(workspace) || workspace.starts_with(root) {
        return Err(PreflightError::InvalidSelection);
    }
    Ok(root)
}

fn read_iid(path: &Path) -> Result<ImmutableImageId, PreflightError> {
    let invalid = |_| PreflightError::InvalidResponse;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(invalid)?;
    let before = file.metadata().map_err(invalid)?;
    // SAFETY: scalar process query with no pointers or failure sentinel.
    if !before.is_file()
        // The builder replaces the file under umask 022; the 0700 stage dir
        // keeps it private. Reject only writable-by-others.
        || before.mode() & 0o022 != 0
        || before.nlink() != 1
        || before.uid() != unsafe { libc::geteuid() }
        || before.len() > 72
    {
        return Err(PreflightError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    file.take(73).read_to_end(&mut bytes).map_err(invalid)?;
    let after = fs::symlink_metadata(path).map_err(invalid)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.nlink() != 1
        || after.mode() != before.mode()
        || bytes.len() > 72
    {
        return Err(PreflightError::InvalidResponse);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    ImmutableImageId::new(text.strip_suffix('\n').unwrap_or(text))
        .map_err(|_| PreflightError::InvalidResponse)
}

pub(super) fn ensure(
    docker: &mut ManagedDocker,
    yaml: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
    workspace: &Path,
    staging_root: &Path,
) -> Result<ImmutableImageId, PreflightError> {
    // Validate input, identity, and staging before ANY Docker query or filesystem write.
    let parsed = crate::config::load(pithos).map_err(|_| PreflightError::InvalidInput)?;
    let browser = crate::config::browser_config(&parsed)
        .map_err(|_| PreflightError::InvalidInput)?
        .enabled;
    if &parsed != yaml {
        return Err(PreflightError::InvalidInput);
    }
    if HostIdentity::effective().ok() != Some(identity) {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, workspace)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    let (cached, base) = image_cache::resolve_with_base(docker, yaml, pithos, identity)?;
    if let Some(id) = cached {
        return Ok(id);
    }
    let hash = image_cache::fingerprint(yaml, pithos, identity, &base)?;
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    let stage = Stage::new(root)?;
    embed::extract_with_identity_to(&stage.context).map_err(|_| PreflightError::Unavailable)?;
    // The emitted client layer copies these; its text names their fingerprint.
    if browser {
        crate::browser::assets::extract_to(&stage.context)
            .map_err(|_| PreflightError::Unavailable)?;
    }
    let emitted = dockerfile::emit_with_identity(&parsed, identity);
    // Name the base by tag: BuildKit cannot build `FROM sha256:<id>`. The pin
    // is enforced by the unchanged tag ID and the built image's layer chain.
    if !emitted.contains(&format!("FROM {} AS base", crate::docker::BASE_IMAGE_REF)) {
        return Err(PreflightError::Unsupported);
    }
    let dockerfile = stage.context.join("Dockerfile");
    fs::write(&dockerfile, emitted).map_err(|_| PreflightError::Unavailable)?;
    let context = stage.context.clone();
    run_build(
        docker,
        stage,
        &context,
        &dockerfile,
        &[format!("{}={hash}", image_cache::LABEL_KEY)],
        &format!("pithos-broker-identity:{hash}"),
        // Recheck the exact tag after staging, immediately before the CLI.
        |docker| {
            if image_cache::inspect_base(docker)? != base {
                return Err(PreflightError::Changed);
            }
            Ok(())
        },
        |docker, built| {
            image_cache::verify_candidate(docker, built, identity, &hash)?;
            image_cache::verify_layered_on(docker, built, &base)?;
            if image_cache::inspect_base(docker)? != base {
                return Err(PreflightError::Changed);
            }
            Ok(())
        },
    )
}

/// Resolve or build the identity Chromium sidecar image. Its Dockerfile pins
/// its own base by digest; the result is bound by label and the exact account.
pub(super) fn ensure_browser(
    docker: &mut ManagedDocker,
    identity: HostIdentity,
    workspace: &Path,
    staging_root: &Path,
) -> Result<ImmutableImageId, PreflightError> {
    if HostIdentity::effective().ok() != Some(identity) {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, workspace)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    let hash = crate::browser::assets::fingerprint_with_identity(identity);
    if let Some(id) = image_cache::resolve_browser(docker, identity, &hash)? {
        return Ok(id);
    }
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    let stage = Stage::new(root)?;
    crate::browser::assets::extract_with_identity_to(&stage.context, identity)
        .map_err(|_| PreflightError::Unavailable)?;
    let dockerfile = stage.context.join("browser/runtime/Dockerfile");
    let context = stage.context.clone();
    run_build(
        docker,
        stage,
        &context,
        &dockerfile,
        &[format!("{}={hash}", image_cache::BROWSER_LABEL_KEY)],
        &format!("pithos-broker-browser:{hash}"),
        |_| Ok(()),
        |docker, built| image_cache::verify_browser_candidate(docker, built, identity, &hash),
    )
}

/// Resolve or pull `postgres:<major.minor>`. The pull runs through the frozen
/// selection with a private config copy (the CLI writes into it), then the
/// result must resolve locally like a cache hit.
pub(super) fn ensure_postgres(
    docker: &mut ManagedDocker,
    version: &str,
    workspace: &Path,
    staging_root: &Path,
) -> Result<super::PostgresImage, PreflightError> {
    let exact = version.split('.').count() == 2
        && version.split('.').all(|part| {
            !part.is_empty() && part.len() <= 4 && part.bytes().all(|b| b.is_ascii_digit())
        });
    if !exact {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, workspace)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    let reference = format!("postgres:{version}");
    if let Some(image) = image_cache::resolve_postgres(docker, &reference)? {
        return Ok(image);
    }
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    let stage = Stage::new(root)?;
    match fs::read(docker.config.resolved.join("config.json")) {
        Ok(bytes) => fs::write(stage.config.join("config.json"), bytes)
            .map_err(|_| PreflightError::Unavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(PreflightError::Unavailable),
    }
    docker.check_selection()?;
    let mut command = Command::new(&docker.executable.resolved);
    command
        .env_clear()
        .env("DOCKER_CLI_TELEMETRY_OPTOUT", "1")
        .current_dir(&stage.config)
        .arg("--host")
        .arg(&docker.endpoint)
        .arg("--config")
        .arg(&stage.config)
        .args(["pull", "--quiet", &reference]);
    docker.build_stage = Some(stage.dir);
    let report = docker.build_supervisor.execute(&mut command);
    let result = (|| {
        let report = report.map_err(|_| PreflightError::Unavailable)?;
        docker.check_selection()?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || report.signal_error
            || report.wait_error
            || docker.work_shutdown.is_requested()
        {
            return Err(PreflightError::Unavailable);
        }
        image_cache::resolve_postgres(docker, &reference)?.ok_or(PreflightError::Unavailable)
    })();
    if !docker.build_supervisor.is_in_flight() {
        docker.build_stage = None;
    }
    result
}

/// A workspace-relative path that is exactly its canonical self: no `..`,
/// `.`, empty components, absolute paths or symlinks anywhere below the
/// workspace. `"."` names the workspace itself (directories only).
fn contained(workspace: &Path, relative: &str, directory: bool) -> Option<PathBuf> {
    let joined = if relative == "." && directory {
        workspace.to_owned()
    } else {
        if relative.is_empty()
            || relative.len() > 1024
            || relative.starts_with('/')
            || relative.chars().any(char::is_control)
            || relative
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return None;
        }
        workspace.join(relative)
    };
    if fs::canonicalize(&joined).ok()? != joined {
        return None;
    }
    let meta = fs::symlink_metadata(&joined).ok()?;
    (if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    })
    .then_some(joined)
}

const APP_RUN_LABEL: &str = "io.pithos.broker.app.run";
const APP_LOGICAL_LABEL: &str = "io.pithos.broker.app.logical";

/// Build a project app image. Everything is validated before any Docker
/// call; the result is labelled with the run and app and has no VOLUMEs.
pub(super) fn build_app(
    docker: &mut ManagedDocker,
    inputs: AppBuild<'_>,
    staging_root: &Path,
) -> Result<ImmutableImageId, PreflightError> {
    if !crate::broker::app::valid_app_name(inputs.app)
        || inputs.run_id.is_empty()
        || inputs.run_id.len() > 64
        || !inputs
            .run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, inputs.workspace)?;
    let dockerfile = contained(inputs.workspace, inputs.dockerfile, false)
        .ok_or(PreflightError::InvalidInput)?;
    let context =
        contained(inputs.workspace, inputs.context, true).ok_or(PreflightError::InvalidInput)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    for part in [inputs.run_id, inputs.app] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let suffix: String = hash.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let labels = [
        format!("{APP_RUN_LABEL}={}", inputs.run_id),
        format!("{APP_LOGICAL_LABEL}={}", inputs.app),
    ];
    let stage = Stage::new(root)?;
    run_build(
        docker,
        stage,
        &context,
        &dockerfile,
        &labels,
        &format!("pithos-broker-app:{suffix}"),
        |_| Ok(()),
        |docker, built| image_cache::verify_app(docker, built, inputs.run_id, inputs.app),
    )
}

/// A private 0700 stage. Only `context` is uploaded; buildx keeps its state
/// in `buildx`. The CLI gets `config`, a private copy of the frozen client
/// config: after a registry pull it writes (e.g. `.token_seed`) into it.
struct Stage {
    dir: tempfile::TempDir,
    context: PathBuf,
    buildx: PathBuf,
    config: PathBuf,
}
impl Stage {
    fn new(root: &Path) -> Result<Self, PreflightError> {
        let dir = tempfile::Builder::new()
            .prefix("pithos-image-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)
            .map_err(|_| PreflightError::Unavailable)?;
        let meta = fs::symlink_metadata(dir.path()).map_err(|_| PreflightError::Unavailable)?;
        // SAFETY: scalar process query with no pointers or failure sentinel.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(PreflightError::InvalidSelection);
        }
        let context = dir.path().join("context");
        let buildx = dir.path().join("buildx");
        let config = dir.path().join("config");
        for path in [&context, &buildx, &config] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| PreflightError::Unavailable)?;
        }
        Ok(Self {
            dir,
            context,
            buildx,
            config,
        })
    }
}

/// One builder-neutral build through the frozen selection. `before` runs
/// right before the CLI starts; `verify` must prove the built ID before it is
/// returned. The stage is retained while the supervisor may own a child.
#[allow(clippy::too_many_arguments)]
fn run_build(
    docker: &mut ManagedDocker,
    stage: Stage,
    context: &Path,
    dockerfile: &Path,
    labels: &[String],
    tag: &str,
    before: impl FnOnce(&mut ManagedDocker) -> Result<(), PreflightError>,
    verify: impl FnOnce(&mut ManagedDocker, &ImmutableImageId) -> Result<(), PreflightError>,
) -> Result<ImmutableImageId, PreflightError> {
    let iid = stage.context.join("image.iid");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&iid)
        .map_err(|_| PreflightError::Unavailable)?;
    drop(file);
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    before(docker)?;
    // Same content as the frozen config; check_selection proves it unchanged.
    match fs::read(docker.config.resolved.join("config.json")) {
        Ok(bytes) => fs::write(stage.config.join("config.json"), bytes)
            .map_err(|_| PreflightError::Unavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(PreflightError::Unavailable),
    }
    docker.check_selection()?;
    let mut command = Command::new(&docker.executable.resolved);
    command
        .env_clear()
        .env("BUILDX_CONFIG", &stage.buildx)
        // Plain image, no provenance/SBOM manifests, no CLI telemetry.
        .env("BUILDX_NO_DEFAULT_ATTESTATIONS", "1")
        .env("DOCKER_CLI_TELEMETRY_OPTOUT", "1")
        .current_dir(&stage.config)
        .arg("--host")
        .arg(&docker.endpoint)
        .arg("--config")
        .arg(&stage.config)
        // Builder-neutral flags only: without buildx, Docker falls back to the
        // legacy builder, which rejects BuildKit-only flags.
        .arg("build")
        // Quiet keeps a failure's step summary small enough to be captured.
        .arg("-q")
        .arg("--pull=false")
        .arg("-f")
        .arg(dockerfile);
    for label in labels {
        command.arg("--label").arg(label);
    }
    command
        // The containerd image store drops unnamed build results.
        .arg("--tag")
        .arg(tag)
        .arg("--iidfile")
        .arg(&iid)
        .arg(context);
    docker.build_stage = Some(stage.dir);
    let report = docker.build_supervisor.execute(&mut command);
    let result = (|| {
        let report = report.map_err(|_| PreflightError::Unavailable)?;
        docker.check_selection()?;
        if report.signal_error || report.wait_error || docker.work_shutdown.is_requested() {
            return Err(PreflightError::Unavailable);
        }
        match report.outcome {
            Outcome::Exited(status) if status.success() => {}
            Outcome::Exited(_) => {
                return Err(PreflightError::BuildFailed(failed_step(
                    &report.stderr,
                    dockerfile,
                )));
            }
            _ => return Err(PreflightError::Unavailable),
        }
        let built = read_iid(&iid)?;
        verify(docker, &built)?;
        Ok(built)
    })();
    if !docker.build_supervisor.is_in_flight() {
        docker.build_stage = None;
    }
    result
}

/// The kind of Dockerfile step a failed build stopped at. Reads only the
/// step's line number from stderr; neither stderr nor the step text is kept.
fn failed_step(stderr: &CapturedOutput, dockerfile: &Path) -> BuildStep {
    if stderr.read_error {
        return BuildStep::Unknown;
    }
    step_at(stderr.raw_bytes(), dockerfile)
}

fn step_at(stderr: &[u8], dockerfile: &Path) -> BuildStep {
    let Some(line) = dockerfile
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| failed_line(stderr, name))
    else {
        return BuildStep::Unknown;
    };
    fs::read_to_string(dockerfile)
        .ok()
        .and_then(|text| text.lines().nth(line - 1).map(classify))
        .unwrap_or(BuildStep::Unknown)
}

/// 1-based line from the `<file name>:<N>` header of a quiet build's summary.
fn failed_line(stderr: &[u8], file_name: &str) -> Option<usize> {
    String::from_utf8_lossy(stderr).lines().find_map(|line| {
        let number = line.trim().strip_prefix(file_name)?.strip_prefix(':')?;
        if !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        number.parse().ok().filter(|&n| n > 0)
    })
}

/// Fixed label for one Dockerfile line; the first matching rule wins.
fn classify(line: &str) -> BuildStep {
    let trimmed = line.trim();
    if trimmed.starts_with("FROM ") {
        BuildStep::BaseImage
    } else if line.contains("/opt/pi-patches") {
        BuildStep::PiPatches
    } else if line.contains("pi-coding-agent") {
        BuildStep::PiInstall
    } else if line.contains("-install.sh") {
        BuildStep::Toolchain
    } else if line.contains("identity-image.py") || line.contains("identity_image.py") {
        BuildStep::Account
    } else if line.contains("pithos-browser") || line.contains("playwright") {
        BuildStep::Browser
    } else if trimmed.starts_with("RUN apt-get") {
        BuildStep::Packages
    } else {
        BuildStep::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_emitted_steps_to_fixed_labels() {
        // Arrange
        let cases = [
            (
                "FROM ghcr.io/anton-kochev/pithos:base AS base",
                BuildStep::BaseImage,
            ),
            (
                "RUN set -e; for p in /opt/pi-patches/*.mjs; do node \"$p\"; done",
                BuildStep::PiPatches,
            ),
            ("COPY pi-patches/ /opt/pi-patches/", BuildStep::PiPatches),
            (
                "RUN npm install -g @earendil-works/pi-coding-agent@0.99.1",
                BuildStep::PiInstall,
            ),
            (
                "RUN /usr/local/bin/dotnet-install.sh 10.0.102",
                BuildStep::Toolchain,
            ),
            ("RUN python3 /tmp/identity-image.py pi", BuildStep::Account),
            (
                "COPY browser/client/ /opt/pithos-browser/client/",
                BuildStep::Browser,
            ),
            (
                "RUN apt-get update && apt-get install -y git",
                BuildStep::Packages,
            ),
            (
                "COPY entrypoint.sh /usr/local/bin/entrypoint.sh",
                BuildStep::Other,
            ),
        ];

        for (line, expected) in cases {
            // Act
            let step = classify(line);

            // Assert
            assert_eq!(step, expected, "{line}");
        }
    }

    #[test]
    fn failed_line_reads_the_quiet_build_summary_header() {
        // Arrange
        let stderr = b"Dockerfile:3\n--------------------\n   1 |     FROM ghcr.io/anton-kochev/pithos:base AS base\n   3 | >>> RUN set -e; for p in /opt/pi-patches/*.mjs; do ...\n--------------------\nERROR: failed to build: failed to solve: process \"/bin/sh -c ...\" did not complete successfully: exit code: 1\n";

        // Act
        let line = failed_line(stderr, "Dockerfile");

        // Assert
        assert_eq!(line, Some(3));
    }

    #[test]
    fn failed_line_rejects_output_without_a_header() {
        // Arrange
        let cases: [&[u8]; 4] = [
            b"ERROR: failed to solve: exit code: 1\n",
            b"Dockerfile:\n",
            b"Dockerfile:0\n",
            b"Other.Dockerfile:3x\n",
        ];

        for stderr in cases {
            // Act
            let line = failed_line(stderr, "Dockerfile");

            // Assert
            assert_eq!(line, None);
        }
    }

    #[test]
    fn step_at_classifies_the_line_the_summary_names() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let dockerfile = dir.path().join("Dockerfile");
        let yaml = crate::config::load(b"toolchains: {}\npi:\n  version: \"0.99.1\"\n").unwrap();
        let text = dockerfile::emit(&yaml);
        fs::write(&dockerfile, &text).expect("write");
        let line_of = |needle: &str| {
            text.lines()
                .position(|line| line.contains(needle))
                .expect("emitted line")
                + 1
        };
        let summary = |line: usize| format!("Dockerfile:{line}\nERROR: failed to build\n");

        // Act
        let patch = step_at(
            summary(line_of("for p in /opt/pi-patches")).as_bytes(),
            &dockerfile,
        );
        let install = step_at(summary(line_of("npm install")).as_bytes(), &dockerfile);
        let past_end = step_at(summary(10_000).as_bytes(), &dockerfile);
        let unmatched = step_at(b"ERROR: failed to build\n", &dockerfile);

        // Assert
        assert_eq!(patch, BuildStep::PiPatches);
        assert_eq!(install, BuildStep::PiInstall);
        assert_eq!(past_end, BuildStep::Unknown);
        assert_eq!(unmatched, BuildStep::Unknown);
    }

    #[test]
    fn failed_step_is_unknown_without_readable_stderr() {
        // Arrange
        let mut stderr = CapturedOutput::default();
        stderr.read_error = true;

        // Act
        let step = failed_step(&stderr, Path::new("/nonexistent/Dockerfile"));

        // Assert
        assert_eq!(step, BuildStep::Unknown);
    }
}

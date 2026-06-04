use std::fmt;

pub(super) const VALID_TOP_LEVEL: &[&str] = &["toolchains", "extras", "pi"];
pub(super) const VALID_EXTRAS: &[&str] = &["apt"];
pub(super) const VALID_PI: &[&str] = &["version", "extensions"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(".pithos: not valid UTF-8")]
    NotUtf8,

    /// `line` and `col` are 1-based, as reported by `saphyr::Marker`.
    #[error(".pithos line {line}:{col}: {msg}")]
    Parse {
        line: usize,
        col: usize,
        msg: String,
    },

    #[error(".pithos: missing required key `toolchains`")]
    MissingToolchains,

    #[error(".pithos: `toolchains` must be a mapping")]
    ToolchainsNotMapping,

    #[error(".pithos: top-level keys must be strings")]
    NonStringTopLevelKey,

    #[error(".pithos: `toolchains` keys must be strings")]
    NonStringToolchainKey,

    #[error(
        ".pithos: unknown top-level key `{key}`; valid keys: {}",
        ListBackticked(VALID_TOP_LEVEL)
    )]
    UnknownTopLevelKey { key: String },

    #[error(
        ".pithos: unknown toolchain `{name}`; valid: {}",
        ListBackticked(crate::embed::VALID_TOOLCHAINS)
    )]
    UnknownToolchain { name: String },

    #[error(".pithos toolchains.{toolchain}: version `{value}` is not accepted; specify an exact version like `1.85.0`")]
    FloatingVersionRejected { toolchain: String, value: String },

    #[error(".pithos toolchains.{toolchain}: version `{value}` must match `N`, `N.N`, or `N.N.N` (digits only)")]
    InvalidVersion { toolchain: String, value: String },

    #[error(".pithos toolchains.{toolchain}: version must be a quoted string; wrap the value in quotes, e.g. `{toolchain}: \"1.85.0\"`")]
    NonStringVersion { toolchain: String },

    #[error(".pithos toolchains.{toolchain}: expected a version string, got a mapping; use the flat form, e.g. `{toolchain}: \"1.85.0\"` (nested `version:` keys are not supported)")]
    MappingVersion { toolchain: String },

    #[error(".pithos: `extras` must be a mapping")]
    ExtrasNotMapping,

    #[error(".pithos: `extras` keys must be strings")]
    NonStringExtrasKey,

    #[error(
        ".pithos extras.{key}: unknown key; valid: {}",
        ListBackticked(VALID_EXTRAS)
    )]
    UnknownExtrasKey { key: String },

    #[error(".pithos extras.apt: must be a sequence")]
    AptNotSequence,

    #[error(".pithos extras.apt[{index}]: must be a string")]
    AptEntryNotString { index: usize },

    #[error(".pithos extras.apt: invalid package name `{entry}`; must match `^[a-z0-9][a-z0-9.+-]+$`")]
    InvalidAptPackageName { entry: String },

    #[error(".pithos: `pi` must be a mapping")]
    PiNotMapping,

    #[error(".pithos: `pi` keys must be strings")]
    NonStringPiKey,

    #[error(
        ".pithos pi.{key}: unknown key; valid: {}",
        ListBackticked(VALID_PI)
    )]
    UnknownPiKey { key: String },

    #[error(".pithos pi: missing required key `version`")]
    MissingPiVersion,

    #[error(".pithos pi.version: must be a quoted string; wrap the value in quotes, e.g. `version: \"0.75.3\"`")]
    NonStringPiVersion,

    #[error(".pithos pi.version: version `{value}` must match `N`, `N.N`, or `N.N.N` (digits only)")]
    InvalidPiVersion { value: String },

    #[error(".pithos pi.version: version `{value}` is not accepted; specify an exact version like `0.75.3`")]
    FloatingPiVersionRejected { value: String },

    #[error(".pithos pi.extensions: must be a mapping")]
    ExtensionsNotMapping,

    #[error(".pithos pi.extensions: keys must be strings")]
    NonStringExtensionName,

    #[error(".pithos pi.extensions.{name}: must be a quoted string with prefix `npm:` or `git:`, e.g. `\"npm:0.10.7\"` or `\"git:https://example.com/repo#v1.0.0\"`")]
    NonStringExtensionSpec { name: String },

    #[error(".pithos pi.extensions.{name}: spec `{value}` must start with `npm:` or `git:`")]
    InvalidExtensionPrefix { name: String, value: String },

    #[error(".pithos pi.extensions.{name}: npm version `{value}` is not accepted; specify an exact version like `npm:0.10.7`")]
    FloatingExtensionVersionRejected { name: String, value: String },

    #[error(".pithos pi.extensions.{name}: npm version `{value}` must match `N`, `N.N`, or `N.N.N` (digits only)")]
    InvalidExtensionVersion { name: String, value: String },

    #[error(".pithos pi.extensions.{name}: git spec `{value}` must include a ref, e.g. `git:https://example.com/repo#v1.0.0`")]
    MissingExtensionGitRef { name: String, value: String },

    #[error(".pithos pi.extensions.{name}: git URL is empty")]
    EmptyExtensionGitUrl { name: String },

    #[error(".pithos pi.extensions.{name}: git ref is empty")]
    EmptyExtensionGitRef { name: String },
}

struct ListBackticked(&'static [&'static str]);

impl fmt::Display for ListBackticked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for item in self.0 {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "`{item}`")?;
            first = false;
        }
        Ok(())
    }
}

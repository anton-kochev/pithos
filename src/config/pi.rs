use saphyr::YamlOwned;

use super::error::{ConfigError, VALID_PI};
use super::version::is_valid_version;

pub(super) fn validate(pi: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = pi.as_mapping() else {
        return Err(ConfigError::PiNotMapping);
    };

    let mut version: Option<&YamlOwned> = None;
    let mut extensions: Option<&YamlOwned> = None;
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringPiKey);
        };
        if !VALID_PI.contains(&name) {
            return Err(ConfigError::UnknownPiKey {
                key: name.to_string(),
            });
        }
        match name {
            "version" => version = Some(value),
            "extensions" => extensions = Some(value),
            _ => unreachable!("VALID_PI gate above rejects other keys"),
        }
    }

    let Some(version) = version else {
        return Err(ConfigError::MissingPiVersion);
    };
    validate_version(version)?;
    if let Some(extensions) = extensions {
        validate_extensions(extensions)?;
    }
    Ok(())
}

fn validate_version(value: &YamlOwned) -> Result<(), ConfigError> {
    let Some(version) = value.as_str() else {
        return Err(ConfigError::NonStringPiVersion);
    };
    if matches!(version, "stable" | "nightly" | "latest") {
        return Err(ConfigError::FloatingPiVersionRejected {
            value: version.to_string(),
        });
    }
    if !is_valid_version(version) {
        return Err(ConfigError::InvalidPiVersion {
            value: version.to_string(),
        });
    }
    Ok(())
}

fn validate_extensions(extensions: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = extensions.as_mapping() else {
        return Err(ConfigError::ExtensionsNotMapping);
    };
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringExtensionName);
        };
        let Some(spec) = value.as_str() else {
            return Err(ConfigError::NonStringExtensionSpec {
                name: name.to_string(),
            });
        };
        if let Some(rest) = spec.strip_prefix("npm:") {
            validate_npm_spec(name, rest)?;
        } else if let Some(rest) = spec.strip_prefix("git:") {
            validate_git_spec(name, rest)?;
        } else {
            return Err(ConfigError::InvalidExtensionPrefix {
                name: name.to_string(),
                value: spec.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_npm_spec(name: &str, version: &str) -> Result<(), ConfigError> {
    if matches!(version, "stable" | "nightly" | "latest") {
        return Err(ConfigError::FloatingExtensionVersionRejected {
            name: name.to_string(),
            value: version.to_string(),
        });
    }
    if !is_valid_version(version) {
        return Err(ConfigError::InvalidExtensionVersion {
            name: name.to_string(),
            value: version.to_string(),
        });
    }
    Ok(())
}

fn validate_git_spec(name: &str, rest: &str) -> Result<(), ConfigError> {
    let Some((url, gitref)) = rest.rsplit_once('#') else {
        return Err(ConfigError::MissingExtensionGitRef {
            name: name.to_string(),
            value: format!("git:{rest}"),
        });
    };
    if url.is_empty() {
        return Err(ConfigError::EmptyExtensionGitUrl {
            name: name.to_string(),
        });
    }
    if gitref.is_empty() {
        return Err(ConfigError::EmptyExtensionGitRef {
            name: name.to_string(),
        });
    }
    Ok(())
}

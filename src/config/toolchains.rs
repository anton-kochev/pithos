use saphyr::YamlOwned;

use super::error::ConfigError;
use super::version::is_valid_version;

pub(super) fn validate(toolchains: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = toolchains.as_mapping() else {
        return Err(ConfigError::ToolchainsNotMapping);
    };

    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringToolchainKey);
        };
        if !crate::embed::VALID_TOOLCHAINS.contains(&name) {
            return Err(ConfigError::UnknownToolchain {
                name: name.to_string(),
            });
        }
        validate_version(name, value)?;
    }

    Ok(())
}

fn validate_version(toolchain: &str, value: &YamlOwned) -> Result<(), ConfigError> {
    let Some(version) = value.as_str() else {
        return Err(ConfigError::NonStringVersion {
            toolchain: toolchain.to_string(),
        });
    };
    if matches!(version, "stable" | "nightly" | "latest") {
        return Err(ConfigError::FloatingVersionRejected {
            toolchain: toolchain.to_string(),
            value: version.to_string(),
        });
    }
    if !is_valid_version(version) {
        return Err(ConfigError::InvalidVersion {
            toolchain: toolchain.to_string(),
            value: version.to_string(),
        });
    }
    Ok(())
}


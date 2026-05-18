use saphyr::YamlOwned;

use super::error::{ConfigError, VALID_PI};
use super::version::is_valid_version;

pub(super) fn validate(pi: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = pi.as_mapping() else {
        return Err(ConfigError::PiNotMapping);
    };

    let mut version: Option<&YamlOwned> = None;
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringPiKey);
        };
        if !VALID_PI.contains(&name) {
            return Err(ConfigError::UnknownPiKey {
                key: name.to_string(),
            });
        }
        // Only `version` reaches here — the VALID_PI gate above rejects
        // anything else, so no inner key match is needed.
        version = Some(value);
    }

    let Some(version) = version else {
        return Err(ConfigError::MissingPiVersion);
    };
    validate_version(version)
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

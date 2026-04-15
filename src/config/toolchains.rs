use saphyr::YamlOwned;

use super::error::{ConfigError, VALID_TOOLCHAINS};

pub(super) fn validate(toolchains: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = toolchains.as_mapping() else {
        return Err(ConfigError::ToolchainsNotMapping);
    };

    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringToolchainKey);
        };
        if !VALID_TOOLCHAINS.contains(&name) {
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

fn is_valid_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::is_valid_version;

    #[test]
    fn accepts_one_two_three_segment_numeric_versions() {
        assert!(is_valid_version("1"));
        assert!(is_valid_version("10"));
        assert!(is_valid_version("1.85"));
        assert!(is_valid_version("10.0.102"));
    }

    #[test]
    fn rejects_four_segment_versions() {
        assert!(!is_valid_version("1.2.3.4"));
    }

    #[test]
    fn rejects_non_digit_segments() {
        assert!(!is_valid_version("1.85-beta"));
        assert!(!is_valid_version("v1.2"));
        assert!(!is_valid_version("1..2"));
        assert!(!is_valid_version(""));
        assert!(!is_valid_version(".1"));
        assert!(!is_valid_version("1."));
    }
}

use saphyr::YamlOwned;

use super::error::{ConfigError, VALID_EXTRAS};

/// The Debian-safe package-name pattern. Mirrored verbatim in
/// `ConfigError::InvalidAptPackageName`'s `#[error(...)]` string and
/// re-implemented by `is_valid_apt_name`. Keep all three in sync.
pub(super) const APT_NAME_PATTERN: &str = "^[a-z0-9][a-z0-9.+-]+$";

pub(super) fn validate(extras: &YamlOwned) -> Result<(), ConfigError> {
    if extras.is_null() {
        return Ok(());
    }
    let Some(mapping) = extras.as_mapping() else {
        return Err(ConfigError::ExtrasNotMapping);
    };

    let mut apt: Option<&YamlOwned> = None;
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringExtrasKey);
        };
        if !VALID_EXTRAS.contains(&name) {
            return Err(ConfigError::UnknownExtrasKey {
                key: name.to_string(),
            });
        }
        if name == "apt" {
            apt = Some(value);
        }
    }

    let Some(apt) = apt else {
        return Ok(());
    };
    if apt.is_null() {
        return Ok(());
    }
    let Some(seq) = apt.as_sequence() else {
        return Err(ConfigError::AptNotSequence);
    };

    for (index, item) in seq.iter().enumerate() {
        let Some(name) = item.as_str() else {
            return Err(ConfigError::AptEntryNotString { index });
        };
        if !is_valid_apt_name(name) {
            return Err(ConfigError::InvalidAptPackageName {
                entry: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Implements `APT_NAME_PATTERN`. Keep in sync.
fn is_valid_apt_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|&b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'+' || b == b'-'
    })
}

#[cfg(test)]
mod tests {
    use super::is_valid_apt_name;

    #[test]
    fn accepts_valid_package_names() {
        assert!(is_valid_apt_name("git"));
        assert!(is_valid_apt_name("libssl3"));
        assert!(is_valid_apt_name("g++"));
        assert!(is_valid_apt_name("lib.ssl"));
        assert!(is_valid_apt_name("a0"));
        assert!(is_valid_apt_name("ca-certificates"));
        assert!(is_valid_apt_name("python3.11"));
    }

    #[test]
    fn rejects_empty_and_too_short() {
        assert!(!is_valid_apt_name(""));
        assert!(!is_valid_apt_name("a"));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(!is_valid_apt_name("Git"));
    }

    #[test]
    fn rejects_invalid_leading_char() {
        assert!(!is_valid_apt_name("-git"));
        assert!(!is_valid_apt_name(".git"));
    }

    #[test]
    fn rejects_invalid_trailing_char() {
        assert!(!is_valid_apt_name("git!"));
        assert!(!is_valid_apt_name("git_"));
    }

    #[test]
    fn rejects_whitespace_and_shell_metacharacters() {
        assert!(!is_valid_apt_name("git pkg"));
        assert!(!is_valid_apt_name("git;rm"));
    }

    #[test]
    fn rejects_non_ascii() {
        assert!(!is_valid_apt_name("café"));
    }
}

use saphyr::YamlOwned;

use super::{ConfigError, version::is_valid_version};

/// A database the broker starts next to Pi for the next invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConfig {
    /// Exact `major.minor`, the official image tag; a major alone floats.
    pub version: String,
    pub database: String,
}

fn error(message: impl Into<String>) -> ConfigError {
    ConfigError::Postgres(message.into())
}

// Lowercase so it never needs quoting in SQL; 63 bytes is Postgres' limit.
fn valid_database(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Validate the optional `postgres` block. Every key is required.
pub fn postgres_config(doc: &YamlOwned) -> Result<Option<PostgresConfig>, ConfigError> {
    let Some(value) = doc.as_mapping().and_then(|mapping| {
        mapping
            .iter()
            .find(|(key, _)| key.as_str() == Some("postgres"))
            .map(|(_, value)| value)
    }) else {
        return Ok(None);
    };
    let mapping = value
        .as_mapping()
        .ok_or_else(|| error("must be a mapping"))?;
    let (mut version, mut database) = (None, None);
    for (key, value) in mapping {
        match key.as_str() {
            Some("version") => {
                let text = value
                    .as_str()
                    .ok_or_else(|| error("version must be a quoted string, e.g. \"17.10\""))?;
                if !is_valid_version(text) || text.split('.').count() != 2 {
                    return Err(error(
                        "version must be an exact `major.minor`, e.g. \"17.10\"",
                    ));
                }
                version = Some(text.to_owned());
            }
            Some("database") => {
                let text = value.as_str().filter(|name| valid_database(name));
                database = Some(
                    text.ok_or_else(|| error("database must match [a-z_][a-z0-9_]{0,62}"))?
                        .to_owned(),
                );
            }
            Some(key) => {
                return Err(error(format!(
                    "unknown key `{key}`; valid keys: `version`, `database`"
                )));
            }
            None => return Err(error("keys must be strings")),
        }
    }
    Ok(Some(PostgresConfig {
        version: version.ok_or_else(|| error("missing required key `version`"))?,
        database: database.ok_or_else(|| error("missing required key `database`"))?,
    }))
}

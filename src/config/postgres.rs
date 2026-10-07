use saphyr::YamlOwned;

use super::{ConfigError, version::is_valid_version};

/// A database the broker starts next to Pi for the next invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConfig {
    /// Exact `major.minor`, the official image tag; a major alone floats.
    pub version: String,
    pub database: String,
    /// Optional connection limit; `None` keeps Postgres' default of 100.
    pub max_connections: Option<u32>,
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

/// Validate the optional `postgres` block. `version` and `database` are required.
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
    let (mut version, mut database, mut max_connections) = (None, None, None);
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
            Some("max_connections") => {
                // Bounded: each connection reserves shared memory in a 1 GiB container.
                let limit = value
                    .as_integer()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (20..=1000).contains(n));
                max_connections =
                    Some(limit.ok_or_else(|| {
                        error("max_connections must be an integer from 20 to 1000")
                    })?);
            }
            Some(key) => {
                return Err(error(format!(
                    "unknown key `{key}`; valid keys: `version`, `database`, `max_connections`"
                )));
            }
            None => return Err(error("keys must be strings")),
        }
    }
    Ok(Some(PostgresConfig {
        version: version.ok_or_else(|| error("missing required key `version`"))?,
        database: database.ok_or_else(|| error("missing required key `database`"))?,
        max_connections,
    }))
}

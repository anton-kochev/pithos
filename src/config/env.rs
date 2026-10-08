use saphyr::YamlOwned;

use super::ConfigError;

/// A value Pithos knows only at run time: the managed database's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostgresField {
    Host,
    Port,
    User,
    Password,
    Database,
    Url,
}

impl PostgresField {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "postgres.host" => Self::Host,
            "postgres.port" => Self::Port,
            "postgres.user" => Self::User,
            "postgres.password" => Self::Password,
            "postgres.database" => Self::Database,
            "postgres.url" => Self::Url,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Postgres(PostgresField),
}

/// Extra variables for Pi's environment, in declared order. Values may name
/// the managed database with `${postgres.<field>}`; `$$` is a literal `$`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvConfig {
    variables: Vec<(String, Vec<Part>)>,
}

impl EnvConfig {
    /// Whether any value needs the database's run-time coordinates.
    pub fn uses_postgres(&self) -> bool {
        self.variables
            .iter()
            .flat_map(|(_, parts)| parts)
            .any(|part| matches!(part, Part::Postgres(_)))
    }

    /// `KEY=value\n` lines for Pi's private env file.
    pub fn render(&self, postgres: &dyn Fn(PostgresField) -> String) -> String {
        let mut out = String::new();
        for (key, parts) in &self.variables {
            out.push_str(key);
            out.push('=');
            for part in parts {
                match part {
                    Part::Text(text) => out.push_str(text),
                    Part::Postgres(field) => out.push_str(&postgres(*field)),
                }
            }
            out.push('\n');
        }
        out
    }
}

fn error(message: impl Into<String>) -> ConfigError {
    ConfigError::Env(message.into())
}

// What `docker run --env-file` and every shell accept, plus `-` and `.` for
// .NET-style names such as `ConnectionStrings__app-admin`.
fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

// Pithos sets these itself; a project value would silently replace them.
fn reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("PITHOS_") || upper.starts_with("GIT_CONFIG_")
}

// Echo a placeholder only when it is plainly a name, never arbitrary text.
fn describe(placeholder: &str) -> String {
    let plain = !placeholder.is_empty()
        && placeholder.len() <= 64
        && placeholder
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'));
    if plain {
        format!("unknown placeholder `${{{placeholder}}}`")
    } else {
        "unknown placeholder".into()
    }
}

fn parse_value(key: &str, value: &str) -> Result<Vec<Part>, ConfigError> {
    // `--env-file` is line-based: a line break would start another variable.
    if value.contains(['\n', '\r', '\0']) {
        return Err(error(format!("`{key}` must be a single line")));
    }
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = value;
    while let Some(at) = rest.find('$') {
        text.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            text.push('$');
            rest = tail;
        } else if let Some(body) = after.strip_prefix('{') {
            let end = body
                .find('}')
                .ok_or_else(|| error(format!("`{key}` has an unclosed placeholder")))?;
            let name = &body[..end];
            let field = PostgresField::parse(name)
                .ok_or_else(|| error(format!("`{key}`: {}", describe(name))))?;
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Postgres(field));
            rest = &body[end + 1..];
        } else {
            text.push('$');
            rest = after;
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

/// Validate the optional `env` block. A `${postgres.*}` placeholder needs a
/// `postgres` block, which in turn needs `--broker=workspace`.
pub fn env_config(doc: &YamlOwned) -> Result<Option<EnvConfig>, ConfigError> {
    let Some(mapping) = doc.as_mapping() else {
        return Ok(None);
    };
    let Some(value) = mapping
        .iter()
        .find(|(key, _)| key.as_str() == Some("env"))
        .map(|(_, value)| value)
    else {
        return Ok(None);
    };
    let entries = value
        .as_mapping()
        .ok_or_else(|| error("must be a mapping of variable names to quoted strings"))?;
    let mut variables = Vec::new();
    for (key, value) in entries {
        let key = key
            .as_str()
            .filter(|name| valid_name(name))
            .ok_or_else(|| {
                error("a key is not a valid variable name ([A-Za-z_][A-Za-z0-9_.-]*)")
            })?;
        if reserved(key) {
            return Err(error(format!(
                "`{key}` is reserved for Pithos (PITHOS_* and GIT_CONFIG_*)"
            )));
        }
        let text = value
            .as_str()
            .ok_or_else(|| error(format!("`{key}` must be a quoted string")))?;
        variables.push((key.to_owned(), parse_value(key, text)?));
    }
    let config = EnvConfig { variables };
    let has_postgres = mapping
        .iter()
        .any(|(key, _)| key.as_str() == Some("postgres"));
    if config.uses_postgres() && !has_postgres {
        return Err(error(
            "a ${postgres.*} placeholder needs a `postgres` block",
        ));
    }
    Ok(Some(config))
}

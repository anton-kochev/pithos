use saphyr::{LoadableYamlNode, ScalarOwned, YamlOwned};

const VALID_TOP_LEVEL: &[&str] = &["toolchains"];

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

    #[error(".pithos: unknown top-level key `{key}`; valid keys: {valid}")]
    UnknownTopLevelKey { key: String, valid: String },
}

fn valid_keys_display() -> String {
    VALID_TOP_LEVEL
        .iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_or_empty(docs: Vec<YamlOwned>) -> YamlOwned {
    docs.into_iter()
        .next()
        .unwrap_or(YamlOwned::Value(ScalarOwned::Null))
}

fn validate_top_level(doc: &YamlOwned) -> Result<(), ConfigError> {
    let Some(mapping) = doc.as_mapping() else {
        // Null, scalar, or sequence — none of these carry a `toolchains` key.
        return Err(ConfigError::MissingToolchains);
    };

    let mut saw_toolchains = false;
    for key in mapping.keys() {
        if let Some(name) = key.as_str() {
            if !VALID_TOP_LEVEL.contains(&name) {
                return Err(ConfigError::UnknownTopLevelKey {
                    key: name.to_string(),
                    valid: valid_keys_display(),
                });
            }
            if name == "toolchains" {
                saw_toolchains = true;
            }
        }
    }

    if !saw_toolchains {
        return Err(ConfigError::MissingToolchains);
    }

    Ok(())
}

pub fn load(bytes: &[u8]) -> Result<YamlOwned, ConfigError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ConfigError::NotUtf8)?;
    let docs = YamlOwned::load_from_str(text).map_err(|e| {
        let m = e.marker();
        ConfigError::Parse {
            line: m.line(),
            col: m.col(),
            msg: e.to_string(),
        }
    })?;
    // Multi-doc rejection is deferred per the 1.1 plan — take the first.
    let doc = first_or_empty(docs);
    validate_top_level(&doc)?;
    Ok(doc)
}

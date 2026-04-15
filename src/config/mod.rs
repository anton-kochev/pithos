mod error;
mod toolchains;

use saphyr::{LoadableYamlNode, ScalarOwned, YamlOwned};

pub use error::ConfigError;
use error::VALID_TOP_LEVEL;

fn first_or_empty(docs: Vec<YamlOwned>) -> YamlOwned {
    docs.into_iter()
        .next()
        .unwrap_or(YamlOwned::Value(ScalarOwned::Null))
}

fn validate_top_level(doc: &YamlOwned) -> Result<&YamlOwned, ConfigError> {
    let Some(mapping) = doc.as_mapping() else {
        // Null, scalar, or sequence — none of these carry a `toolchains` key.
        return Err(ConfigError::MissingToolchains);
    };

    let mut toolchains: Option<&YamlOwned> = None;
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err(ConfigError::NonStringTopLevelKey);
        };
        if !VALID_TOP_LEVEL.contains(&name) {
            return Err(ConfigError::UnknownTopLevelKey {
                key: name.to_string(),
            });
        }
        if name == "toolchains" {
            toolchains = Some(value);
        }
    }

    toolchains.ok_or(ConfigError::MissingToolchains)
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
    let toolchains = validate_top_level(&doc)?;
    toolchains::validate(toolchains)?;
    Ok(doc)
}

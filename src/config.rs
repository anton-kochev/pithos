use saphyr::{LoadableYamlNode, ScalarOwned, YamlOwned};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(".pithos: not valid UTF-8")]
    NotUtf8,

    /// `line` and `col` are 1-based, as reported by `saphyr::Marker`.
    #[error(".pithos line {line}:{col}: {msg}")]
    Parse { line: usize, col: usize, msg: String },
}

/// An empty .pithos document (no content) is represented as Null;
/// schema validation in Story 1.3 will reject when required fields are missing.
fn first_or_empty(docs: Vec<YamlOwned>) -> YamlOwned {
    docs.into_iter()
        .next()
        .unwrap_or(YamlOwned::Value(ScalarOwned::Null))
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
    Ok(first_or_empty(docs))
}

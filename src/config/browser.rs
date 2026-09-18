use saphyr::YamlOwned;

use super::ConfigError;

/// Browser configuration describes the next invocation, never live state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub mode: BrowserMode,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    #[default]
    Interactive,
    Headless,
}

impl BrowserMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Headless => "headless",
        }
    }
}

/// Read the default-off contract and validate every supplied field, including
/// mode on disabled configurations. Unlike omission, an explicit null is invalid.
pub fn browser_config(doc: &YamlOwned) -> Result<BrowserConfig, ConfigError> {
    let Some(value) = doc.as_mapping().and_then(|mapping| {
        mapping
            .iter()
            .find(|(key, _)| key.as_str() == Some("browser"))
            .map(|(_, value)| value)
    }) else {
        return Ok(BrowserConfig::default());
    };
    let mapping = value
        .as_mapping()
        .ok_or_else(|| ConfigError::Browser("must be a mapping".into()))?;
    let mut config = BrowserConfig::default();
    for (key, value) in mapping {
        match key.as_str() {
            Some("enabled") => {
                config.enabled = value.as_bool().ok_or_else(|| {
                    ConfigError::Browser("enabled must be a boolean (true or false)".into())
                })?;
            }
            Some("mode") => {
                config.mode = match value.as_str() {
                    Some("interactive") => BrowserMode::Interactive,
                    Some("headless") => BrowserMode::Headless,
                    _ => {
                        return Err(ConfigError::Browser(
                            "mode must be interactive or headless".into(),
                        ));
                    }
                };
            }
            Some(key) => {
                return Err(ConfigError::Browser(format!(
                    "unknown key `{key}`; valid keys: `enabled`, `mode`"
                )));
            }
            None => return Err(ConfigError::Browser("keys must be strings".into())),
        }
    }
    Ok(config)
}

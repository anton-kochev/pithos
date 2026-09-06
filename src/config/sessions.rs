use super::ConfigError;
use saphyr::YamlOwned;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionStorage {
    #[default]
    Project,
    Volume,
}

/// Extract and validate the runtime-only session policy.
pub fn session_storage(doc: &YamlOwned) -> Result<SessionStorage, ConfigError> {
    let section = doc.as_mapping().and_then(|m| {
        m.iter()
            .find(|(k, _)| k.as_str() == Some("sessions"))
            .map(|(_, v)| v)
    });
    let Some(section) = section else {
        return Ok(SessionStorage::Project);
    };
    let mapping = section
        .as_mapping()
        .ok_or_else(|| ConfigError::Sessions("must be a mapping".into()))?;
    let mut storage = None;
    for (key, value) in mapping {
        if key.as_str() != Some("storage") {
            return Err(ConfigError::Sessions(
                "unknown key; valid key: storage".into(),
            ));
        }
        storage = Some(match value.as_str() {
            Some("project") => SessionStorage::Project,
            Some("volume") => SessionStorage::Volume,
            _ => {
                return Err(ConfigError::Sessions(
                    "storage must be project or volume (a string)".into(),
                ));
            }
        });
    }
    storage.ok_or_else(|| ConfigError::Sessions("missing required key storage".into()))
}

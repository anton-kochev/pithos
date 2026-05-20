use std::collections::BTreeMap;
use std::fmt::Write as _;

use saphyr::YamlOwned;

/// Emit the deterministic Pi-extension manifest for a validated `.pithos`
/// config.
///
/// The manifest is one line per declared extension, format `<name>\t<spec>`,
/// sorted alphabetically by name. When `pi.extensions` is absent or empty,
/// returns an empty string — callers should still write it so a stale file
/// on disk doesn't survive a config change.
///
/// **Precondition:** `yaml` must be the validated output of
/// [`crate::config::load`]. This function trusts validation has confirmed
/// every key/value is a well-formed string; it will panic otherwise.
pub fn manifest(yaml: &YamlOwned) -> String {
    let entries = sorted_entries(yaml);
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(entries.len() * 64);
    for (name, spec) in entries {
        writeln!(out, "{name}\t{spec}").unwrap();
    }
    out
}

/// Number of declared extensions in the validated config. Used for narration.
pub fn count(yaml: &YamlOwned) -> usize {
    sorted_entries(yaml).len()
}

fn sorted_entries(yaml: &YamlOwned) -> BTreeMap<String, String> {
    let mapping = yaml.as_mapping().expect("validated by config::load");
    let mut pi: Option<&YamlOwned> = None;
    for (key, value) in mapping {
        let name = key.as_str().expect("validated by config::load");
        if name == "pi" {
            pi = Some(value);
            break;
        }
    }
    let Some(pi) = pi else {
        return BTreeMap::new();
    };
    let pi_map = pi.as_mapping().expect("validated by config::load");
    let mut extensions: Option<&YamlOwned> = None;
    for (key, value) in pi_map {
        let name = key.as_str().expect("validated by config::load");
        if name == "extensions" {
            extensions = Some(value);
            break;
        }
    }
    let Some(extensions) = extensions else {
        return BTreeMap::new();
    };
    let map = extensions.as_mapping().expect("validated by config::load");
    map.iter()
        .map(|(k, v)| {
            let name = k.as_str().expect("validated by config::load");
            let spec = v.as_str().expect("validated by config::load");
            (name.to_string(), spec.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{count, manifest};
    use crate::config;

    #[test]
    fn manifest_empty_when_pi_absent() {
        let yaml = config::load(b"toolchains: {}\n").unwrap();
        assert_eq!(manifest(&yaml), "");
        assert_eq!(count(&yaml), 0);
    }

    #[test]
    fn manifest_empty_when_extensions_absent() {
        let yaml = config::load(b"toolchains: {}\npi:\n  version: \"0.75.3\"\n").unwrap();
        assert_eq!(manifest(&yaml), "");
        assert_eq!(count(&yaml), 0);
    }

    #[test]
    fn manifest_empty_when_extensions_empty() {
        let yaml = config::load(
            b"toolchains: {}\npi:\n  version: \"0.75.3\"\n  extensions: {}\n",
        )
        .unwrap();
        assert_eq!(manifest(&yaml), "");
        assert_eq!(count(&yaml), 0);
    }

    #[test]
    fn manifest_emits_single_entry() {
        let yaml = config::load(
            b"toolchains: {}\npi:\n  version: \"0.75.3\"\n  extensions:\n    pi-web-access: \"npm:0.10.7\"\n",
        )
        .unwrap();
        assert_eq!(manifest(&yaml), "pi-web-access\tnpm:0.10.7\n");
        assert_eq!(count(&yaml), 1);
    }

    #[test]
    fn manifest_sorts_alphabetically() {
        let yaml = config::load(
            b"toolchains: {}\npi:\n  version: \"0.75.3\"\n  extensions:\n    zeta: \"npm:1.0.0\"\n    alpha: \"git:https://example.com/a#v1\"\n    middle: \"npm:2.0.0\"\n",
        )
        .unwrap();
        assert_eq!(
            manifest(&yaml),
            "alpha\tgit:https://example.com/a#v1\nmiddle\tnpm:2.0.0\nzeta\tnpm:1.0.0\n"
        );
        assert_eq!(count(&yaml), 3);
    }
}

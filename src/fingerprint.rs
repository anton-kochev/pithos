use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Compute the SHA-256 fingerprint over (Dockerfile || .pithos || installers).
/// Returns a 64-char lowercase hex digest.
///
/// Installers are hashed in alphabetical order of `name` (BTreeMap iteration
/// is sort-by-key), matching the emitter's layer order (FR-303). Pi-config is
/// intentionally absent per FR-204 (bind-mounted at runtime, not baked).
///
/// No separator or length-prefix between blobs: input boundaries are
/// unambiguous because the Dockerfile is emitter-controlled and ends with
/// `\n`, `.pithos` is validated UTF-8 YAML, and installer bodies are
/// repo-controlled scripts. Length-extension attacks are out of scope
/// (inputs are not adversarial).
pub fn compute(dockerfile: &str, pithos: &[u8], installers: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile.as_bytes());
    hasher.update(pithos);
    for content in installers.values() {
        hasher.update(content);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        hex.push_str(&format!("{:02x}", byte));
    }
    hex
}

/// Docker label key under which the fingerprint is stored on built images
/// (FR-202). Used by `docker build --label LABEL_KEY=<hash>` and queried
/// via `docker inspect` to detect cache hits (FR-203).
pub const LABEL_KEY: &str = "dev.pithos.fingerprint";

/// Format a fingerprint hash as a `key=value` string ready to pass after
/// `--label` to `docker build` (FR-402). `hash` is expected to be `compute()`
/// output — this function does not validate; garbage in, garbage out.
pub fn label(hash: &str) -> String {
    format!("{LABEL_KEY}={hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Insertion order REVERSED from alphabetical: if someone swaps
    // BTreeMap for Vec<(String, Vec<u8>)>, the known-vector test fails.
    fn fixture_installers() -> BTreeMap<String, Vec<u8>> {
        let mut m = BTreeMap::new();
        m.insert("rust".to_string(), b"#!rust\n".to_vec());
        m.insert("dotnet".to_string(), b"#!dotnet\n".to_vec());
        m
    }

    #[test]
    fn compute_returns_known_sha256_for_fixed_input() {
        // Anti-drift: pre-computed via
        //   { printf 'FROM base\n'; printf 'toolchains: {}\n';
        //     printf '#!dotnet\n'; printf '#!rust\n'; } | sha256sum
        let out = compute("FROM base\n", b"toolchains: {}\n", &fixture_installers());
        assert_eq!(
            out,
            "470fd1b2c8f7daa92b6aa1adde442a3cbfef5b1c07432052b2fd66c6cbaad603"
        );
    }

    #[test]
    fn compute_is_deterministic_across_two_calls() {
        let a = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        let b = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        assert_eq!(a, b);
    }

    #[test]
    fn compute_changes_when_dockerfile_changes() {
        let a = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        let b = compute("FROM base2\n", b"x: 1\n", &fixture_installers());
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_pithos_changes() {
        let a = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        let b = compute("FROM base\n", b"x: 2\n", &fixture_installers());
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_installer_content_changes() {
        let mut tweaked = fixture_installers();
        tweaked.insert("rust".to_string(), b"#!rust-2\n".to_vec());
        let a = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        let b = compute("FROM base\n", b"x: 1\n", &tweaked);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_installer_set_changes() {
        let mut extra = fixture_installers();
        extra.insert("go".to_string(), b"#!go\n".to_vec());
        let a = compute("FROM base\n", b"x: 1\n", &fixture_installers());
        let b = compute("FROM base\n", b"x: 1\n", &extra);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_is_insensitive_to_installer_insertion_order() {
        // Behavioral proof of FR-201's "sorted deterministically" —
        // the property doesn't just rely on BTreeMap structure.
        let mut a_map = BTreeMap::new();
        a_map.insert("dotnet".to_string(), b"#!dotnet\n".to_vec());
        a_map.insert("rust".to_string(), b"#!rust\n".to_vec());
        let mut b_map = BTreeMap::new();
        b_map.insert("rust".to_string(), b"#!rust\n".to_vec());
        b_map.insert("dotnet".to_string(), b"#!dotnet\n".to_vec());
        let a = compute("FROM base\n", b"x: 1\n", &a_map);
        let b = compute("FROM base\n", b"x: 1\n", &b_map);
        assert_eq!(a, b);
    }

    #[test]
    fn label_formats_key_equals_hash() {
        assert_eq!(label("abc123"), "dev.pithos.fingerprint=abc123");
    }
}

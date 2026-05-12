use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Compute the SHA-256 fingerprint over
/// (Dockerfile || .pithos || installers || base_image_id).
/// Returns a 64-char lowercase hex digest.
///
/// Installers are hashed in alphabetical order of `name` (BTreeMap iteration
/// is sort-by-key), matching the emitter's layer order (FR-303). Pi-config is
/// intentionally absent per FR-204 (bind-mounted at runtime, not baked).
///
/// `base_image_id` is the resolved Image ID (`sha256:...`) of the local
/// `ghcr.io/anton-kochev/pithos:base` image, as emitted by
/// `docker inspect --format '{{.Id}}'`. Hashing it ties the per-project
/// cache key to the actual base layer, so any base change (local rebuild via
/// `pithos rebuild-base`, CI publish, `docker pull`) invalidates the
/// per-project cache and forces a rebuild. Without this input, edits to
/// `Dockerfile.base` / `entrypoint.sh` are invisible to per-project cache
/// hits.
///
/// No separator or length-prefix between blobs: input boundaries are
/// unambiguous because the Dockerfile is emitter-controlled and ends with
/// `\n`, `.pithos` is validated UTF-8 YAML, installer bodies are
/// repo-controlled scripts, and `base_image_id` is a fixed-shape docker
/// digest. Length-extension attacks are out of scope (inputs are not
/// adversarial).
pub fn compute(
    dockerfile: &str,
    pithos: &[u8],
    installers: &BTreeMap<String, Vec<u8>>,
    base_image_id: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile.as_bytes());
    hasher.update(pithos);
    for content in installers.values() {
        hasher.update(content);
    }
    hasher.update(base_image_id.as_bytes());
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

/// Docker label key under which the resolved exact version of a toolchain is
/// stored on built images. The launcher reads
/// `/opt/pithos-versions/<toolchain>` from a first-pass image and applies
/// `--label dev.pithos.<toolchain>-version=<resolved>` on the second pass,
/// so `docker inspect` is the audit source for what patch actually landed.
///
/// Co-located with [`LABEL_KEY`] and [`label`] so the `dev.pithos.*`
/// namespace lives in one place.
///
/// Note the asymmetry with [`compute`]: the fingerprint (installer-script
/// contents) is the identity / cache key, whereas this label is informational
/// metadata about what the installer happened to resolve to the day the
/// image was built. Two builds that share a fingerprint can legitimately
/// carry different version labels — e.g. `dotnet: "10"` resolving to a
/// newer patch on a later day — and [`crate::docker::find_image_by_fingerprint`]
/// will reuse whichever image is already tagged without re-resolving.
pub fn version_label_key(toolchain: &str) -> String {
    format!("dev.pithos.{toolchain}-version")
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

    // Fixture base image ID, kept stable across tests so changing it is a
    // deliberate, easy-to-spot diff. Mirrors the shape emitted by
    // `docker inspect --format '{{.Id}}'`.
    const FIXTURE_BASE: &str = "sha256:test";

    #[test]
    fn compute_returns_known_sha256_for_fixed_input() {
        // Anti-drift: pre-computed via
        //   { printf 'FROM base\n'; printf 'toolchains: {}\n';
        //     printf '#!dotnet\n'; printf '#!rust\n';
        //     printf 'sha256:test'; } | sha256sum
        // Note: no trailing \n on `sha256:test` — `compute` hashes the raw
        // bytes of `base_image_id`, and FIXTURE_BASE has no embedded newline.
        let out = compute(
            "FROM base\n",
            b"toolchains: {}\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        assert_eq!(
            out,
            "b0b96de5039c081a096df40a178ab4a5a85165f41d3d94a428c0fc7791612066"
        );
    }

    #[test]
    fn compute_is_deterministic_across_two_calls() {
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn compute_changes_when_dockerfile_changes() {
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute(
            "FROM base2\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_pithos_changes() {
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute(
            "FROM base\n",
            b"x: 2\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_installer_content_changes() {
        let mut tweaked = fixture_installers();
        tweaked.insert("rust".to_string(), b"#!rust-2\n".to_vec());
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute("FROM base\n", b"x: 1\n", &tweaked, FIXTURE_BASE);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_installer_set_changes() {
        let mut extra = fixture_installers();
        extra.insert("go".to_string(), b"#!go\n".to_vec());
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute("FROM base\n", b"x: 1\n", &extra, FIXTURE_BASE);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_changes_when_base_image_id_changes() {
        // The whole point of plumbing base_image_id through `compute`: a
        // changed base layer must invalidate every per-project cache key.
        let a = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            FIXTURE_BASE,
        );
        let b = compute(
            "FROM base\n",
            b"x: 1\n",
            &fixture_installers(),
            "sha256:other",
        );
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
        let a = compute("FROM base\n", b"x: 1\n", &a_map, FIXTURE_BASE);
        let b = compute("FROM base\n", b"x: 1\n", &b_map, FIXTURE_BASE);
        assert_eq!(a, b);
    }

    #[test]
    fn label_formats_key_equals_hash() {
        assert_eq!(label("abc123"), "dev.pithos.fingerprint=abc123");
    }

    #[test]
    fn version_label_key_formats_per_toolchain() {
        // Format lock — the `--version` suffix and `dev.pithos.` prefix are
        // externally observable via `docker inspect` and must not drift.
        assert_eq!(version_label_key("dotnet"), "dev.pithos.dotnet-version");
        assert_eq!(version_label_key("rust"), "dev.pithos.rust-version");
        assert_eq!(version_label_key("go"), "dev.pithos.go-version");
    }
}

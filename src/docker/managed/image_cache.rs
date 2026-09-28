//! Managed identity Pi cache lookup; no build or runtime authority.
use super::{ImageInfo, ImmutableImageId, ManagedDocker, PreflightError};
use crate::{
    docker::{BASE_IMAGE_REF, HostIdentity},
    dockerfile, embed,
};
use saphyr::YamlOwned;
use serde::{
    Deserialize, Deserializer,
    de::{Error as _, MapAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Label namespace is deliberately disjoint from legacy images.
pub const LABEL_KEY: &str = "io.pithos.broker.identity-fingerprint";
const DOMAIN: &[u8] = b"pithos-managed-pi-identity-cache-v1\0";
pub(super) const BASE: &str = r#"{"id":{{json .Id}}}"#;
const LIST: &str = "{{json .ID}}";
const INSPECT: &str = r#"{"id":{{json .Id}},"user":{{json (index .Config "User")}},"env":{{json (index .Config "Env")}},"volumes":{{json (index .Config "Volumes")}},"labels":{{json (index .Config "Labels")}}}"#;

const LAYERS: &str = r#"{"id":{{json .Id}},"layers":{{json .RootFS.Layers}}}"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BaseInfo {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layers {
    id: String,
    layers: Vec<String>,
}

fn layers(
    docker: &mut ManagedDocker,
    id: &ImmutableImageId,
) -> Result<Vec<String>, PreflightError> {
    let bytes = docker.query(&["image", "inspect", "--format", LAYERS, id.as_str()])?;
    let info: Layers =
        serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    if info.id != id.as_str() {
        return Err(PreflightError::Changed);
    }
    Ok(info.layers)
}

/// The build names the base by tag (BuildKit cannot build `FROM sha256:<id>`).
/// Prove the result really sits on the pinned base: its filesystem layers must
/// extend the base's exact layer chain.
pub(super) fn verify_layered_on(
    docker: &mut ManagedDocker,
    built: &ImmutableImageId,
    base: &ImmutableImageId,
) -> Result<(), PreflightError> {
    let base = layers(docker, base)?;
    let built = layers(docker, built)?;
    if base.is_empty() || built.len() <= base.len() || !built.starts_with(&base) {
        return Err(PreflightError::Changed);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    id: String,
    user: String,
    env: Vec<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    volumes: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(deserialize_with = "unique_labels")]
    labels: Option<BTreeMap<String, String>>,
}

// Deserialize the map directly from the JSON stream: an intermediate Value
// would already have discarded duplicate keys.
fn unique_labels<'de, D>(deserializer: D) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Labels;
    impl<'de> Visitor<'de> for Labels {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a label map without duplicate keys")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut labels = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if labels.insert(key, value).is_some() {
                    return Err(M::Error::custom("duplicate label key"));
                }
            }
            Ok(labels)
        }
    }

    struct OptionalLabels;
    impl<'de> Visitor<'de> for OptionalLabels {
        type Value = Option<BTreeMap<String, String>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a label map or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_map(Labels).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalLabels)
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Compute the v1 managed Pi cache key from the validated raw configuration
/// and the exact local base image ID. `yaml` must agree with the parsed raw
/// bytes; browser-enabled configurations are unsupported here. This is
/// distinct from the legacy image fingerprint.
pub fn fingerprint(
    yaml: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
    base: &ImmutableImageId,
) -> Result<String, PreflightError> {
    let parsed = validated_config(yaml, pithos)?;
    fingerprint_validated(&parsed, pithos, identity, base)
}

fn validated_config(yaml: &YamlOwned, pithos: &[u8]) -> Result<YamlOwned, PreflightError> {
    let parsed = crate::config::load(pithos).map_err(|_| PreflightError::InvalidInput)?;
    if crate::config::browser_config(&parsed)
        .map_err(|_| PreflightError::InvalidInput)?
        .enabled
    {
        return Err(PreflightError::Unsupported);
    }
    if &parsed != yaml {
        return Err(PreflightError::InvalidInput);
    }
    Ok(parsed)
}

fn fingerprint_validated(
    parsed: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
    base: &ImmutableImageId,
) -> Result<String, PreflightError> {
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    frame(
        &mut hash,
        dockerfile::emit_with_identity(parsed, identity).as_bytes(),
    );
    frame(&mut hash, pithos);
    for name in dockerfile::toolchain_names(parsed) {
        let bytes = embed::installer_bytes(&name).ok_or(PreflightError::InvalidInput)?;
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, bytes);
    }
    for bytes in [
        embed::PI_BUN_COMPAT_MJS,
        embed::ENTRYPOINT_SH,
        embed::IDENTITY_IMAGE_PY,
    ] {
        frame(&mut hash, bytes);
    }
    frame(&mut hash, base.as_str().as_bytes());
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub(super) fn resolve(
    docker: &mut ManagedDocker,
    yaml: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
) -> Result<Option<ImmutableImageId>, PreflightError> {
    // Validate raw input and agreement before any Docker query.
    Ok(resolve_with_base(docker, yaml, pithos, identity)?.0)
}

pub(super) fn resolve_with_base(
    docker: &mut ManagedDocker,
    yaml: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
) -> Result<(Option<ImmutableImageId>, ImmutableImageId), PreflightError> {
    let parsed = validated_config(yaml, pithos)?;
    let base = inspect_base(docker)?;
    let hash = fingerprint_validated(&parsed, pithos, identity, &base)?;
    let filter = format!("label={LABEL_KEY}={hash}");
    let bytes = docker.query(&[
        "image",
        "ls",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        LIST,
    ])?;
    let text = std::str::from_utf8(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    let mut ids = text.lines();
    let Some(first) = ids.next() else {
        return Ok((None, base));
    };
    let id: String = serde_json::from_str(first).map_err(|_| PreflightError::InvalidResponse)?;
    let id = ImmutableImageId::new(&id).map_err(|_| PreflightError::InvalidResponse)?;
    if ids.next().is_some() {
        return Err(PreflightError::InvalidResponse);
    }
    verify_candidate(docker, &id, identity, &hash)?;
    Ok((Some(id), base))
}

pub(super) fn inspect_base(docker: &mut ManagedDocker) -> Result<ImmutableImageId, PreflightError> {
    let bytes = docker.query(&["image", "inspect", "--format", BASE, BASE_IMAGE_REF])?;
    let base: BaseInfo =
        serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    ImmutableImageId::new(&base.id).map_err(|_| PreflightError::InvalidResponse)
}

pub(super) fn verify_candidate(
    docker: &mut ManagedDocker,
    id: &ImmutableImageId,
    identity: HostIdentity,
    hash: &str,
) -> Result<(), PreflightError> {
    let bytes = docker.query(&["image", "inspect", "--format", INSPECT, id.as_str()])?;
    let info: Candidate =
        serde_json::from_slice(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    if info.id != id.as_str() {
        return Err(PreflightError::Changed);
    }
    if info
        .labels
        .as_ref()
        .and_then(|labels| labels.get(LABEL_KEY))
        .map(String::as_str)
        != Some(hash)
        || info
            .volumes
            .as_ref()
            .is_some_and(|volumes| !volumes.is_empty())
        || !(ImageInfo {
            id: info.id,
            user: info.user,
            env: info.env,
        })
        .supported(identity)
    {
        return Err(PreflightError::Unsupported);
    }
    Ok(())
}

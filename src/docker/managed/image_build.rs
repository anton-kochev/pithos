//! Guarded host-owned build; never a launch token or daemon-side cancellation claim.
use super::{ImmutableImageId, ManagedDocker, PreflightError, image_cache};
use crate::{docker::HostIdentity, dockerfile, embed, lifecycle::Outcome};
use saphyr::YamlOwned;
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

fn trusted_directory(path: &Path, private: bool) -> Result<(), PreflightError> {
    let invalid = |_| PreflightError::InvalidSelection;
    // Canonical equality rejects lexical aliases and symlinks in any component.
    if !path.is_absolute() || fs::canonicalize(path).map_err(invalid)? != path {
        return Err(PreflightError::InvalidSelection);
    }
    // SAFETY: scalar process query with no pointers or failure sentinel.
    let uid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor).map_err(invalid)?;
        let sticky_root = ancestor != path && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        if !meta.is_dir()
            || ![0, uid].contains(&meta.uid())
            || (meta.mode() & 0o022 != 0 && !sticky_root)
            || (ancestor == path
                && (meta.uid() != uid || (private && meta.mode() & 0o777 != 0o700)))
        {
            return Err(PreflightError::InvalidSelection);
        }
    }
    Ok(())
}

fn private_root<'a>(root: &'a Path, workspace: &Path) -> Result<&'a Path, PreflightError> {
    trusted_directory(workspace, false)?;
    trusted_directory(root, true)?;
    if root.starts_with(workspace) || workspace.starts_with(root) {
        return Err(PreflightError::InvalidSelection);
    }
    Ok(root)
}

fn read_iid(path: &Path) -> Result<ImmutableImageId, PreflightError> {
    let invalid = |_| PreflightError::InvalidResponse;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(invalid)?;
    let before = file.metadata().map_err(invalid)?;
    // SAFETY: scalar process query with no pointers or failure sentinel.
    if !before.is_file()
        // The builder replaces the file under umask 022; the 0700 stage dir
        // keeps it private. Reject only writable-by-others.
        || before.mode() & 0o022 != 0
        || before.nlink() != 1
        || before.uid() != unsafe { libc::geteuid() }
        || before.len() > 72
    {
        return Err(PreflightError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    file.take(73).read_to_end(&mut bytes).map_err(invalid)?;
    let after = fs::symlink_metadata(path).map_err(invalid)?;
    if after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.nlink() != 1
        || after.mode() != before.mode()
        || bytes.len() > 72
    {
        return Err(PreflightError::InvalidResponse);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| PreflightError::InvalidResponse)?;
    ImmutableImageId::new(text.strip_suffix('\n').unwrap_or(text))
        .map_err(|_| PreflightError::InvalidResponse)
}

pub(super) fn ensure(
    docker: &mut ManagedDocker,
    yaml: &YamlOwned,
    pithos: &[u8],
    identity: HostIdentity,
    workspace: &Path,
    staging_root: &Path,
) -> Result<ImmutableImageId, PreflightError> {
    // Validate input, identity, and staging before ANY Docker query or filesystem write.
    let parsed = crate::config::load(pithos).map_err(|_| PreflightError::InvalidInput)?;
    let browser = crate::config::browser_config(&parsed)
        .map_err(|_| PreflightError::InvalidInput)?
        .enabled;
    if &parsed != yaml {
        return Err(PreflightError::InvalidInput);
    }
    if HostIdentity::effective().ok() != Some(identity) {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, workspace)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    let (cached, base) = image_cache::resolve_with_base(docker, yaml, pithos, identity)?;
    if let Some(id) = cached {
        return Ok(id);
    }
    let hash = image_cache::fingerprint(yaml, pithos, identity, &base)?;
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    let stage = Stage::new(root)?;
    embed::extract_with_identity_to(&stage.context).map_err(|_| PreflightError::Unavailable)?;
    // The emitted client layer copies these; its text names their fingerprint.
    if browser {
        crate::browser::assets::extract_to(&stage.context)
            .map_err(|_| PreflightError::Unavailable)?;
    }
    let emitted = dockerfile::emit_with_identity(&parsed, identity);
    // Name the base by tag: BuildKit cannot build `FROM sha256:<id>`. The pin
    // is enforced by the unchanged tag ID and the built image's layer chain.
    if !emitted.contains(&format!("FROM {} AS base", crate::docker::BASE_IMAGE_REF)) {
        return Err(PreflightError::Unsupported);
    }
    let dockerfile = stage.context.join("Dockerfile");
    fs::write(&dockerfile, emitted).map_err(|_| PreflightError::Unavailable)?;
    run_build(
        docker,
        stage,
        &dockerfile,
        &format!("{}={hash}", image_cache::LABEL_KEY),
        &format!("pithos-broker-identity:{hash}"),
        // Recheck the exact tag after staging, immediately before the CLI.
        |docker| {
            if image_cache::inspect_base(docker)? != base {
                return Err(PreflightError::Changed);
            }
            Ok(())
        },
        |docker, built| {
            image_cache::verify_candidate(docker, built, identity, &hash)?;
            image_cache::verify_layered_on(docker, built, &base)?;
            if image_cache::inspect_base(docker)? != base {
                return Err(PreflightError::Changed);
            }
            Ok(())
        },
    )
}

/// Resolve or build the identity Chromium sidecar image. Its Dockerfile pins
/// its own base by digest; the result is bound by label and the exact account.
pub(super) fn ensure_browser(
    docker: &mut ManagedDocker,
    identity: HostIdentity,
    workspace: &Path,
    staging_root: &Path,
) -> Result<ImmutableImageId, PreflightError> {
    if HostIdentity::effective().ok() != Some(identity) {
        return Err(PreflightError::InvalidInput);
    }
    let root = private_root(staging_root, workspace)?;
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    let hash = crate::browser::assets::fingerprint_with_identity(identity);
    if let Some(id) = image_cache::resolve_browser(docker, identity, &hash)? {
        return Ok(id);
    }
    if docker.has_child() {
        return Err(PreflightError::ChildPending);
    }
    let stage = Stage::new(root)?;
    crate::browser::assets::extract_with_identity_to(&stage.context, identity)
        .map_err(|_| PreflightError::Unavailable)?;
    let dockerfile = stage.context.join("browser/runtime/Dockerfile");
    run_build(
        docker,
        stage,
        &dockerfile,
        &format!("{}={hash}", image_cache::BROWSER_LABEL_KEY),
        &format!("pithos-broker-browser:{hash}"),
        |_| Ok(()),
        |docker, built| image_cache::verify_browser_candidate(docker, built, identity, &hash),
    )
}

/// A private 0700 stage. Only `context` is uploaded; buildx keeps its state
/// in `buildx`. The CLI gets `config`, a private copy of the frozen client
/// config: after a registry pull it writes (e.g. `.token_seed`) into it.
struct Stage {
    dir: tempfile::TempDir,
    context: PathBuf,
    buildx: PathBuf,
    config: PathBuf,
}
impl Stage {
    fn new(root: &Path) -> Result<Self, PreflightError> {
        let dir = tempfile::Builder::new()
            .prefix("pithos-image-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)
            .map_err(|_| PreflightError::Unavailable)?;
        let meta = fs::symlink_metadata(dir.path()).map_err(|_| PreflightError::Unavailable)?;
        // SAFETY: scalar process query with no pointers or failure sentinel.
        if !meta.is_dir()
            || meta.mode() & 0o777 != 0o700
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(PreflightError::InvalidSelection);
        }
        let context = dir.path().join("context");
        let buildx = dir.path().join("buildx");
        let config = dir.path().join("config");
        for path in [&context, &buildx, &config] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| PreflightError::Unavailable)?;
        }
        Ok(Self {
            dir,
            context,
            buildx,
            config,
        })
    }
}

/// One builder-neutral build through the frozen selection. `before` runs
/// right before the CLI starts; `verify` must prove the built ID before it is
/// returned. The stage is retained while the supervisor may own a child.
fn run_build(
    docker: &mut ManagedDocker,
    stage: Stage,
    dockerfile: &Path,
    label: &str,
    tag: &str,
    before: impl FnOnce(&mut ManagedDocker) -> Result<(), PreflightError>,
    verify: impl FnOnce(&mut ManagedDocker, &ImmutableImageId) -> Result<(), PreflightError>,
) -> Result<ImmutableImageId, PreflightError> {
    let iid = stage.context.join("image.iid");
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&iid)
        .map_err(|_| PreflightError::Unavailable)?;
    drop(file);
    if docker.work_shutdown.is_requested() {
        return Err(PreflightError::Unavailable);
    }
    before(docker)?;
    // Same content as the frozen config; check_selection proves it unchanged.
    match fs::read(docker.config.resolved.join("config.json")) {
        Ok(bytes) => fs::write(stage.config.join("config.json"), bytes)
            .map_err(|_| PreflightError::Unavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(PreflightError::Unavailable),
    }
    docker.check_selection()?;
    let mut command = Command::new(&docker.executable.resolved);
    command
        .env_clear()
        .env("BUILDX_CONFIG", &stage.buildx)
        // Plain image, no provenance/SBOM manifests, no CLI telemetry.
        .env("BUILDX_NO_DEFAULT_ATTESTATIONS", "1")
        .env("DOCKER_CLI_TELEMETRY_OPTOUT", "1")
        .current_dir(&stage.config)
        .arg("--host")
        .arg(&docker.endpoint)
        .arg("--config")
        .arg(&stage.config)
        // Builder-neutral flags only: without buildx, Docker falls back to the
        // legacy builder, which rejects BuildKit-only flags.
        .arg("build")
        .arg("--pull=false")
        .arg("-f")
        .arg(dockerfile)
        .arg("--label")
        .arg(label)
        // The containerd image store drops unnamed build results.
        .arg("--tag")
        .arg(tag)
        .arg("--iidfile")
        .arg(&iid)
        .arg(&stage.context);
    docker.build_stage = Some(stage.dir);
    let report = docker.build_supervisor.execute(&mut command);
    let result = (|| {
        let report = report.map_err(|_| PreflightError::Unavailable)?;
        docker.check_selection()?;
        if !matches!(report.outcome, Outcome::Exited(status) if status.success())
            || report.signal_error
            || report.wait_error
            || docker.work_shutdown.is_requested()
        {
            return Err(PreflightError::Unavailable);
        }
        let built = read_iid(&iid)?;
        verify(docker, &built)?;
        Ok(built)
    })();
    if !docker.build_supervisor.is_in_flight() {
        docker.build_stage = None;
    }
    result
}

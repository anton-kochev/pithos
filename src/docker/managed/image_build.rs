//! Guarded host-owned build; never a launch token or daemon-side cancellation claim.
use super::{ImmutableImageId, ManagedDocker, PreflightError, image_cache};
use crate::{docker::HostIdentity, dockerfile, embed, lifecycle::Outcome};
use saphyr::YamlOwned;
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
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
    if crate::config::browser_config(&parsed)
        .map_err(|_| PreflightError::InvalidInput)?
        .enabled
    {
        return Err(PreflightError::Unsupported);
    }
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
    let stage = tempfile::Builder::new()
        .prefix("pithos-image-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(root)
        .map_err(|_| PreflightError::Unavailable)?;
    let stage_meta = fs::symlink_metadata(stage.path()).map_err(|_| PreflightError::Unavailable)?;
    // SAFETY: scalar process query with no pointers or failure sentinel.
    if !stage_meta.is_dir()
        || stage_meta.mode() & 0o777 != 0o700
        || stage_meta.uid() != unsafe { libc::geteuid() }
    {
        return Err(PreflightError::InvalidSelection);
    }
    // Only `context` is uploaded. Buildx keeps its state in `buildx`, away from
    // the frozen client config directory.
    let context = &stage.path().join("context");
    let buildx_state = stage.path().join("buildx");
    for dir in [context, &buildx_state] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|_| PreflightError::Unavailable)?;
    }
    embed::extract_with_identity_to(context).map_err(|_| PreflightError::Unavailable)?;
    let emitted = dockerfile::emit_with_identity(&parsed, identity);
    // Name the base by tag: BuildKit cannot build `FROM sha256:<id>`. The pin
    // is enforced by the unchanged tag ID and the built image's layer chain.
    if !emitted.contains(&format!("FROM {} AS base", crate::docker::BASE_IMAGE_REF)) {
        return Err(PreflightError::Unsupported);
    }
    let dockerfile = context.join("Dockerfile");
    fs::write(&dockerfile, emitted).map_err(|_| PreflightError::Unavailable)?;
    let iid = context.join("image.iid");
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
    // Recheck the exact tag after staging, immediately before starting the CLI.
    if image_cache::inspect_base(docker)? != base {
        return Err(PreflightError::Changed);
    }
    docker.check_selection()?;
    let label = format!("{}={hash}", image_cache::LABEL_KEY);
    let mut command = Command::new(&docker.executable.resolved);
    command
        .env_clear()
        .env("BUILDX_CONFIG", &buildx_state)
        // Plain image, no provenance/SBOM manifests, no CLI telemetry.
        .env("BUILDX_NO_DEFAULT_ATTESTATIONS", "1")
        .env("DOCKER_CLI_TELEMETRY_OPTOUT", "1")
        .current_dir(&docker.config.resolved)
        .arg("--host")
        .arg(&docker.endpoint)
        .arg("--config")
        .arg(&docker.config.resolved)
        // Builder-neutral flags only: without buildx, Docker falls back to the
        // legacy builder, which rejects BuildKit-only flags.
        .arg("build")
        .arg("--pull=false")
        .arg("-f")
        .arg(&dockerfile)
        .arg("--label")
        .arg(&label)
        // The containerd image store drops unnamed build results.
        .arg("--tag")
        .arg(format!("pithos-broker-identity:{hash}"))
        .arg("--iidfile")
        .arg(&iid)
        .arg(context);
    // Retain stage while the supervisor may still own a child on any failure.
    docker.build_stage = Some(stage);
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
        image_cache::verify_candidate(docker, &built, identity, &hash)?;
        image_cache::verify_layered_on(docker, &built, &base)?;
        if image_cache::inspect_base(docker)? != base {
            return Err(PreflightError::Changed);
        }
        Ok(built)
    })();
    if !docker.build_supervisor.is_in_flight() {
        docker.build_stage = None;
    }
    result
}

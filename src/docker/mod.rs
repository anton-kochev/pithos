mod build;
mod daemon;
mod home_admission;
mod home_lease;
pub use home_lease::{HomeLease, LegacyHomeUse};
mod identity;
mod image;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod managed;
mod run;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod selection;
mod versions;

pub use build::{BuildError, BuildRequest, build, build_base, build_request};
pub use daemon::{ProbeError, classify_probe, probe_daemon};
pub use home_admission::{HomeInspectionError, inspection_args};
pub use identity::{HostIdentity, IdentityError, ImageRole, identity_overlay};
pub use image::{
    BASE_IMAGE_REF, ImageInfo, PithosImage, find_image_by_fingerprint, inspect_image,
    inspect_image_id, list_dangling_pithos_images, list_tagged_pithos_images, remove_image,
    tag_image,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use managed::image_cache as managed_image_cache;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use managed::probes::{PiInputs, ProbeError as OwnedProbeError};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use managed::{
    ImmutableImageId, ManagedDocker, PreflightChildState, PreflightError, ReadOnlyPreflight,
    VolumeName,
};
pub use run::{RunEnvironment, RunError, RunRequest, run, run_request, tmux_wrap};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use selection::{DockerSelection, HostDockerSnapshot};
pub use versions::{ExtractError, extract_versions};

#![cfg(any(target_os = "linux", target_os = "macos"))]

// The observation API is crate-private. Behavioral fake-Docker tests live in
// src/docker/managed/volume.rs, where they can exercise that boundary without
// making it public. This integration test protects the public input boundary.
use pithos::docker::{PreflightError, VolumeName};

#[test]
fn physical_names_are_exact_validated_arguments() {
    assert_eq!(VolumeName::new("pi-home").unwrap().as_str(), "pi-home");
    for invalid in ["", "x", "-bad", "bad:rw", "bad/name", "bad\nname"] {
        assert!(matches!(
            VolumeName::new(invalid),
            Err(PreflightError::InvalidInput)
        ));
    }
}

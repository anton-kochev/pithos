#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The run's one private Pi env file: sections concatenated, never adopted.
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::broker::pi_env::PiEnvFile;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

#[test]
fn env_file_is_private_concatenates_sections_and_is_removed_on_cleanup() {
    let run = tempfile::tempdir().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let sections = [
        "PITHOS_POSTGRES_HOST=pithos-postgres\nPITHOS_POSTGRES_PORT=5432\n",
        "GIT_CONFIG_COUNT=1\n",
    ];
    let mut file = PiEnvFile::create(run.path(), &sections).unwrap();
    assert_eq!(file.path(), run.path().join("pi.env"));
    assert_eq!(
        fs::read_to_string(file.path()).unwrap(),
        "PITHOS_POSTGRES_HOST=pithos-postgres\nPITHOS_POSTGRES_PORT=5432\n\
         GIT_CONFIG_COUNT=1\n"
    );
    let meta = fs::symlink_metadata(file.path()).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o777 == 0o600);
    // Never adopt existing state.
    assert!(PiEnvFile::create(run.path(), &sections).is_err());
    file.cleanup().unwrap();
    assert!(!file.path().exists());
    file.cleanup().unwrap();
}

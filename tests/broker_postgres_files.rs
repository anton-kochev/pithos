#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The run's private Postgres env file: a fresh password per run, never adopted.
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;

use pithos::broker::postgres::PostgresFiles;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

#[test]
fn env_file_is_private_with_a_fresh_password_and_removed_on_cleanup() {
    let run = tempfile::tempdir().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut files = PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").unwrap();
    assert_eq!(files.env_path(), run.path().join("postgres.env"));
    let password = files.password().to_owned();
    assert!(password.len() == 32 && password.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        fs::read_to_string(files.env_path()).unwrap(),
        format!(
            "POSTGRES_PASSWORD={password}\nPOSTGRES_DB=app\nPGDATA=/var/lib/postgresql/data/pithos\n"
        )
    );
    let meta = fs::symlink_metadata(files.env_path()).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o777 == 0o600);
    // Pi's copy: how to reach and log in to the run's database.
    assert_eq!(files.pi_env_path(), run.path().join("pi-postgres.env"));
    assert_eq!(
        fs::read_to_string(files.pi_env_path()).unwrap(),
        format!(
            "PITHOS_POSTGRES_HOST=pithos-postgres\nPITHOS_POSTGRES_PORT=5432\n\
             PITHOS_POSTGRES_USER=postgres\nPITHOS_POSTGRES_PASSWORD={password}\n\
             PITHOS_POSTGRES_DATABASE=app\n\
             PITHOS_POSTGRES_URL=postgresql://postgres:{password}@pithos-postgres:5432/app\n"
        )
    );
    let meta = fs::symlink_metadata(files.pi_env_path()).unwrap();
    assert!(meta.is_file() && meta.mode() & 0o777 == 0o600);
    // Never adopt existing state.
    assert!(PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").is_err());
    files.cleanup().unwrap();
    assert!(!files.env_path().exists() && !files.pi_env_path().exists());
    files.cleanup().unwrap();
    // Every run gets its own password.
    let other = PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").unwrap();
    assert_ne!(other.password(), password);
}

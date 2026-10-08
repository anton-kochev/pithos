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
    // Pi's section: how to reach and log in to the run's database. It goes
    // into the run's one Pi env file, never a file of its own.
    assert_eq!(
        files.pi_section(),
        format!(
            "PITHOS_POSTGRES_HOST=pithos-postgres\nPITHOS_POSTGRES_PORT=5432\n\
             PITHOS_POSTGRES_USER=postgres\nPITHOS_POSTGRES_PASSWORD={password}\n\
             PITHOS_POSTGRES_DATABASE=app\n\
             PITHOS_POSTGRES_URL=postgresql://postgres:{password}@pithos-postgres:5432/app\n"
        )
    );
    assert!(!run.path().join("pi-postgres.env").exists());
    // Never adopt existing state.
    assert!(PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").is_err());
    files.cleanup().unwrap();
    assert!(!files.env_path().exists());
    files.cleanup().unwrap();
    // Every run gets its own password.
    let other = PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").unwrap();
    assert_ne!(other.password(), password);
}

#[test]
fn project_env_section_names_the_runs_database() {
    let run = tempfile::tempdir().unwrap();
    fs::set_permissions(run.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let files = PostgresFiles::create(run.path(), "app", "/var/lib/postgresql/data").unwrap();
    let password = files.password().to_owned();
    let config = pithos::config::load(
        b"toolchains: {}\npostgres: {version: \"18.6\", database: app}\nenv:\n  \
          APP_DB: \"${postgres.url}\"\n  \
          ADMIN: \"Host=${postgres.host};Port=${postgres.port};Database=${postgres.database};Username=${postgres.user};Password=${postgres.password}\"\n",
    )
    .unwrap();
    let env = pithos::config::env_config(&config).unwrap().unwrap();
    assert_eq!(
        pithos::broker::pi_env::project_section(&env, Some(&files)).unwrap(),
        format!(
            "APP_DB=postgresql://postgres:{password}@pithos-postgres:5432/app\n\
             ADMIN=Host=pithos-postgres;Port=5432;Database=app;Username=postgres;Password={password}\n"
        )
    );
    // A placeholder with no database to fill it is refused, never left blank.
    assert!(pithos::broker::pi_env::project_section(&env, None).is_none());
    let literal = pithos::config::load(b"toolchains: {}\nenv: {A: x}\n").unwrap();
    let literal = pithos::config::env_config(&literal).unwrap().unwrap();
    assert_eq!(
        pithos::broker::pi_env::project_section(&literal, None).unwrap(),
        "A=x\n"
    );
}

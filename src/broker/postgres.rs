//! The run's private Postgres env file. Docker reads it on the host
//! (`--env-file`), so the password never appears in any argv.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    io,
    path::{Path, PathBuf},
};

/// The database's name on the run network, its port and superuser.
const HOST: &str = "pithos-postgres";
const PORT: u16 = 5432;
const USER: &str = "postgres";

#[derive(Debug)]
pub struct PostgresFiles {
    env: PathBuf,
    database: String,
    password: String,
}
impl PostgresFiles {
    /// Write fresh `<run directory>/postgres.env` for the server (0600, never
    /// adopted). Postgres keeps its data in `<data_root>/pithos`, a directory
    /// it creates itself and so owns, on a tmpfs it could not chown.
    pub fn create(run_directory: &Path, database: &str, data_root: &str) -> io::Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let password: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let content = format!(
            "POSTGRES_PASSWORD={password}\nPOSTGRES_DB={database}\nPGDATA={data_root}/pithos\n"
        );
        let env =
            super::extension::create_private(run_directory, "postgres.env", content.as_bytes())?;
        Ok(Self {
            env,
            database: database.to_owned(),
            password,
        })
    }
    pub fn env_path(&self) -> &Path {
        &self.env
    }
    pub fn password(&self) -> &str {
        &self.password
    }
    /// Pi's `KEY=value\n` lines: every process in Pi can reach and log in to
    /// the database.
    pub fn pi_section(&self) -> String {
        let Self {
            database, password, ..
        } = self;
        format!(
            "PITHOS_POSTGRES_HOST={HOST}\nPITHOS_POSTGRES_PORT={PORT}\n\
             PITHOS_POSTGRES_USER={USER}\nPITHOS_POSTGRES_PASSWORD={password}\n\
             PITHOS_POSTGRES_DATABASE={database}\n\
             PITHOS_POSTGRES_URL=postgresql://{USER}:{password}@{HOST}:{PORT}/{database}\n"
        )
    }
    /// One of the database's coordinates, for the project's `env` block.
    pub fn field(&self, field: crate::config::PostgresField) -> String {
        use crate::config::PostgresField;
        let Self {
            database, password, ..
        } = self;
        match field {
            PostgresField::Host => HOST.into(),
            PostgresField::Port => PORT.to_string(),
            PostgresField::User => USER.into(),
            PostgresField::Password => password.clone(),
            PostgresField::Database => database.clone(),
            PostgresField::Url => {
                format!("postgresql://{USER}:{password}@{HOST}:{PORT}/{database}")
            }
        }
    }
    /// Remove the file; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        super::extension::remove(&self.env)
    }
}

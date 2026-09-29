//! The run's private Postgres env files. Docker reads them on the host
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
    pi_env: PathBuf,
    password: String,
}
impl PostgresFiles {
    /// Write fresh `<run directory>/postgres.env` for the server and
    /// `pi-postgres.env` for Pi (0600, never adopted). Postgres keeps its
    /// data in `<data_root>/pithos`, a directory it creates itself and so
    /// owns, on a tmpfs it could not chown.
    pub fn create(run_directory: &Path, database: &str, data_root: &str) -> io::Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let password: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let content = format!(
            "POSTGRES_PASSWORD={password}\nPOSTGRES_DB={database}\nPGDATA={data_root}/pithos\n"
        );
        let env =
            super::extension::create_private(run_directory, "postgres.env", content.as_bytes())?;
        // Pi's copy: every process in Pi can reach and log in to the database.
        let pi_content = format!(
            "PITHOS_POSTGRES_HOST={HOST}\nPITHOS_POSTGRES_PORT={PORT}\n\
             PITHOS_POSTGRES_USER={USER}\nPITHOS_POSTGRES_PASSWORD={password}\n\
             PITHOS_POSTGRES_DATABASE={database}\n\
             PITHOS_POSTGRES_URL=postgresql://{USER}:{password}@{HOST}:{PORT}/{database}\n"
        );
        let pi_env = match super::extension::create_private(
            run_directory,
            "pi-postgres.env",
            pi_content.as_bytes(),
        ) {
            Ok(path) => path,
            Err(error) => {
                let _ = super::extension::remove(&env);
                return Err(error);
            }
        };
        Ok(Self {
            env,
            pi_env,
            password,
        })
    }
    pub fn env_path(&self) -> &Path {
        &self.env
    }
    pub fn pi_env_path(&self) -> &Path {
        &self.pi_env
    }
    pub fn password(&self) -> &str {
        &self.password
    }
    /// Remove both files; absent is success.
    pub fn cleanup(&mut self) -> io::Result<()> {
        super::extension::remove(&self.env)?;
        super::extension::remove(&self.pi_env)
    }
}

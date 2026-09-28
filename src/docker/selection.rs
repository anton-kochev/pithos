//! Read-only host Docker selection; never an activation or launch authority.
use super::{ManagedDocker, PreflightError};
use crate::lifecycle::Shutdown;
use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

fn candidate_exists(path: &Path) -> Result<bool, PreflightError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(PreflightError::InvalidSelection),
    }
}

/// Explicit, already captured host inputs; no process environment is read here.
#[derive(Default)]
pub struct HostDockerSnapshot {
    pub path: Option<OsString>,
    pub docker_host: Option<OsString>,
    pub docker_context: Option<OsString>,
    pub docker_config: Option<OsString>,
    pub home: Option<OsString>,
    pub broker_config: Option<PathBuf>,
}

/// Canonical paths only; downstream owners must revalidate before use.
#[derive(Debug, PartialEq, Eq)]
pub struct DockerSelection {
    pub executable: PathBuf,
    pub socket: PathBuf,
    pub config: PathBuf,
}

impl HostDockerSnapshot {
    /// Validate the host's explicit selections without executing Docker or writing state.
    pub fn discover(&self) -> Result<DockerSelection, PreflightError> {
        let invalid = PreflightError::InvalidSelection;
        // Even "default" is not resolved through an ambient Docker context.
        if self.docker_context.is_some() {
            return Err(invalid);
        }
        let executable = if let Some(path) = &self.path {
            // Relative and empty entries (e.g. the .NET SDK's literal
            // `~/.dotnet/tools`) are skipped, never resolved against cwd.
            let directories: Vec<_> = env::split_paths(path)
                .filter(|directory| directory.is_absolute())
                .collect();
            if directories.is_empty() {
                return Err(invalid);
            }
            let mut selected = None;
            for directory in directories {
                let candidate = directory.join("docker");
                // A dangling symlink is still an existing, unsafe first candidate.
                if candidate_exists(&candidate)? {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.ok_or(invalid)?
        } else {
            #[cfg(target_os = "linux")]
            const FALLBACK: &[&str] = &["/usr/local/bin/docker", "/usr/bin/docker"];
            #[cfg(target_os = "macos")]
            const FALLBACK: &[&str] = &[
                "/usr/local/bin/docker",
                "/opt/homebrew/bin/docker",
                "/usr/bin/docker",
            ];
            let mut selected = None;
            for path in FALLBACK {
                let candidate = PathBuf::from(path);
                if candidate_exists(&candidate)? {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.ok_or(invalid)?
        };
        let socket = if let Some(host) = &self.docker_host {
            let host = host.to_str().ok_or(invalid)?;
            PathBuf::from(host.strip_prefix("unix://").ok_or(invalid)?)
        } else {
            // Docker Desktop's own socket first: /var/run/docker.sock is only a
            // link to it, under root:daemon 0775 directories that fail trust.
            #[cfg(target_os = "macos")]
            let desktop = {
                let home = self.home.as_ref().ok_or(invalid)?;
                let home = PathBuf::from(home);
                if !home.is_absolute() {
                    return Err(invalid);
                }
                home.join(".docker/run/docker.sock")
            };
            #[cfg(target_os = "macos")]
            let preferred = candidate_exists(&desktop)?.then_some(desktop);
            #[cfg(target_os = "linux")]
            let preferred = None;
            match preferred {
                Some(socket) => socket,
                None => {
                    let standard = PathBuf::from("/var/run/docker.sock");
                    if !candidate_exists(&standard)? {
                        return Err(invalid);
                    }
                    standard
                }
            }
        };
        let config = match &self.docker_config {
            Some(explicit) => PathBuf::from(explicit),
            None => self.broker_config.clone().ok_or(invalid)?,
        };
        // The broker's own private config may carry exactly one generated entry:
        // the buildx plugin directory beside the selected Docker Desktop CLI.
        let generated = if self.docker_config.is_none() {
            let resolved = fs::canonicalize(&executable).map_err(|_| invalid)?;
            super::managed::desktop_plugin_dir(&resolved)
                .and_then(|dir| super::managed::desktop_plugin_config(&dir))
        } else {
            None
        };
        if let Some(bytes) = &generated {
            write_generated_config(&config, bytes).map_err(|_| invalid)?;
        }
        // Adapter construction is read-only. Coordinator ownership is separate.
        let endpoint = format!("unix://{}", socket.to_str().ok_or(invalid)?);
        let _validated = ManagedDocker::new(&executable, &endpoint, &config, Shutdown::new())?;
        if self.docker_config.is_none() {
            let mut entries = fs::read_dir(&config).map_err(|_| invalid)?;
            if entries.next().is_some()
                && generated.as_deref() != fs::read(config.join("config.json")).ok().as_deref()
            {
                return Err(invalid);
            }
        }
        Ok(DockerSelection {
            executable: fs::canonicalize(&executable).map_err(|_| invalid)?,
            socket: fs::canonicalize(&socket).map_err(|_| invalid)?,
            config: fs::canonicalize(&config).map_err(|_| invalid)?,
        })
    }
}

/// Create the private generated config once (0600, create-new). An existing
/// file is left untouched; validation then requires it to match exactly.
fn write_generated_config(directory: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("config.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    file.write_all(bytes)?;
    file.sync_all()
}

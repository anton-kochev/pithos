//! Host-owned web search. A run gets a revocable operation, never a provider key.
mod config;
mod provider;
mod server;

pub use config::WebConfigError;
pub(crate) use config::{WebConfig, guard_mounts, new_run_dir};
use server::SearchBroker;
use std::{ffi::OsString, io, path::Path};

pub(crate) const SOCKET_TARGET: &str = "/run/pithos-web-search.sock";
const STATUS: &str = "PITHOS_WEB_SEARCH_STATUS";
const TOKEN: &str = "PITHOS_WEB_SEARCH_TOKEN";
#[cfg(target_os = "linux")]
const SOCKET: &str = "PITHOS_WEB_SEARCH_SOCKET";
#[cfg(target_os = "macos")]
const PORT: &str = "PITHOS_WEB_SEARCH_PORT";
const OFFLINE: &str = "PI_OFFLINE";

pub(crate) fn offline_value(value: &str) -> bool {
    ["1", "true", "yes"]
        .iter()
        .any(|setting| value.trim().eq_ignore_ascii_case(setting))
}

pub(crate) fn host_offline() -> bool {
    std::env::var(OFFLINE).is_ok_and(|value| offline_value(&value))
}

#[derive(Default)]
pub(crate) enum WebSession {
    Ready(SearchBroker),
    #[default]
    Unconfigured,
    Unavailable,
    Offline,
}

impl WebSession {
    /// The operation bearer may enter Pi, but not the Docker CLI's argv.
    pub(crate) fn environment(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Ready(broker) => {
                let mut vars = vec![(STATUS, "ready".into()), (TOKEN, broker.token().into())];
                #[cfg(target_os = "linux")]
                vars.push((SOCKET, SOCKET_TARGET.into()));
                #[cfg(target_os = "macos")]
                vars.push((PORT, broker.port().to_string()));
                vars
            }
            Self::Unconfigured => vec![(STATUS, "unconfigured".into())],
            Self::Unavailable => vec![(STATUS, "unavailable".into())],
            Self::Offline => vec![(STATUS, "offline".into()), (OFFLINE, "1".into())],
        }
    }

    pub(crate) fn env_section(&self) -> String {
        self.environment()
            .into_iter()
            .map(|(name, value)| format!("{name}={value}\n"))
            .collect()
    }

    pub(crate) fn socket_source(&self) -> Option<&Path> {
        #[cfg(target_os = "linux")]
        if let Self::Ready(broker) = self {
            return Some(broker.socket_path());
        }
        None
    }
}

pub(crate) fn socket_mount(source: &Path) -> io::Result<OsString> {
    let mut mount = crate::sessions::bind_mount(source, SOCKET_TARGET)?;
    mount.push(",readonly");
    Ok(mount)
}

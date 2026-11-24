use super::provider::{SearchService, adapter};
use crate::docker::workspace::covers;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const WEB_DIR: &str = ".config/pithos/web";
const RUN_FILES: &str = ".pithos-run-files";
const CONTROL_DIRS: &[&str] = &[
    WEB_DIR,
    RUN_FILES,
    ".pithos-broker",
    ".pithos-browser-runs",
    ".pithos-home-leases",
    ".docker",
];
const PROVIDER: &str = "PITHOS_WEB_SEARCH_PROVIDER";
const MAX_CONFIG_BYTES: u64 = 16 * 1024;

/// These errors stop launch: skipping search would not hide a mounted file.
#[derive(Debug, thiserror::Error)]
pub enum WebConfigError {
    #[error("keep Pithos host control state outside every container bind mount")]
    HostStateMounted,
    #[error("cannot verify Pithos host control state")]
    HostStateUnavailable,
    #[error(
        "web search: keep the host .env and the installed pithos executable outside every container bind mount"
    )]
    Mounted,
    #[error(
        "web search: use a host-owned ~/.config/pithos/web directory (0700) and a regular .env file (0600, no links)"
    )]
    UnsafePath,
}

pub(crate) struct WebConfig(Setup);

enum Setup {
    Unconfigured,
    Unavailable,
    Configured {
        service: Box<SearchService>,
        socket_parent: PathBuf,
    },
}

impl WebConfig {
    pub(crate) fn load(
        mounts: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, WebConfigError> {
        Setup::load(mounts).map(Self)
    }

    pub(crate) fn start(self, offline: bool) -> super::WebSession {
        use super::{WebSession, server::SearchBroker};
        if offline {
            return WebSession::Offline;
        }
        match self.0 {
            Setup::Unconfigured => WebSession::Unconfigured,
            Setup::Unavailable => WebSession::Unavailable,
            Setup::Configured {
                service,
                socket_parent,
            } => match SearchBroker::start(service, &socket_parent) {
                Ok(broker) => WebSession::Ready(broker),
                Err(_) => WebSession::Unavailable,
            },
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Guard caller-supplied binds even when Web isn't configured. A concurrent
/// run must not be able to replace the host-owned source for a later bind.
pub(crate) fn guard_mounts(
    mounts: impl IntoIterator<Item = impl AsRef<Path>>,
) -> Result<(), WebConfigError> {
    let Some(home) = home() else { return Ok(()) };
    let rejected = |_| WebConfigError::HostStateUnavailable;
    let home = fs::canonicalize(home).map_err(rejected)?;
    for mount in mounts {
        let mount = fs::canonicalize(mount).map_err(rejected)?;
        for control in CONTROL_DIRS {
            let control = home.join(control);
            if paths_overlap(&mount, &control).map_err(rejected)? {
                return Err(WebConfigError::HostStateMounted);
            }
            if let Ok(resolved) = fs::canonicalize(&control) {
                if paths_overlap(&mount, &resolved).map_err(rejected)? {
                    return Err(WebConfigError::HostStateMounted);
                }
            }
        }
    }
    Ok(())
}

fn paths_overlap(mount: &Path, private: &Path) -> io::Result<bool> {
    if covers(mount, private)? || mount.starts_with(private) {
        return Ok(true);
    }
    match covers(private, mount) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        result => result,
    }
}

fn trusted_private_dir(directory: &Path) -> bool {
    // SAFETY: geteuid takes no pointers and cannot fail.
    let uid = unsafe { libc::geteuid() };
    directory.ancestors().all(|ancestor| {
        fs::symlink_metadata(ancestor).is_ok_and(|meta| {
            let sticky_root = ancestor != directory && meta.uid() == 0 && meta.mode() & 0o1000 != 0;
            meta.is_dir()
                && [0, uid].contains(&meta.uid())
                && (meta.mode() & 0o022 == 0 || sticky_root)
                && (ancestor != directory || meta.uid() == uid && meta.mode() & 0o777 == 0o700)
        })
    })
}

pub(crate) fn new_run_dir(workspace: &Path) -> io::Result<tempfile::TempDir> {
    guard_mounts([workspace]).map_err(io::Error::other)?;
    let home = home().ok_or_else(|| io::Error::other("need an absolute host HOME"))?;
    let root = home.canonicalize()?.join(RUN_FILES);
    match fs::DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !trusted_private_dir(&root) {
        return Err(io::Error::other(WebConfigError::HostStateUnavailable));
    }
    tempfile::Builder::new().prefix("run-").tempdir_in(root)
}

impl Setup {
    fn load(mounts: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<Self, WebConfigError> {
        let Some(home) = home() else {
            return Ok(Self::Unconfigured);
        };
        Self::load_from(&home, mounts)
    }

    fn load_from(
        home: &Path,
        mounts: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, WebConfigError> {
        let named_path = home.join(WEB_DIR).join(".env");
        match fs::symlink_metadata(&named_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Unconfigured),
            Err(_) => return Err(WebConfigError::UnsafePath),
            Ok(_) => {}
        }
        let path = fs::canonicalize(home)
            .map_err(|_| WebConfigError::UnsafePath)?
            .join(WEB_DIR)
            .join(".env");
        let directory = path.parent().expect("fixed config path has a parent");
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|_| WebConfigError::UnsafePath)?;
        let covers =
            |root: &Path, path: &Path| covers(root, path).map_err(|_| WebConfigError::UnsafePath);
        for source in mounts {
            let source = fs::canonicalize(source).map_err(|_| WebConfigError::UnsafePath)?;
            if covers(&source, &path)? || covers(&source, &exe)? || covers(directory, &source)? {
                return Err(WebConfigError::Mounted);
            }
        }

        // The host owns the directory, including the per-run socket names.
        if !trusted_private_dir(directory) {
            return Err(WebConfigError::UnsafePath);
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|_| WebConfigError::UnsafePath)?;
        let meta = file.metadata().map_err(|_| WebConfigError::UnsafePath)?;
        // SAFETY: geteuid takes no pointers and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o777 != 0o600 || meta.nlink() != 1
        {
            return Err(WebConfigError::UnsafePath);
        }
        if meta.len() > MAX_CONFIG_BYTES {
            return Ok(Self::Unavailable);
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_CONFIG_BYTES
        {
            return Ok(Self::Unavailable);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(Self::Unavailable);
        };
        Ok(Self::parse(text, directory.to_owned()))
    }

    fn parse(text: &str, socket_parent: PathBuf) -> Self {
        let mut fields = BTreeMap::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let Some((name, value)) = line.split_once('=') else {
                return Self::Unavailable;
            };
            let name = name.trim();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                || fields.insert(name, value.trim()).is_some()
            {
                return Self::Unavailable;
            }
        }
        let Some(id) = fields.get(PROVIDER).filter(|id| !id.is_empty()) else {
            return Self::Unconfigured;
        };
        let Some(adapter) = adapter(id) else {
            return Self::Unavailable;
        };
        let Some(key) = fields.get(adapter.key_name()).filter(|key| !key.is_empty()) else {
            return Self::Unconfigured;
        };
        if key.len() > 1024
            || !key
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'\'' | b'"' | b'`'))
        {
            return Self::Unavailable;
        }
        Self::Configured {
            service: Box::new(SearchService::new(adapter, (*key).to_owned())),
            socket_parent,
        }
    }
}

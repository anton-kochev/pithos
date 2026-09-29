//! Cooperative same-host-user home interlock, independent of Docker context.
//!
//! Root is the explicit host `HOME/.pithos-home-leases`, not a Docker volume.
//! SHA-256 of the actual volume name deliberately conflates equal names across
//! daemons: ambient context must never decide whether two callers share a lock.
//! Trusted, stable host parents are required; hostile same-UID/root replacement
//! of paths is outside this cooperative lock's authority. No stale repair occurs.

use std::{io, path::Path};

/// Exclusive broker use plus durable outstanding-use evidence.
/// Drop releases the OS lock but deliberately preserves the marker.
#[must_use = "hold through all home consumers; explicitly finish only after settlement"]
pub struct HomeLease(imp::Use);

impl HomeLease {
    /// Acquire once, before broker preflight, at an explicit private host root.
    /// Refuses live holders and *any* outstanding marker, including crash debt.
    /// The host must supply the same stable root used by its legacy invocations;
    /// neither HOME nor Docker environment variables are read by this API.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn broker(root: &Path, volume: &super::VolumeName) -> io::Result<Self> {
        imp::Use::acquire(root, volume.as_str(), true, None).map(|(lease, _)| Self(lease))
    }

    /// Like [`Self::broker`], but clears debt left by dead holders. Holding the
    /// exclusive flock proves no pithos process uses the home, so every marker
    /// is from a dead process; they are removed only when `mounted` positively
    /// reports that no container mounts the volume. Returns the cleared count.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn broker_recovering(
        root: &Path,
        volume: &super::VolumeName,
        mounted: impl FnOnce() -> io::Result<bool>,
    ) -> io::Result<(Self, usize)> {
        let mut mounted = Some(mounted);
        let mut check = || mounted.take().map_or(Ok(true), |check| check());
        imp::Use::acquire(root, volume.as_str(), true, Some(&mut check))
            .map(|(lease, cleared)| (Self(lease), cleared))
    }

    /// Remove only this holder's marker and release its lock.
    ///
    /// The caller must positively establish completion of **all** home consumers
    /// (normal engine acknowledgement/reconciled owned resources). Local child
    /// death, timeout, cancellation or an uncertain daemon outcome is not proof.
    /// Validation errors preserve evidence; Drop never calls this method. A
    /// directory-sync error after the completed use's unlink can leave that
    /// marker's deletion non-durable (it may reappear after a host crash).
    pub fn finish(self) -> io::Result<()> {
        self.0.finish()
    }
}

/// Shared legacy use; other legacy callers remain concurrent, including nesting.
/// Existing valid markers do not block legacy use and are never cleared by it.
#[must_use = "hold through the home consumer; errors and Drop retain crash evidence"]
pub struct LegacyHomeUse(imp::Use);

impl LegacyHomeUse {
    /// Discover legacy host HOME; never consult Docker context for lock identity.
    pub fn acquire_current(volume: &str) -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| io::Error::other("HOME is required for private home leases"))?;
        // A symlinked HOME ancestor must not lock users out of legacy runs.
        // The broker requires a canonical HOME, so both lanes share one root.
        let home = std::fs::canonicalize(home)?;
        Self::acquire(&home.join(".pithos-home-leases"), volume)
    }

    /// Acquire at an explicit host lease root (also useful for isolated callers).
    pub fn acquire(root: &Path, volume: &str) -> io::Result<Self> {
        imp::Use::acquire(root, volume, false, None).map(|(lease, _)| Self(lease))
    }

    /// Explicit positive completion; removes only this holder's marker.
    /// Helpers require successful CLI return. Interactive callers must exclude
    /// Docker errors 125–127, signals and unknown outcomes. This legacy client
    /// contract cannot prove every external daemon outcome; errors retain debt.
    pub fn finish(self) -> io::Result<()> {
        self.0.finish()
    }
}

#[cfg(unix)]
mod imp {
    use fs2::FileExt;
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File, Metadata, OpenOptions},
        io::{self, Write},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        path::{Component, Path, PathBuf},
    };

    const MARKER: &[u8] = b"pithos-home-use-v1\noutstanding\n";

    fn invalid() -> io::Error {
        io::Error::other("unsafe home lease state; explicit host recovery required")
    }
    fn uid() -> u32 {
        // SAFETY: geteuid has no pointer or lifetime preconditions.
        unsafe { libc::geteuid() }
    }
    fn same(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    fn private(meta: &Metadata, directory: bool) -> io::Result<()> {
        if meta.uid() != uid()
            || meta.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
            || if directory {
                !meta.is_dir()
            } else {
                !meta.is_file() || meta.nlink() != 1
            }
        {
            return Err(invalid());
        }
        Ok(())
    }
    fn open_file(path: &Path, create: bool) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        check_file(path, &file)?;
        Ok(file)
    }
    fn check_file(path: &Path, file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        private(&metadata, false)?;
        let binding = fs::symlink_metadata(path)?;
        private(&binding, false)?;
        if !same(&metadata, &binding) {
            return Err(invalid());
        }
        Ok(())
    }

    /// Validate markers; `refuse` rejects any. Returns the valid marker paths.
    fn check_markers(path: &Path, refuse: bool) -> io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if refuse {
                return Err(io::Error::other(
                    "outstanding home use; explicit host recovery required",
                ));
            }
            // A concurrent shared holder may finish its own marker. It may also
            // still be writing, so never infer completion from content/length.
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            private(&meta, false)?;
            let name = entry.file_name();
            if !name.to_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            }) {
                return Err(invalid());
            }
            found.push(entry.path());
        }
        Ok(found)
    }

    struct Directory {
        path: PathBuf,
        file: File,
    }
    impl Directory {
        fn open(path: &Path) -> io::Result<Self> {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            private(&fs::symlink_metadata(path)?, true)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            let directory = Self {
                path: path.into(),
                file,
            };
            directory.check()?;
            Ok(directory)
        }
        fn check(&self) -> io::Result<()> {
            let metadata = self.file.metadata()?;
            let binding = fs::symlink_metadata(&self.path)?;
            private(&metadata, true)?;
            private(&binding, true)?;
            if !same(&metadata, &binding) {
                return Err(invalid());
            }
            Ok(())
        }
    }

    pub(super) struct Use {
        root: Directory,
        key: Directory,
        uses: Directory,
        lock: File,
        marker: File,
        marker_path: PathBuf,
    }
    impl Use {
        /// `recover` (exclusive only) answers whether a container still mounts
        /// the volume; it is asked only when dead holders left markers.
        pub(super) fn acquire(
            root: &Path,
            volume: &str,
            exclusive: bool,
            mut recover: Option<&mut dyn FnMut() -> io::Result<bool>>,
        ) -> io::Result<(Self, usize)> {
            if !(2..=255).contains(&volume.len())
                || !volume.as_bytes()[0].is_ascii_alphanumeric()
                || !volume
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                || !root.is_absolute()
            {
                return Err(invalid());
            }
            // Refuse links in existing ancestors. Parents are never created or
            // chmodded; only the private root and its own children may be made.
            let parent = root.parent().ok_or_else(invalid)?;
            let mut ancestor = PathBuf::new();
            for component in parent.components() {
                if !matches!(component, Component::RootDir | Component::Normal(_)) {
                    return Err(invalid());
                }
                ancestor.push(component);
                if !fs::symlink_metadata(&ancestor)?.is_dir() {
                    return Err(invalid());
                }
            }
            let parent_meta = fs::symlink_metadata(parent)?;
            if parent_meta.uid() != uid() || parent_meta.mode() & 0o022 != 0 {
                return Err(invalid());
            }
            let root = Directory::open(root)?;
            let hash: String = Sha256::digest(volume.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let key = Directory::open(&root.path.join(hash))?;
            // Check existing state before adding anything inside this key.
            for entry in fs::read_dir(&key.path)? {
                let entry = entry?;
                let name = entry.file_name();
                if name == "lease" {
                    let meta = fs::symlink_metadata(entry.path())?;
                    private(&meta, false)?;
                    if meta.len() != 0 {
                        return Err(invalid());
                    }
                } else if name == "uses" {
                    private(&fs::symlink_metadata(entry.path())?, true)?;
                    check_markers(&entry.path(), exclusive && recover.is_none())?;
                } else {
                    return Err(invalid());
                }
            }
            let uses = Directory::open(&key.path.join("uses"))?;
            let lock_path = key.path.join("lease");
            let lock = match open_file(&lock_path, true) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open_file(&lock_path, false)?,
                Err(e) => return Err(e),
            };
            if lock.metadata()?.len() != 0 {
                return Err(invalid());
            }
            if exclusive {
                FileExt::try_lock_exclusive(&lock)?;
            } else {
                FileExt::try_lock_shared(&lock)?;
            }
            // Under the exclusive flock no live pithos holds this home, so any
            // marker is a dead holder's. Clear only on a positive "not mounted".
            let mut cleared = 0;
            match recover.as_mut().filter(|_| exclusive) {
                None => {
                    check_markers(&uses.path, exclusive)?;
                }
                Some(mounted) => {
                    let stale = check_markers(&uses.path, false)?;
                    if !stale.is_empty() {
                        if mounted()? {
                            return Err(io::Error::new(
                                io::ErrorKind::ResourceBusy,
                                "home volume is still mounted by a container",
                            ));
                        }
                        for marker in &stale {
                            private(&fs::symlink_metadata(marker)?, false)?;
                            fs::remove_file(marker)?;
                        }
                        uses.file.sync_all()?;
                        cleared = stale.len();
                    }
                }
            }
            let mut random = [0_u8; 32];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let marker_path = uses.path.join(name);
            let mut marker = open_file(&marker_path, true)?;
            marker.write_all(MARKER)?;
            marker.sync_all()?;
            lock.sync_all()?;
            uses.file.sync_all()?;
            key.file.sync_all()?;
            root.file.sync_all()?;
            File::open(parent)?.sync_all()?;
            let value = Self {
                root,
                key,
                uses,
                lock,
                marker,
                marker_path,
            };
            value.check()?;
            Ok((value, cleared))
        }

        fn check(&self) -> io::Result<()> {
            self.root.check()?;
            self.key.check()?;
            self.uses.check()?;
            check_file(&self.key.path.join("lease"), &self.lock)?;
            check_file(&self.marker_path, &self.marker)
        }

        pub(super) fn finish(self) -> io::Result<()> {
            self.check()?;
            fs::remove_file(&self.marker_path)?;
            self.uses.file.sync_all()
            // Files close here: only after positive completion and durable unlink.
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::{io, path::Path};
    pub(super) struct Use;
    impl Use {
        pub(super) fn acquire(
            _: &Path,
            _: &str,
            _: bool,
            _: Option<&mut dyn FnMut() -> io::Result<bool>>,
        ) -> io::Result<(Self, usize)> {
            Err(io::Error::other("private home leases require Unix"))
        }
        pub(super) fn finish(self) -> io::Result<()> {
            Err(io::Error::other("private home leases require Unix"))
        }
    }
}

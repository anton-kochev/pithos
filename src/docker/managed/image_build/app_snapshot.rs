//! Broker app inputs are mutable. Look them up through pinned directories and
//! hand Docker only the copies in the host-private stage.

use super::{AppBuild, PreflightError, Stage};
use crate::lifecycle::Shutdown;
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File},
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        },
    },
    path::{Component, Path, PathBuf},
    ptr::NonNull,
};

const READ_FLAGS: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
const DIR_FLAGS: libc::c_int = READ_FLAGS | libc::O_DIRECTORY;

const INVALID: PreflightError = PreflightError::InvalidInput;
const UNAVAILABLE: PreflightError = PreflightError::Unavailable;
const LIMIT: PreflightError = PreflightError::InvalidLimits;

const MAX_BYTES: u64 = 1 << 30;
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;
const MAX_CONTROL_BYTES: u64 = 1 << 20;

pub(super) fn stage(
    inputs: &AppBuild<'_>,
    stage: &Stage,
    shutdown: &Shutdown,
) -> Result<PathBuf, PreflightError> {
    let dockerfile = relative_path(inputs.dockerfile, false)?;
    let context = relative_path(inputs.context, true)?;
    // private_root has already checked the canonical workspace and its trusted
    // ancestors. Pin it without following aliases even while opening it.
    let root = Path::new("/");
    let workspace_path = inputs
        .workspace
        .strip_prefix(root)
        .map_err(|_| PreflightError::InvalidSelection)?;
    let workspace = open_dir(root)
        .and_then(|dir| walk(dir, workspace_path))
        .map_err(|_| PreflightError::InvalidSelection)?;
    let source =
        walk(workspace.try_clone().map_err(|_| UNAVAILABLE)?, context).map_err(|_| INVALID)?;
    let root_ignore = control_at(source.as_raw_fd(), c".dockerignore")?.is_some();
    let mut budget = Budget {
        bytes: MAX_BYTES,
        entries: MAX_ENTRIES,
        shutdown,
    };
    let (staged, recipe_ignore) = match dockerfile.strip_prefix(context) {
        Ok(relative) => {
            let recipe = Recipe::find(source.try_clone().map_err(|_| UNAVAILABLE)?, relative)?;
            (stage.context.join(relative), recipe.ignore_status.is_some())
        }
        Err(_) => {
            let recipe = Recipe::find(workspace, dockerfile)?;
            let staged = recipe.copy_to(&stage.dir.path().join("recipe"), &mut budget)?;
            (staged, recipe.ignore_status.is_some())
        }
    };
    copy_tree(source, &stage.context, &mut budget, 0)?;

    // Docker opens these paths rather than just archiving entries. Check the
    // actual staged lookups, even if the stage and source fold case differently.
    let context = open_dir(&stage.context).map_err(|_| UNAVAILABLE)?;
    let root = open_dir(stage.dir.path()).map_err(|_| UNAVAILABLE)?;
    let relative = staged.strip_prefix(stage.dir.path()).map_err(|_| INVALID)?;
    let recipe = Recipe::find(root, relative)?;
    if root_ignore != control_at(context.as_raw_fd(), c".dockerignore")?.is_some()
        || recipe_ignore != recipe.ignore_status.is_some()
    {
        return Err(INVALID);
    }
    Ok(staged)
}

fn relative_path(relative: &str, directory: bool) -> Result<&Path, PreflightError> {
    if directory && relative == "." {
        return Ok(Path::new(""));
    }
    if relative.len() > 1024
        || relative.chars().any(char::is_control)
        || relative
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(INVALID);
    }
    Ok(Path::new(relative))
}

struct Recipe {
    parent: File,
    name: CString,
    status: libc::stat,
    ignore: CString,
    ignore_status: Option<libc::stat>,
}

impl Recipe {
    fn find(root: File, path: &Path) -> Result<Self, PreflightError> {
        let parent = walk(root, path.parent().ok_or(INVALID)?).map_err(|_| INVALID)?;
        let name = path.file_name().ok_or(INVALID)?;
        let mut ignore = name.to_os_string();
        ignore.push(".dockerignore");
        let name = CString::new(name.as_bytes()).map_err(|_| INVALID)?;
        let ignore = CString::new(ignore.as_os_str().as_bytes()).map_err(|_| INVALID)?;
        let status = control_at(parent.as_raw_fd(), &name)?.ok_or(INVALID)?;
        let ignore_status = control_at(parent.as_raw_fd(), &ignore)?;
        Ok(Self {
            parent,
            name,
            status,
            ignore,
            ignore_status,
        })
    }

    /// Copy only the external Dockerfile and its companion, not other siblings.
    fn copy_to(
        &self,
        destination: &Path,
        budget: &mut Budget<'_>,
    ) -> Result<PathBuf, PreflightError> {
        create_directory(destination)?;
        let fd = self.parent.as_raw_fd();
        let staged = destination.join(OsStr::from_bytes(self.name.to_bytes()));
        budget.entry()?;
        copy_file(fd, &self.name, &self.status, &staged, budget)?;
        if let Some(status) = &self.ignore_status {
            let ignore = destination.join(OsStr::from_bytes(self.ignore.to_bytes()));
            budget.entry()?;
            copy_file(fd, &self.ignore, status, &ignore, budget)?;
        }
        Ok(staged)
    }
}

fn control_at(dir: RawFd, name: &CStr) -> Result<Option<libc::stat>, PreflightError> {
    match stat_at(dir, name) {
        Ok(status) if standalone_regular(&status) => {
            if u64::try_from(status.st_size).map_err(|_| INVALID)? > MAX_CONTROL_BYTES {
                return Err(LIMIT);
            }
            Ok(Some(status))
        }
        // An appended .dockerignore suffix may not fit in a filesystem name.
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENAMETOOLONG)
            ) =>
        {
            Ok(None)
        }
        _ => Err(INVALID),
    }
}

fn standalone_regular(status: &libc::stat) -> bool {
    status.st_mode & libc::S_IFMT == libc::S_IFREG && status.st_nlink == 1
}

struct Budget<'a> {
    bytes: u64,
    entries: usize,
    shutdown: &'a Shutdown,
}

impl Budget<'_> {
    fn check_shutdown(&self) -> Result<(), PreflightError> {
        if self.shutdown.is_requested() {
            return Err(UNAVAILABLE);
        }
        Ok(())
    }

    fn entry(&mut self) -> Result<(), PreflightError> {
        self.entries = self.entries.checked_sub(1).ok_or(LIMIT)?;
        Ok(())
    }

    fn payload(&mut self, count: usize) -> Result<(), PreflightError> {
        self.bytes = self.bytes.checked_sub(count as u64).ok_or(LIMIT)?;
        Ok(())
    }
}

fn create_directory(path: &Path) -> Result<(), PreflightError> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| UNAVAILABLE)
}

fn copy_tree(
    source: File,
    destination: &Path,
    budget: &mut Budget<'_>,
    depth: usize,
) -> Result<(), PreflightError> {
    if depth > MAX_DEPTH {
        return Err(LIMIT);
    }
    let mode = source.metadata().map_err(|_| INVALID)?.mode();
    let mut entries = Entries::new(source).map_err(|_| INVALID)?;
    let fd = entries.fd();
    loop {
        budget.check_shutdown()?;
        let Some(name) = entries.next().map_err(|_| INVALID)? else {
            break;
        };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        budget.entry()?;
        // readdir returns single filename components, not paths.
        let path = destination.join(OsStr::from_bytes(name.to_bytes()));
        let status = stat_at(fd, name).map_err(|_| INVALID)?;
        match status.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let child = open_at(fd, name, true).map_err(|_| INVALID)?;
                create_directory(&path)?;
                copy_tree(child, &path, budget, depth + 1)?;
            }
            libc::S_IFREG => copy_file(fd, name, &status, &path, budget)?,
            libc::S_IFLNK => copy_link(fd, name, &path, budget)?,
            _ => return Err(INVALID),
        }
    }
    // Keep staged directories traversable and removable by the host owner.
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(mode & 0o777 | 0o700),
    )
    .map_err(|_| UNAVAILABLE)
}

fn copy_file(
    fd: RawFd,
    name: &CStr,
    before: &libc::stat,
    path: &Path,
    budget: &mut Budget<'_>,
) -> Result<(), PreflightError> {
    // A multiply linked inode can have names outside the workspace. Check
    // before opening, then verify the type, identity and link count on the fd
    // before reading. O_NONBLOCK avoids waiting if the entry became a FIFO.
    if !standalone_regular(before) {
        return Err(INVALID);
    }
    budget.check_shutdown()?;
    let mut source = open_at(fd, name, false).map_err(|_| INVALID)?;
    // SAFETY: fstat initializes the result on success and source owns this fd.
    let opened =
        unsafe { read_stat(|out| libc::fstat(source.as_raw_fd(), out)) }.map_err(|_| INVALID)?;
    if !standalone_regular(&opened)
        || opened.st_dev != before.st_dev
        || opened.st_ino != before.st_ino
    {
        return Err(INVALID);
    }
    if u64::try_from(opened.st_size).map_err(|_| INVALID)? > budget.bytes {
        return Err(LIMIT);
    }
    let mut dest = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| UNAVAILABLE)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        budget.check_shutdown()?;
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(INVALID),
        };
        // st_size alone does not bound reads from concurrently edited files.
        budget.payload(count)?;
        dest.write_all(&buffer[..count]).map_err(|_| UNAVAILABLE)?;
    }
    #[allow(clippy::unnecessary_cast)] // mode_t is narrower on macOS.
    let mode = opened.st_mode as u32;
    dest.set_permissions(fs::Permissions::from_mode(mode & 0o777 | 0o400))
        .map_err(|_| UNAVAILABLE)
}

fn copy_link(
    fd: RawFd,
    name: &CStr,
    path: &Path,
    budget: &mut Budget<'_>,
) -> Result<(), PreflightError> {
    let mut target = [0; libc::PATH_MAX as usize + 1];
    // SAFETY: name is NUL-terminated and target is writable for its length.
    let length =
        unsafe { libc::readlinkat(fd, name.as_ptr(), target.as_mut_ptr().cast(), target.len()) };
    if length < 0 || length as usize >= target.len() {
        return Err(INVALID);
    }
    let literal = Path::new(OsStr::from_bytes(&target[..length as usize]));
    // No absolute or parent components, even in an otherwise in-tree link.
    // This keeps chains of copied links in the tree without resolving them.
    if literal
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(INVALID);
    }
    budget.payload(length as usize)?;
    symlink(literal, path).map_err(|_| UNAVAILABLE)
}

fn open_dir(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(DIR_FLAGS)
        .open(path)
}

fn walk(mut dir: File, path: &Path) -> io::Result<File> {
    for part in path.components() {
        let Component::Normal(part) = part else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let name = CString::new(part.as_bytes())?;
        dir = open_at(dir.as_raw_fd(), &name, true)?;
    }
    Ok(dir)
}

fn open_at(dir: RawFd, name: &CStr, directory: bool) -> io::Result<File> {
    let flags = if directory { DIR_FLAGS } else { READ_FLAGS };
    // SAFETY: name is NUL-terminated; these read-only flags take no mode argument.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), libc::O_RDONLY | flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    // SAFETY: name is NUL-terminated and fstatat initializes the result on success.
    unsafe { read_stat(|out| libc::fstatat(dir, name.as_ptr(), out, libc::AT_SYMLINK_NOFOLLOW)) }
}

// SAFETY: stat must initialize the result if it returns zero.
unsafe fn read_stat(stat: impl FnOnce(*mut libc::stat) -> libc::c_int) -> io::Result<libc::stat> {
    let mut status = MaybeUninit::uninit();
    if stat(status.as_mut_ptr()) != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: callers use fstat or fstatat, which initialize status on success.
    Ok(unsafe { status.assume_init() })
}

struct Entries(NonNull<libc::DIR>);

impl Entries {
    fn new(file: File) -> io::Result<Self> {
        // SAFETY: file owns a live directory fd. Transfer ownership on success only.
        let ptr = unsafe { libc::fdopendir(file.as_raw_fd()) };
        let ptr = NonNull::new(ptr).ok_or_else(io::Error::last_os_error)?;
        let _ = file.into_raw_fd();
        Ok(Self(ptr))
    }

    fn fd(&self) -> RawFd {
        // SAFETY: self owns a live directory stream.
        unsafe { libc::dirfd(self.0.as_ptr()) }
    }

    fn next(&mut self) -> io::Result<Option<&CStr>> {
        // readdir uses NULL for both EOF and errors; clear thread-local errno.
        // SAFETY: self exclusively owns the stream. The returned name borrows it
        // until the next call and readdir guarantees NUL termination.
        unsafe {
            #[cfg(target_os = "linux")]
            let errno = libc::__errno_location();
            #[cfg(target_os = "macos")]
            let errno = libc::__error();
            *errno = 0;
            let entry = libc::readdir(self.0.as_ptr());
            if entry.is_null() {
                return if *errno == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::last_os_error())
                };
            }
            Ok(Some(CStr::from_ptr((*entry).d_name.as_ptr())))
        }
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: this closes the owned stream and its directory fd once.
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

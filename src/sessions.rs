//! Project-local transcript storage. Credentials remain in the home volume.
use std::{
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

const IGNORE: &str = "*\n!.gitignore\n";

fn directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(io::Error::other(format!(
            "{} must be a directory, not a symlink or file",
            path.display()
        )));
    }
    Ok(())
}

pub fn prepare(workspace: &Path) -> io::Result<PathBuf> {
    let root = workspace.join(".pi/sessions");
    let result = (|| {
        directory(&workspace.join(".pi"))?;
        directory(&root)?;
        let ignore = root.join(".gitignore");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&ignore)
        {
            Ok(mut file) => file.write_all(IGNORE.as_bytes())?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&ignore)?.file_type().is_file() {
                    return Err(io::Error::other(
                        "session .gitignore must be a regular file",
                    ));
                }
                if fs::read_to_string(&ignore)? != IGNORE {
                    return Err(io::Error::other(
                        "custom session .gitignore preserved; use '*\\n!.gitignore\\n' or sessions.storage: volume",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
        // Check host writability without modifying existing files or permissions.
        tempfile::NamedTempFile::new_in(&root)?;
        Ok(())
    })();
    result.map_err(|e: io::Error| io::Error::new(e.kind(), format!("sessions at {}: {e}; container UID 501:20 also needs access; no permission relaxation or storage fallback is performed", root.display())))?;
    Ok(root)
}

/// Docker --mount CSV quotes permit spaces, colons, commas and quotes in paths.
pub fn bind_mount(root: &Path, target: &str) -> io::Result<OsString> {
    let path = root
        .to_str()
        .ok_or_else(|| io::Error::other("session mount path must be UTF-8"))?;
    Ok(format!(
        "type=bind,\"source={}\",target={target}",
        path.replace('"', "\"\"")
    )
    .into())
}

fn docker_output(args: &[&str]) -> io::Result<String> {
    let out = Command::new("docker").args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "docker {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn migrate(workspace: &Path, merge: bool) -> io::Result<()> {
    let project = crate::project::name_from_path(workspace)
        .ok_or_else(|| io::Error::other("cannot derive project name"))?;
    let volume = format!("pithos-home-{project}");
    let image = format!("pithos:{project}");
    docker_output(&["volume", "inspect", &volume])?;
    docker_output(&["image", "inspect", &image])?;
    let filter = format!("volume={volume}");
    if !docker_output(&["ps", "-q", "--filter", &filter])?
        .trim()
        .is_empty()
    {
        return Err(io::Error::other(
            "stop all containers using the legacy home volume before migration",
        ));
    }
    let root = prepare(workspace)?;
    if !merge
        && fs::read_dir(&root)?.any(|e| e.map(|e| e.file_name() != ".gitignore").unwrap_or(true))
    {
        return Err(io::Error::other(
            "session destination is not empty; use sessions migrate --merge (never overwrites)",
        ));
    }
    eprintln!(
        "Importing {volume} into {}. Same-basename checkouts may share this legacy volume. Keep all source and destination writers stopped until completion.",
        root.display()
    );
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            "501:20",
            "--entrypoint",
            "python3",
            "--mount",
        ])
        .arg(format!(
            "type=volume,source={volume},target=/legacy,readonly,volume-nocopy"
        ))
        .arg("--mount")
        .arg(bind_mount(&root, "/sessions")?)
        .arg(image)
        .args(["-c", include_str!("sessions/migrate.py")])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "session import failed; source unchanged; completed files retained. Resolve the error and retry with --merge",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preparation_is_idempotent_and_protected() {
        let temp = tempfile::tempdir().unwrap();
        let root = prepare(temp.path()).unwrap();
        assert_eq!(fs::read_to_string(root.join(".gitignore")).unwrap(), IGNORE);
        fs::write(root.join("existing.jsonl"), "private").unwrap();
        prepare(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("existing.jsonl")).unwrap(),
            "private"
        );
    }
    #[test]
    fn preserves_custom_ignore_and_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let root = prepare(temp.path()).unwrap();
        fs::write(root.join(".gitignore"), "custom").unwrap();
        assert!(prepare(temp.path()).is_err());
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "custom"
        );
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_parent() {
        let temp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), temp.path().join(".pi")).unwrap();
        assert!(prepare(temp.path()).is_err());
        assert!(!elsewhere.path().join("sessions").exists());
    }
}

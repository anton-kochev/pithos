#![cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fixtures/canonical_temp.rs"]
mod tempfile;
use pithos::docker::{HomeLease, LegacyHomeUse, VolumeName};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const VOLUME: &str = "pithos-home-project";
fn volume() -> VolumeName {
    VolumeName::new(VOLUME).unwrap()
}
fn root(home: &Path) -> PathBuf {
    home.join(".pithos-home-leases")
}
fn key(root: &Path) -> PathBuf {
    let hash: String = Sha256::digest(VOLUME.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    root.join(hash)
}
fn markers(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(key(root).join("uses"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}
fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn holder_child() {
    let Ok(kind) = std::env::var("LEASE_HOLDER") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("LEASE_ROOT").unwrap());
    if let Some(barrier) = std::env::var_os("LEASE_BARRIER") {
        fs::write(std::env::var_os("LEASE_WAITING").unwrap(), b"waiting").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !Path::new(&barrier).exists() {
            assert!(Instant::now() < deadline, "initialization barrier deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let broker = if kind == "broker" {
        Some(HomeLease::broker(&root, &volume()).unwrap())
    } else {
        None
    };
    let legacy = if kind == "legacy" {
        Some(LegacyHomeUse::acquire_current(VOLUME).unwrap())
    } else {
        None
    };
    fs::write(std::env::var_os("LEASE_READY").unwrap(), b"ready").unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    if let Some(lease) = broker {
        lease.finish().unwrap();
    }
    if let Some(lease) = legacy {
        lease.finish().unwrap();
    }
}

struct Holder(Child);
impl Holder {
    fn start(home: &Path, kind: &str, id: &str) -> Self {
        let ready = home.join(id);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "holder_child", "--nocapture"])
            .env("LEASE_HOLDER", kind)
            .env("LEASE_ROOT", root(home))
            .env("LEASE_READY", &ready)
            .env("HOME", home)
            .env("DOCKER_HOST", "unix:///irrelevant-other-daemon.sock")
            .env("DOCKER_CONTEXT", "different-context")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "holder failed before readiness"
            );
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("holder deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Self(child)
    }
    fn finish(mut self) {
        self.0.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        assert!(self.0.wait().unwrap().success());
    }
    fn crash(mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn concurrent_first_lease_initialization_preserves_every_holders_marker() {
    for _ in 0..8 {
        let home = tempfile::tempdir().unwrap();
        // Canonicalize here so this race regression does not depend on fixture portability.
        let home = fs::canonicalize(home.path()).unwrap();
        let barrier = home.join("start");
        let mut holders = Vec::new();
        for id in 0..12 {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "holder_child", "--nocapture"])
                .env("LEASE_HOLDER", "legacy")
                .env("LEASE_ROOT", root(&home))
                .env("LEASE_READY", home.join(format!("ready-{id}")))
                .env("LEASE_WAITING", home.join(format!("waiting-{id}")))
                .env("LEASE_BARRIER", &barrier)
                .env("HOME", &home)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            holders.push(Holder(child));
        }
        for prefix in ["waiting", "ready"] {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !(0..holders.len()).all(|id| home.join(format!("{prefix}-{id}")).exists()) {
                for holder in &mut holders {
                    assert!(
                        holder.0.try_wait().unwrap().is_none(),
                        "first acquisition failed"
                    );
                }
                assert!(Instant::now() < deadline, "first acquisition deadline");
                std::thread::sleep(Duration::from_millis(1));
            }
            if prefix == "waiting" {
                assert!(!root(&home).exists(), "lease initialized before barrier");
                fs::write(&barrier, b"go").unwrap();
            }
        }
        let state = root(&home);
        assert_eq!(markers(&state).len(), holders.len());
        // Leave one genuine crash marker, then ensure all other finishers preserve it.
        holders.pop().unwrap().crash();
        let before = markers(&state);
        for holder in holders {
            holder.finish();
        }
        let debt = markers(&state);
        assert_eq!(debt.len(), 1);
        assert!(before.contains(&debt[0]));
        let inode = fs::metadata(&debt[0]).unwrap().ino();
        let bytes = fs::read(&debt[0]).unwrap();
        LegacyHomeUse::acquire(&state, VOLUME)
            .unwrap()
            .finish()
            .unwrap();
        assert!(HomeLease::broker(&state, &volume()).is_err());
        assert_eq!(fs::metadata(&debt[0]).unwrap().ino(), inode);
        assert_eq!(fs::read(&debt[0]).unwrap(), bytes);
    }
}

fn temporary_home_in(parent: &Path) -> tempfile::TempDir {
    tempfile::TempDir::canonical(::tempfile::tempdir_in(parent).unwrap()).unwrap()
}

#[test]
fn canonical_temporary_wrapper_accepts_symlinked_parent_without_weakening_leases() {
    let parent = tempfile::tempdir().unwrap();
    let parent = fs::canonicalize(parent.path()).unwrap();
    let target = parent.join("real");
    fs::create_dir(&target).unwrap();
    let alias = parent.join("alias");
    symlink(&target, &alias).unwrap();
    let home = temporary_home_in(&alias);
    let raw = alias.join(home.path().file_name().unwrap());
    assert!(LegacyHomeUse::acquire(&root(&raw), VOLUME).is_err());
    assert!(!root(&raw).exists(), "production followed symlinked parent");
    let holder = Holder::start(home.path(), "legacy", "ready");
    assert_eq!(markers(&root(home.path())).len(), 1);
    holder.finish();
    HomeLease::broker(&root(home.path()), &volume())
        .unwrap()
        .finish()
        .unwrap();
}

#[test]
fn legacy_home_behind_symlinked_parent_uses_canonical_lease_root() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("real");
    fs::create_dir(&target).unwrap();
    let alias = parent.path().join("alias");
    symlink(&target, &alias).unwrap();
    let home = temporary_home_in(&alias);
    let raw = alias.join(home.path().file_name().unwrap());
    let holder = Holder::start(&raw, "legacy", "ready");
    assert_eq!(markers(&root(home.path())).len(), 1);
    assert!(HomeLease::broker(&root(home.path()), &volume()).is_err());
    holder.finish();
    assert!(markers(&root(home.path())).is_empty());
}

#[test]
fn real_process_broker_excludes_both_lanes_and_different_volume_is_independent() {
    let home = tempfile::tempdir().unwrap();
    let holder = Holder::start(home.path(), "broker", "ready");
    assert!(HomeLease::broker(&root(home.path()), &volume()).is_err());
    assert!(LegacyHomeUse::acquire(&root(home.path()), VOLUME).is_err());
    HomeLease::broker(
        &root(home.path()),
        &VolumeName::new("pithos-home-other").unwrap(),
    )
    .unwrap()
    .finish()
    .unwrap();
    holder.finish();
    HomeLease::broker(&root(home.path()), &volume())
        .unwrap()
        .finish()
        .unwrap();
}

#[test]
fn real_parallel_legacy_holders_exclude_broker_and_finish_only_their_markers() {
    let home = tempfile::tempdir().unwrap();
    let first = Holder::start(home.path(), "legacy", "one");
    let second = Holder::start(home.path(), "legacy", "two");
    let state = root(home.path());
    assert_eq!(markers(&state).len(), 2);
    let raw_lock = fs::File::open(key(&state).join("lease")).unwrap();
    assert_eq!(
        fs2::FileExt::try_lock_exclusive(&raw_lock)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(HomeLease::broker(&state, &volume()).is_err());
    first.finish();
    assert_eq!(markers(&state).len(), 1);
    assert!(HomeLease::broker(&state, &volume()).is_err());
    second.finish();
    fs2::FileExt::try_lock_exclusive(&raw_lock).unwrap();
    // No outstanding marker remains: broker must still honor the live flock.
    assert!(HomeLease::broker(&state, &volume()).is_err());
    assert!(markers(&state).is_empty());
    fs2::FileExt::unlock(&raw_lock).unwrap();
    HomeLease::broker(&state, &volume())
        .unwrap()
        .finish()
        .unwrap();
}

#[test]
fn process_crash_preserves_evidence_after_flock_release_in_both_lanes() {
    for kind in ["broker", "legacy"] {
        let home = tempfile::tempdir().unwrap();
        let holder = Holder::start(home.path(), kind, "ready");
        let state = root(home.path());
        let before = markers(&state);
        holder.crash();
        // Shared acquisition proves the crashed process's exclusive lock released.
        LegacyHomeUse::acquire(&state, VOLUME)
            .unwrap()
            .finish()
            .unwrap();
        assert!(HomeLease::broker(&state, &volume()).is_err());
        assert_eq!(markers(&state), before);
    }
}

#[test]
fn drop_is_not_completion_and_legacy_never_deletes_foreign_debt() {
    let home = tempfile::tempdir().unwrap();
    let state = root(home.path());
    drop(HomeLease::broker(&state, &volume()).unwrap());
    let debt = markers(&state);
    assert_eq!(debt.len(), 1);
    let content = fs::read(&debt[0]).unwrap();
    LegacyHomeUse::acquire(&state, VOLUME)
        .unwrap()
        .finish()
        .unwrap();
    assert!(HomeLease::broker(&state, &volume()).is_err());
    assert_eq!(markers(&state), debt);
    assert_eq!(fs::read(&debt[0]).unwrap(), content);
}

#[test]
fn private_layout_is_stable_and_markers_are_opaque_static_per_holder() {
    let home = tempfile::tempdir().unwrap();
    let state = root(home.path());
    let a = LegacyHomeUse::acquire(&state, VOLUME).unwrap();
    let b = LegacyHomeUse::acquire(&state, VOLUME).unwrap();
    for dir in [&state, &key(&state), &key(&state).join("uses")] {
        assert_eq!(mode(dir), 0o700);
    }
    let lock = key(&state).join("lease");
    assert_eq!(mode(&lock), 0o600);
    let inode = fs::metadata(&lock).unwrap().ino();
    let paths = markers(&state);
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    for path in paths {
        assert_eq!(path.file_name().unwrap().len(), 64);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
        assert_eq!(
            fs::read(path).unwrap(),
            b"pithos-home-use-v1\noutstanding\n"
        );
    }
    a.finish().unwrap();
    b.finish().unwrap();
    HomeLease::broker(&state, &volume())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(
        fs::metadata(lock).unwrap().ino(),
        inode,
        "lock inode must never be replaced"
    );
}

#[test]
fn explicit_finish_refuses_replaced_or_hardlinked_own_marker() {
    for hardlink in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let state = root(home.path());
        let holder = HomeLease::broker(&state, &volume()).unwrap();
        let path = markers(&state).pop().unwrap();
        if hardlink {
            fs::hard_link(&path, home.path().join("saved")).unwrap();
        } else {
            fs::rename(&path, home.path().join("saved")).unwrap();
            fs::write(&path, b"foreign").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert!(holder.finish().is_err());
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(HomeLease::broker(&state, &volume()).is_err());
    }
}

#[test]
fn exact_modes_and_existing_lock_contents_refuse_without_repair() {
    for relative in ["", "key", "uses", "lease"] {
        for permissions in [0o000, 0o500, 0o644, 0o755, 0o1700] {
            let home = tempfile::tempdir().unwrap();
            let state = root(home.path());
            HomeLease::broker(&state, &volume())
                .unwrap()
                .finish()
                .unwrap();
            let path = match relative {
                "" => state.clone(),
                "key" => key(&state),
                "uses" => key(&state).join("uses"),
                _ => key(&state).join("lease"),
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(permissions)).unwrap();
            let before = fs::symlink_metadata(&path).unwrap();
            assert!(
                HomeLease::broker(&state, &volume()).is_err(),
                "accepted {relative} {permissions:o}"
            );
            assert!(LegacyHomeUse::acquire(&state, VOLUME).is_err());
            let after = fs::symlink_metadata(&path).unwrap();
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.mode(), after.mode());
            // Restore fixture access only so TempDir can remove test-owned files.
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if relative == "lease" { 0o600 } else { 0o700 }),
            )
            .unwrap();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let state = root(home.path());
    HomeLease::broker(&state, &volume())
        .unwrap()
        .finish()
        .unwrap();
    let lock = key(&state).join("lease");
    fs::write(&lock, b"do not truncate").unwrap();
    assert!(HomeLease::broker(&state, &volume()).is_err());
    assert_eq!(fs::read(lock).unwrap(), b"do not truncate");
}

#[test]
fn corrupt_marker_state_is_refused_before_creating_missing_lock() {
    for broker in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let state = root(home.path());
        HomeLease::broker(&state, &volume())
            .unwrap()
            .finish()
            .unwrap();
        let lock = key(&state).join("lease");
        fs::remove_file(&lock).unwrap();
        let foreign = key(&state).join("uses/foreign");
        fs::write(&foreign, b"unchanged").unwrap();
        let failed = if broker {
            HomeLease::broker(&state, &volume()).is_err()
        } else {
            LegacyHomeUse::acquire(&state, VOLUME).is_err()
        };
        assert!(failed);
        assert!(
            !lock.exists(),
            "repaired corrupt state with a new lock file"
        );
        assert_eq!(fs::read(foreign).unwrap(), b"unchanged");
    }
}

#[test]
fn trailing_slash_cannot_turn_a_root_symlink_into_a_directory() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let link = home.path().join("alias");
    symlink(target.path(), &link).unwrap();
    let supplied = PathBuf::from(format!("{}/", link.display()));
    assert!(
        HomeLease::broker(&supplied, &volume()).is_err(),
        "followed root symlink with trailing slash"
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
}

#[test]
fn links_special_files_and_non_directory_roots_are_not_adopted() {
    for kind in [
        "file-root",
        "symlink-root",
        "symlink-lock",
        "hardlink-lock",
        "symlink-uses",
        "fifo-lock",
    ] {
        let home = tempfile::tempdir().unwrap();
        let state = root(home.path());
        if kind == "file-root" {
            fs::write(&state, b"untouched").unwrap();
        } else if kind == "symlink-root" {
            symlink(home.path(), &state).unwrap();
        } else {
            HomeLease::broker(&state, &volume())
                .unwrap()
                .finish()
                .unwrap();
            let lock = key(&state).join("lease");
            match kind {
                "hardlink-lock" => fs::hard_link(&lock, home.path().join("alias")).unwrap(),
                "symlink-lock" => {
                    fs::remove_file(&lock).unwrap();
                    symlink(home.path().join("absent"), lock).unwrap();
                }
                "symlink-uses" => {
                    let uses = key(&state).join("uses");
                    fs::remove_dir(&uses).unwrap();
                    symlink(home.path(), uses).unwrap();
                }
                "fifo-lock" => {
                    fs::remove_file(&lock).unwrap();
                    assert!(
                        Command::new("mkfifo")
                            .arg(&lock)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                _ => unreachable!(),
            }
        }
        assert!(
            HomeLease::broker(&state, &volume()).is_err(),
            "accepted {kind}"
        );
        assert!(LegacyHomeUse::acquire(&state, VOLUME).is_err());
        assert!(!home.path().join("absent").exists());
    }
}

// Self-clearing debt: a broker run that holds the exclusive flock knows every
// marker belongs to a dead process; it clears them only when Docker says no
// container mounts the volume.

#[test]
fn crashed_holders_debt_is_cleared_when_nothing_mounts_the_home() {
    for kind in ["broker", "legacy"] {
        let home = tempfile::tempdir().unwrap();
        let state = root(home.path());
        Holder::start(home.path(), kind, "ready").crash();
        let debt = markers(&state);
        assert_eq!(debt.len(), 1);
        let (lease, cleared) =
            HomeLease::broker_recovering(&state, &volume(), || Ok(false)).unwrap();
        assert_eq!(cleared, 1, "{kind}");
        let now = markers(&state);
        assert_eq!(now.len(), 1, "only this holder's own marker");
        assert!(!now.contains(&debt[0]));
        lease.finish().unwrap();
        assert!(markers(&state).is_empty());
    }
}

#[test]
fn debt_stays_while_a_container_mounts_the_home_or_docker_is_unsure() {
    let home = tempfile::tempdir().unwrap();
    let state = root(home.path());
    drop(HomeLease::broker(&state, &volume()).unwrap());
    let debt = markers(&state);
    let mounted = HomeLease::broker_recovering(&state, &volume(), || Ok(true))
        .err()
        .unwrap();
    assert_eq!(mounted.kind(), std::io::ErrorKind::ResourceBusy);
    assert_eq!(markers(&state), debt);
    let unsure = HomeLease::broker_recovering(&state, &volume(), || {
        Err(std::io::Error::other("daemon query failed"))
    });
    assert!(unsure.is_err());
    assert_eq!(markers(&state), debt);
}

#[test]
fn docker_is_asked_only_about_real_debt_and_never_past_a_live_holder() {
    let home = tempfile::tempdir().unwrap();
    let state = root(home.path());
    let (lease, cleared) = HomeLease::broker_recovering(&state, &volume(), || {
        panic!("no debt: Docker must not be asked")
    })
    .unwrap();
    assert_eq!(cleared, 0);
    lease.finish().unwrap();
    let holder = Holder::start(home.path(), "legacy", "live");
    let busy = HomeLease::broker_recovering(&state, &volume(), || {
        panic!("live holder: Docker must not be asked")
    })
    .err()
    .unwrap();
    assert_eq!(busy.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(markers(&state).len(), 1, "the live holder's marker stays");
    holder.finish();
}

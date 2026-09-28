//! Private per-run broker credential building block; no broker activation.
//!
//! Linux/macOS only. The caller supplies an existing real 0700 run directory
//! owned by the effective non-root host UID. Its ancestors must remain trusted,
//! host-only paths outside agent-writable mounts. No recursive provisioning,
//! symlink adoption, permission repair or overwrite is performed. Directory and
//! file descriptors are held until drop, but drop never unlinks a credential.
//!
//! Descriptor/path checks detect existing replacements, not hostile concurrent
//! same-UID/root mutations or a remote daemon's different view of the path.
//! Trust local filesystem ownership/mode and sync semantics; ACLs, hostile
//! filesystems and ancestor races are not independently admitted here.
//! Keep the owner alive and the tree stable until all consumers stop. Unlinking
//! is not revocation: copies, open descriptors and read-only binds can survive.
//!
//! The endpoint is explicit host-owned input, never agent configuration. Its
//! narrow HTTP grammar proves neither secure transport nor listener/network
//! authorization. No listener, process runner, environment secret, or admission
//! proof is provided. Actual Docker file-access acceptance remains the caller's
//! responsibility before activation.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use crate::docker::HostIdentity;
use serde::Serialize;
use std::{
    ffi::OsString,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Write},
    net::Ipv4Addr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

const FILE_NAME: &str = "broker-client.json";
const CONTAINER_PATH: &str = "/run/pithos-broker/client.json";
// Fixed code only: neither endpoint nor token is interpolated into argv. The
// program performs no network I/O, writes, permission repair or data output.
const PROBE: &str = r#"
import sys

def check():
    import ipaddress, json, os, re, stat
    path, uid, gid, owner = sys.argv[1:]
    uid, gid, owner = int(uid), int(gid), int(owner)
    if not (0 < uid < 4294967294 and 0 < gid < 4294967294 and owner in (0, uid)):
        return False
    if (os.geteuid(), os.getegid()) != (uid, gid):
        return False
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        metadata = os.fstat(stream.fileno())
        named = os.lstat(path)
        if not stat.S_ISREG(metadata.st_mode) or not stat.S_ISREG(named.st_mode):
            return False
        if (metadata.st_dev, metadata.st_ino) != (named.st_dev, named.st_ino):
            return False
        if metadata.st_uid != owner or metadata.st_nlink != 1 or stat.S_IMODE(metadata.st_mode) != 0o600:
            return False
        if not os.fstatvfs(stream.fileno()).f_flag & os.ST_RDONLY:
            return False
        if os.access(path, os.W_OK, effective_ids=True):
            return False
        data = stream.read(513)
        if len(data) > 512 or len(data) != metadata.st_size:
            return False
    def unique_fields(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError()
            result[key] = value
        return result
    value = json.loads(data.decode('utf-8'), object_pairs_hook=unique_fields)
    if type(value) is not dict or set(value) != {'version', 'endpoint', 'token'}:
        return False
    if type(value['version']) is not int or value['version'] != 1:
        return False
    token, endpoint = value['token'], value['endpoint']
    if type(token) is not str or re.fullmatch(r'[0-9a-f]{64}', token) is None:
        return False
    if type(endpoint) is not str or len(endpoint) > 64:
        return False
    match = re.fullmatch(r'http://(host\.docker\.internal|localhost|\[::1\]|(?:[0-9]{1,3}\.){3}[0-9]{1,3}):([1-9][0-9]{0,4})', endpoint)
    if match is None or int(match[2]) > 65535:
        return False
    if match[1] not in ('host.docker.internal', 'localhost', '[::1]'):
        address = ipaddress.IPv4Address(match[1])
        if address.packed[0] == 0 or address.packed[0] >= 224:
            return False
    return True

try:
    success = check()
except BaseException:
    success = False
sys.exit(0 if success else 1)
"#;

/// Diagnostics never include endpoint, token, file contents, or caller paths.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("invalid broker endpoint")]
    InvalidEndpoint,
    #[error("credential probe requires a full immutable image ID")]
    InvalidImage,
    #[error("credential requires a supported non-root effective host identity")]
    Identity,
    #[error("credential randomness unavailable")]
    Random,
    #[error("credential path is not private or has an unsafe type")]
    UnsafePath,
    #[error("credential path belongs to a different host user")]
    ForeignOwner,
    #[error("credential I/O failed ({0:?}); retain run evidence")]
    Io(io::ErrorKind),
}

impl From<io::Error> for CredentialError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

// This is syntax validation of trusted host input, not route/bridge discovery,
// listener binding, TLS, or authorization. Only explicit unicast IPv4 addresses
// (including loopback), localhost, host.docker.internal and [::1] are accepted.
fn validate_endpoint(endpoint: &str) -> Result<(), CredentialError> {
    let valid = || {
        if endpoint.len() > 64 {
            return None;
        }
        let authority = endpoint.strip_prefix("http://")?;
        let (host, port) = authority.rsplit_once(':')?;
        if port.is_empty()
            || port.len() > 5
            || port.starts_with('0')
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().ok()? == 0
        {
            return None;
        }
        if !matches!(host, "localhost" | "host.docker.internal" | "[::1]") {
            let address = host.parse::<Ipv4Addr>().ok()?;
            if address.octets()[0] == 0 || address.octets()[0] >= 224 {
                return None;
            }
        }
        Some(())
    };
    valid().ok_or(CredentialError::InvalidEndpoint)
}

fn check_metadata(
    metadata: &Metadata,
    directory: bool,
    identity: HostIdentity,
) -> Result<(), CredentialError> {
    if metadata.uid() != identity.uid() {
        return Err(CredentialError::ForeignOwner);
    }
    if (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
        || metadata.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
    {
        return Err(CredentialError::UnsafePath);
    }
    Ok(())
}

fn check_path(
    file: &File,
    path: &Path,
    directory: bool,
    identity: HostIdentity,
) -> Result<(), CredentialError> {
    let descriptor = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    check_metadata(&descriptor, directory, identity)?;
    check_metadata(&named, directory, identity)?;
    if descriptor.dev() != named.dev() || descriptor.ino() != named.ino() {
        return Err(CredentialError::UnsafePath);
    }
    Ok(())
}

#[derive(Serialize)]
struct FileContents<'a> {
    version: u32,
    endpoint: &'a str,
    token: &'a str,
}

/// A secret bearer token. No Display, Clone or serialization implementation.
pub struct SecretToken(String);

impl SecretToken {
    /// Deliberately expose the secret for future authentication. Never log it or
    /// place it in argv, environment variables, URLs or transcripts.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

/// Owns the open credential file. Dropping the handle does not remove the file.
pub struct RunCredential {
    file: File,
    path: PathBuf,
    directory_file: File,
    directory: PathBuf,
    identity: HostIdentity,
    token: SecretToken,
}

impl std::fmt::Debug for RunCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RunCredential([REDACTED])")
    }
}

impl RunCredential {
    /// Build Docker argv (without the executable); never invoke Docker or admit
    /// an image. Requires a trusted, already-built full `sha256:<64 lowercase
    /// hex>` image ID containing /usr/bin/python3. Tags and pull are forbidden.
    /// The caller owns daemon selection, a bounded timeout, output handling,
    /// stopping/reaping probe containers and all actual platform acceptance.
    /// Success of the eventual probe establishes file access only, not listener
    /// reachability, TLS, authorization, account/home admission or revocation.
    pub fn probe_argv(
        &self,
        identity: HostIdentity,
        image: &str,
    ) -> Result<Vec<OsString>, CredentialError> {
        if identity != self.identity {
            return Err(CredentialError::Identity);
        }
        let digest = image
            .strip_prefix("sha256:")
            .ok_or(CredentialError::InvalidImage)?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            return Err(CredentialError::InvalidImage);
        }
        let mount = self.mount_arg()?;
        Ok(vec![
            "run".into(),
            "--rm".into(),
            "--pull=never".into(),
            "--network=none".into(),
            "--read-only".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--user".into(),
            identity.docker_user().into(),
            "--entrypoint=/usr/bin/python3".into(),
            "--mount".into(),
            mount,
            image.into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            PROBE.into(),
            CONTAINER_PATH.into(),
            identity.uid().to_string().into(),
            identity.gid().to_string().into(),
            // Docker Desktop presents shared host files as root-owned in its VM
            // and does not enforce in-VM ownership; the host file stays 0600.
            if cfg!(target_os = "macos") {
                "0".to_string()
            } else {
                identity.uid().to_string()
            }
            .into(),
        ])
    }

    /// Unlink only the still-owned credential, after the caller guarantees that
    /// the listener and all credential-consuming containers/operations stopped.
    /// Rechecks owner, mode, single link and descriptor/path identity first.
    /// Errors leave evidence untouched; no repair or replacement removal occurs.
    /// Success is only an unlink, not revocation, secure erasure or crash-durable
    /// removal: existing binds, open descriptors and token copies can survive it.
    /// Repeated cleanup or subsequent mount requests fail rather than adopt a path.
    pub fn cleanup(&mut self) -> Result<(), CredentialError> {
        self.check_paths()?;
        fs::remove_file(&self.path)?;
        Ok(())
    }

    fn check_paths(&self) -> Result<(), CredentialError> {
        if HostIdentity::effective().map_err(|_| CredentialError::Identity)? != self.identity {
            return Err(CredentialError::Identity);
        }
        check_path(&self.directory_file, &self.directory, true, self.identity)?;
        check_path(&self.file, &self.path, false, self.identity)
    }

    /// Recheck descriptor/path identity and build a read-only exact-file Docker
    /// `--mount` value, never a token argv. The caller must keep host paths trusted
    /// and the handle alive; a returned string does not pin a future Docker bind.
    /// CR/LF paths are rejected because Docker's CSV parser normalizes CRLF,
    /// including inside quoted fields, which could select a different file.
    pub fn mount_arg(&self) -> Result<OsString, CredentialError> {
        self.check_paths()?;
        if self
            .path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\r' | b'\n'))
        {
            return Err(CredentialError::UnsafePath);
        }
        let mut mount = crate::sessions::bind_mount(&self.path, CONTAINER_PATH)?;
        mount.push(",readonly");
        Ok(mount)
    }

    /// Borrow the secret without implicitly formatting or serializing it.
    pub fn token(&self) -> &SecretToken {
        &self.token
    }

    /// Create `broker-client.json` with create-new semantics and mode 0600,
    /// containing exactly version 1, the endpoint and 64 lowercase hex digits
    /// encoding 32 OS-random bytes. Sync file and directory before returning.
    ///
    /// Accepts `http://HOST:PORT` only (at most 64 bytes): HOST is exactly
    /// `host.docker.internal`, `localhost`, `[::1]`, or canonical dotted IPv4
    /// outside 0/8 and 224/3. PORT is canonical decimal 1..=65535. No userinfo,
    /// path (even `/`), query or fragment. An explicit IPv4 address is not proof
    /// that it is a bridge or an authorized destination. Relative directories
    /// are anchored now; `..` components are refused.
    ///
    /// # Errors
    /// Rejects root/reserved identities, invalid endpoint or unsafe paths. I/O
    /// failure may leave a partial private file; preserve it for explicit host
    /// recovery, never retry by overwriting or assume no credential was written.
    /// A restrictive umask causes rejection, not chmod repair.
    pub fn create(directory: impl AsRef<Path>, endpoint: &str) -> Result<Self, CredentialError> {
        let identity = HostIdentity::effective().map_err(|_| CredentialError::Identity)?;
        validate_endpoint(endpoint)?;
        // Normalize redundant separators and trailing dots before lstat, so a
        // symlink spelled `link/` or `link/.` cannot bypass the leaf check.
        let directory = directory.as_ref();
        if directory
            .components()
            .any(|part| part == Component::ParentDir)
        {
            return Err(CredentialError::UnsafePath);
        }
        let directory = std::path::absolute(directory)?
            .components()
            .collect::<std::path::PathBuf>();
        check_metadata(&fs::symlink_metadata(&directory)?, true, identity)?;
        let directory_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&directory)?;
        check_path(&directory_file, &directory, true, identity)?;
        let path = directory.join(FILE_NAME);
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| CredentialError::Random)?;
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let contents = serde_json::to_vec(&FileContents {
            version: 1,
            endpoint,
            token: &token,
        })
        .map_err(|_| CredentialError::Io(io::ErrorKind::InvalidData))?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        check_path(&file, &path, false, identity)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        directory_file.sync_all()?;
        let credential = Self {
            file,
            path,
            directory_file,
            directory,
            identity,
            token: SecretToken(token),
        };
        credential.check_paths()?;
        Ok(credential)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn foreign_owners_and_changed_effective_identity_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut credential =
            RunCredential::create(directory.path(), "http://localhost:1234").unwrap();
        let other =
            HostIdentity::new(credential.identity.uid() + 1, credential.identity.gid()).unwrap();
        // Real metadata, checked against another identity without requiring
        // chown privileges or altering the process's global credentials.
        for (file, is_directory) in [
            (&credential.file, false),
            (&credential.directory_file, true),
        ] {
            assert!(matches!(
                check_metadata(&file.metadata().unwrap(), is_directory, other),
                Err(CredentialError::ForeignOwner)
            ));
        }
        credential.identity = other;
        assert!(matches!(
            credential.mount_arg(),
            Err(CredentialError::Identity)
        ));
        assert!(matches!(
            credential.cleanup(),
            Err(CredentialError::Identity)
        ));
        assert!(credential.path.is_file());
    }
}

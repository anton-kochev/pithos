//! Opt-in image-build identity; not runtime admission or broker authorization.

use sha2::{Digest, Sha256};

/// The two owned image accounts supported by the fixed build overlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageRole {
    Pi,
    Browser,
}

/// Emit a fixed late overlay for an owned Pi or browser image, after installers.
/// Pair with [`crate::embed::extract_identity_to`]. The embedded helper is hashed
/// into these bytes, so hashing the emitted Dockerfile also covers helper changes.
/// Does not build, admit or run an image, and must never repair a mounted home.
pub fn identity_overlay(identity: HostIdentity, role: ImageRole) -> String {
    let (account, home) = match role {
        ImageRole::Pi => ("pi", "/home/pi"),
        ImageRole::Browser => ("browser", "/tmp/browser-home"),
    };
    let digest: String = Sha256::digest(crate::embed::IDENTITY_IMAGE_PY)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let python = match role {
        ImageRole::Pi => "",
        ImageRole::Browser => {
            "RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*\n"
        }
    };
    format!(
        "\nUSER root\n\
         {python}\
         # Identity image helper sha256:{digest}\n\
         COPY identity_image.py /tmp/pithos-identity-image.py\n\
         RUN /usr/bin/python3 /tmp/pithos-identity-image.py --image-build {account} {uid} {gid} && rm /tmp/pithos-identity-image.py\n\
         ENV HOME={home} USER={account} LOGNAME={account}\n\
         USER {user}\n",
        uid = identity.uid(),
        gid = identity.gid(),
        user = identity.docker_user(),
    )
}

/// Effective host IDs without supplementary groups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostIdentity {
    uid: u32,
    gid: u32,
}

/// Failure to capture or validate a supported, non-root host identity.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum IdentityError {
    #[error("image identity requires non-root UID/GID without reserved -1/-2 sentinels")]
    InvalidIds,
    #[error("host identity is supported only on Linux and macOS")]
    UnsupportedPlatform,
}

impl HostIdentity {
    /// Validate numeric IDs. Root and the reserved unsigned -1/-2 values are forbidden.
    pub fn new(uid: u32, gid: u32) -> Result<Self, IdentityError> {
        if uid == 0 || gid == 0 || uid >= u32::MAX - 1 || gid >= u32::MAX - 1 {
            return Err(IdentityError::InvalidIds);
        }
        Ok(Self { uid, gid })
    }

    /// Capture OS effective IDs, never environment variables or supplementary groups.
    /// Rejects root/reserved IDs and hosts other than Linux and macOS.
    pub fn effective() -> Result<Self, IdentityError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // SAFETY: scalar process queries, with no pointers or failure sentinel.
            Self::new(unsafe { libc::geteuid() }, unsafe { libc::getegid() })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(IdentityError::UnsupportedPlatform)
        }
    }

    pub fn uid(self) -> u32 {
        self.uid
    }

    pub fn gid(self) -> u32 {
        self.gid
    }

    pub fn docker_user(self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }
}

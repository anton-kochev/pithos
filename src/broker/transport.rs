//! Owned broker listeners and their container-advertised authorities.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

#[cfg(target_os = "linux")]
use crate::docker::ManagedDocker;

/// How a managed container is permitted to reach the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccess {
    Offline,
    DockerDesktop,
    LinuxHostGateway(InspectedGateway),
}

/// Only a successfully bound, twice-inspected Linux endpoint can issue this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectedGateway {
    #[cfg(target_os = "linux")]
    fingerprint: (Ipv4Addr, Ipv4Addr, u8),
}

impl InspectedGateway {
    /// Exact gateway used for the listener and container host mapping.
    pub fn gateway(self) -> Ipv4Addr {
        #[cfg(target_os = "linux")]
        {
            self.fingerprint.0
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!("Linux gateway policies cannot be created on this target")
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn recheck(self, docker: &mut ManagedDocker) -> Result<(), TransportError> {
        if docker
            .inspect_bridge()
            .map_err(|_| TransportError::Bridge)?
            .fingerprint()
            != self.fingerprint
        {
            return Err(TransportError::Changed);
        }
        Ok(())
    }
}

/// Failure to construct a broker endpoint without broadening its bind address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("invalid broker listener")]
    Listener,
    #[error("Docker bridge observation unavailable or unsupported")]
    Bridge,
    #[error("Docker bridge observation changed")]
    Changed,
    #[error("exact broker address cannot be bound")]
    Bind,
}

/// An inseparable listener, bind address, advertised authority and host policy.
pub struct BrokerEndpoint {
    listener: TcpListener,
    local_addr: SocketAddr,
    authority: String,
    access: HostAccess,
}

impl BrokerEndpoint {
    /// Adopt an already-bound loopback listener for offline use only.
    pub fn offline(listener: TcpListener) -> Result<Self, TransportError> {
        let local_addr = listener
            .local_addr()
            .map_err(|_| TransportError::Listener)?;
        if !local_addr.ip().is_loopback() || local_addr.port() == 0 {
            return Err(TransportError::Listener);
        }
        Ok(Self {
            listener,
            local_addr,
            authority: local_addr.to_string(),
            access: HostAccess::Offline,
        })
    }

    /// Inspect, bind and re-inspect the frozen native-Linux Docker bridge.
    ///
    /// Binding is attempted only for the exact validated gateway. A changed
    /// observation or bind failure closes the listener and never falls back to
    /// a wildcard or loopback address.
    #[cfg(target_os = "linux")]
    pub fn linux(docker: &mut ManagedDocker) -> Result<Self, TransportError> {
        let first = docker
            .inspect_bridge()
            .map_err(|_| TransportError::Bridge)?;
        let listener = TcpListener::bind((first.gateway(), 0)).map_err(|_| TransportError::Bind)?;
        let local_addr = listener.local_addr().map_err(|_| TransportError::Bind)?;
        let second = docker
            .inspect_bridge()
            .map_err(|_| TransportError::Bridge)?;
        if first != second {
            return Err(TransportError::Changed);
        }
        Ok(Self {
            listener,
            local_addr,
            authority: format!("host.docker.internal:{}", local_addr.port()),
            access: HostAccess::LinuxHostGateway(InspectedGateway {
                fingerprint: second.fingerprint(),
            }),
        })
    }

    /// Bind the Docker Desktop host endpoint to IPv4 loopback only.
    #[cfg(target_os = "macos")]
    pub fn docker_desktop() -> Result<Self, TransportError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| TransportError::Bind)?;
        let local_addr = listener.local_addr().map_err(|_| TransportError::Bind)?;
        Ok(Self {
            listener,
            local_addr,
            authority: format!("host.docker.internal:{}", local_addr.port()),
            access: HostAccess::DockerDesktop,
        })
    }

    /// Return the exact socket address used for host-side connections.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return the authority required in credentials and authenticated Host headers.
    pub fn advertised_authority(&self) -> &str {
        &self.authority
    }

    /// Return the immutable container host-access policy for later launch checks.
    pub fn host_access(&self) -> HostAccess {
        self.access
    }

    pub(crate) fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub(crate) fn set_nonblocking(&self) -> Result<(), TransportError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|_| TransportError::Listener)
    }
}

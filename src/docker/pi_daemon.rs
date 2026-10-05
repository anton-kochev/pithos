//! An isolated Docker daemon for Pi: an `<ipv4>:<port>` in the `pithos-docker`
//! VM that Pi's container can reach, handed to Pi as `DOCKER_HOST`. Pithos
//! never uses it.

use std::{
    io::{self, Read, Write},
    net::{SocketAddrV4, TcpStream},
    time::{Duration, Instant},
};

/// A `/_ping` answer is a few hundred bytes; anything longer is not one.
const MAX_ANSWER: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiDaemon {
    address: SocketAddrV4,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PiDaemonError {
    /// Not `<ipv4>:<port>`, or an address the container could never reach.
    #[error("not an <ipv4>:<port> Pi's container can reach")]
    Malformed,
    #[error("no Docker daemon answered at {0}")]
    Unreachable(SocketAddrV4),
    #[error("{0} answered, but not like a Docker daemon")]
    NotDocker(SocketAddrV4),
}

impl PiDaemon {
    /// Accepts only a literal `<ipv4>:<port>` that round-trips exactly
    /// (`SocketAddrV4::to_string() == value`), port != 0, and an address that is
    /// not loopback, unspecified, multicast, broadcast or link-local. Names are
    /// refused: the host and the container resolve them differently.
    pub fn parse(value: &str) -> Result<Self, PiDaemonError> {
        let address: SocketAddrV4 = value.parse().map_err(|_| PiDaemonError::Malformed)?;
        let ip = address.ip();
        if address.to_string() != value
            || address.port() == 0
            || ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip.is_broadcast()
            || ip.is_link_local()
        {
            return Err(PiDaemonError::Malformed);
        }
        Ok(Self { address })
    }

    pub fn address(&self) -> SocketAddrV4 {
        self.address
    }

    /// `[("DOCKER_HOST", "tcp://<ip>:<port>"), ("TESTCONTAINERS_HOST_OVERRIDE", "<ip>")]`
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("DOCKER_HOST", format!("tcp://{}", self.address)),
            (
                "TESTCONTAINERS_HOST_OVERRIDE",
                self.address.ip().to_string(),
            ),
        ]
    }

    /// The same two variables as `KEY=value\n` lines for a Docker `--env-file`.
    pub fn env_lines(&self) -> String {
        self.env()
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect()
    }

    /// `GET /_ping` over plain HTTP/1.1 must answer status 200 with body `OK`
    /// within `timeout` (connect, write and read each bounded by it).
    /// Unreachable (connect error, timeout, EOF before a status line) -> Unreachable;
    /// any other answer -> NotDocker.
    pub fn preflight(&self, timeout: Duration) -> Result<(), PiDaemonError> {
        let answer = self
            .ping(timeout)
            .map_err(|_| PiDaemonError::Unreachable(self.address))?;
        let text = String::from_utf8_lossy(&answer);
        let Some((status_line, _)) = text.split_once("\r\n") else {
            return Err(PiDaemonError::Unreachable(self.address));
        };
        let mut words = status_line.split(' ');
        let http = words
            .next()
            .is_some_and(|version| version.starts_with("HTTP/1."));
        let ok = words.next() == Some("200");
        let body = text
            .split_once("\r\n\r\n")
            .is_some_and(|(_, body)| body.trim() == "OK");
        if http && ok && body {
            Ok(())
        } else {
            Err(PiDaemonError::NotDocker(self.address))
        }
    }

    /// Everything the daemon sent back before EOF, `timeout` or `MAX_ANSWER`.
    /// A daemon that ignores `Connection: close` still gets judged on what
    /// it sent; a peer that never writes yields an empty answer.
    fn ping(&self, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut stream = TcpStream::connect_timeout(&self.address.into(), timeout)?;
        stream.set_write_timeout(Some(timeout))?;
        let request = format!(
            "GET /_ping HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.address
        );
        stream.write_all(request.as_bytes())?;
        let mut answer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || answer.len() >= MAX_ANSWER {
                break;
            }
            stream.set_read_timeout(Some(remaining))?;
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => answer.extend_from_slice(&chunk[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        answer.truncate(MAX_ANSWER);
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, TcpListener};

    const TIMEOUT: Duration = Duration::from_millis(200);

    fn loopback(listener: &TcpListener) -> SocketAddrV4 {
        match listener.local_addr().unwrap() {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(address) => panic!("loopback listener is IPv6: {address}"),
        }
    }

    /// A one-shot peer: reads the request, writes `response`, hangs up.
    fn peer_answering(response: &'static [u8]) -> PiDaemon {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = loopback(&listener);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            stream.write_all(response).unwrap();
        });
        PiDaemon { address }
    }

    #[test]
    fn parse_accepts_only_literal_reachable_ipv4_socket_addresses() {
        // Arrange
        let accepted = ["192.168.64.3:2375", "10.0.0.7:12345"];
        let rejected = [
            "",
            "192.168.64.3",
            "192.168.64.3:0",
            "127.0.0.1:2375",
            "0.0.0.0:2375",
            "224.0.0.1:2375",
            "255.255.255.255:2375",
            "169.254.1.1:2375",
            "pithos-docker:2375",
            "[::1]:2375",
            "tcp://192.168.64.3:2375",
            "192.168.64.3:2375\n",
            "192.168.064.3:2375",
            " 192.168.64.3:2375",
        ];

        // Act / Assert
        for value in accepted {
            let daemon = PiDaemon::parse(value).unwrap_or_else(|e| panic!("{value:?}: {e}"));
            assert_eq!(daemon.address().to_string(), value);
        }
        for value in rejected {
            assert_eq!(
                PiDaemon::parse(value),
                Err(PiDaemonError::Malformed),
                "{value:?}"
            );
        }
    }

    #[test]
    fn env_and_env_lines_hand_pi_the_daemon_and_its_host() {
        // Arrange
        let daemon = PiDaemon::parse("192.168.64.3:2375").unwrap();

        // Act
        let env = daemon.env();
        let lines = daemon.env_lines();

        // Assert
        assert_eq!(
            env,
            [
                ("DOCKER_HOST", "tcp://192.168.64.3:2375".to_owned()),
                ("TESTCONTAINERS_HOST_OVERRIDE", "192.168.64.3".to_owned()),
            ]
        );
        assert_eq!(
            lines,
            "DOCKER_HOST=tcp://192.168.64.3:2375\nTESTCONTAINERS_HOST_OVERRIDE=192.168.64.3\n"
        );
    }

    #[test]
    fn preflight_accepts_a_docker_ping_answer() {
        // Arrange
        let daemon =
            peer_answering(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");

        // Act
        let result = daemon.preflight(TIMEOUT);

        // Assert
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn preflight_refuses_a_non_200_answer() {
        // Arrange
        let daemon = peer_answering(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");

        // Act
        let result = daemon.preflight(TIMEOUT);

        // Assert
        assert_eq!(result, Err(PiDaemonError::NotDocker(daemon.address())));
    }

    #[test]
    fn preflight_refuses_a_200_answer_whose_body_is_not_ok() {
        // Arrange
        let daemon = peer_answering(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nnope");

        // Act
        let result = daemon.preflight(TIMEOUT);

        // Assert
        assert_eq!(result, Err(PiDaemonError::NotDocker(daemon.address())));
    }

    #[test]
    fn preflight_reports_a_closed_port_as_unreachable() {
        // Arrange
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = loopback(&listener);
        drop(listener);
        let daemon = PiDaemon { address };

        // Act
        let result = daemon.preflight(TIMEOUT);

        // Assert
        assert_eq!(result, Err(PiDaemonError::Unreachable(address)));
    }

    #[test]
    fn preflight_reports_a_silent_peer_as_unreachable_within_the_timeout() {
        // Arrange
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = loopback(&listener);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Hold the connection open until the client gives up.
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
        });
        let daemon = PiDaemon { address };

        // Act
        let started = Instant::now();
        let result = daemon.preflight(TIMEOUT);
        let elapsed = started.elapsed();

        // Assert
        assert_eq!(result, Err(PiDaemonError::Unreachable(address)));
        assert!(elapsed < TIMEOUT * 5, "took {elapsed:?}");
    }
}

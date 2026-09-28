//! Bounded status protocol building block; no listener or Docker authority.
//!
//! The host owns the credential, expected authority, snapshot, shutdown and
//! connected socket. This module supplies no binding policy, route admission,
//! TLS, concurrency manager or connection thread. An exchange is not proof of
//! container reachability or authorization to activate a broker.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::credential::SecretToken;
use crate::lifecycle::Shutdown;
use std::{
    borrow::Borrow,
    io::{self, Read, Write},
    net::{Shutdown as SocketShutdown, TcpStream},
    time::{Duration, Instant},
};

/// Redacted lifecycle state; no user-controlled strings or Docker identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Ready,
    Stopping,
    RecoveryRequired,
}

/// A host-owned in-memory snapshot, not a Docker query or readiness probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub phase: Phase,
}

/// Per-connection bounds. Values are checked before processing the socket.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 4..=8192 bytes, including the request line and final CRLFCRLF.
    pub max_header_bytes: usize,
    /// 1..=64 header fields (not counting the request line).
    pub max_header_fields: usize,
    /// 256..=1024 bytes. Every fixed-schema response fits in 256 bytes.
    pub max_response_bytes: usize,
    /// Nonzero, at most 60 seconds. Starts before the first read; never resets.
    pub read_timeout: Duration,
    /// Nonzero, at most 60 seconds. Starts before the first write; never resets.
    pub write_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header_bytes: 8192,
            max_header_fields: 64,
            max_response_bytes: 1024,
            read_timeout: Duration::from_secs(2),
            write_timeout: Duration::from_secs(2),
        }
    }
}

/// Static diagnostics only; never wraps a payload-bearing I/O error.
#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum StatusError {
    #[error("invalid status host authority")]
    InvalidHost,
    #[error("invalid status limits")]
    InvalidLimits,
    #[error("status connection cancelled")]
    Cancelled,
    #[error("status read deadline expired")]
    ReadDeadline,
    #[error("status write deadline expired")]
    WriteDeadline,
    #[error("status I/O failed ({0:?})")]
    Io(io::ErrorKind),
}

impl From<io::Error> for StatusError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// Result of one bounded connection poll. Terminal results are stable on repoll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum StatusPoll {
    /// No complete response yet; the caller must poll again.
    Pending,
    /// One complete HTTP response was written (including a 400/401 rejection).
    /// This is not an authentication, readiness or delivery receipt.
    Finished,
    /// Closed without completing a response; already-written bytes remain sent.
    Failed(StatusError),
}

/// One owned, nonblocking STATUS exchange with a frozen snapshot.
///
/// Uses the same framing/authentication policy as [`handle_connection`]. There
/// is no listener, background work or thread. The caller must poll regularly,
/// cap its active connections and acceptance budget, and service all connections
/// fairly. Deadlines/cancellation are observed by polling, not by a timer task.
///
/// The caller must not concurrently use cloned descriptors or change socket
/// modes. Terminal polls and Drop shut down both directions, even if another
/// descriptor exists. Dropping also releases the owned descriptor.
///
/// A private 64-byte credential copy avoids borrowing the caller across polls;
/// no Clone or serialization is provided and Debug is redacted. Keep the
/// canonical RunCredential and its file alive until all connections/consumers
/// stop. This type never unlinks credentials and makes no secure-erasure claim.
pub struct StatusConnection {
    stream: CloseOnExit<TcpStream>,
    protocol: Protocol,
    shutdown: Shutdown,
    terminal: Option<StatusPoll>,
}

impl std::fmt::Debug for StatusConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StatusConnection([REDACTED])")
    }
}

impl StatusConnection {
    /// Validate host/limits, freeze the snapshot and start the absolute read
    /// deadline, then make the socket nonblocking. No read or write occurs.
    /// The write deadline starts only when a response is prepared.
    ///
    /// # Errors
    /// Invalid configuration or socket-mode failure consumes and shuts down
    /// the socket; validation precedes socket I/O apart from exit shutdown.
    pub fn new(
        stream: TcpStream,
        token: &SecretToken,
        expected_host: &str,
        snapshot: Snapshot,
        limits: Limits,
        shutdown: Shutdown,
    ) -> Result<Self, StatusError> {
        let stream = CloseOnExit(stream);
        let protocol = Protocol::new(token, expected_host, snapshot, limits)?;
        stream.0.set_nonblocking(true)?;
        Ok(Self {
            stream,
            protocol,
            shutdown,
            terminal: None,
        })
    }

    /// Perform at most one nonblocking read and one nonblocking write. Even
    /// Interrupted/WouldBlock yield to the caller; no sleeping or retry loop.
    /// Request storage is 8193 bytes, and response length never exceeds 1024.
    /// Deadlines never reset on progress. Shutdown is checked before each I/O.
    pub fn poll(&mut self) -> StatusPoll {
        if let Some(terminal) = self.terminal {
            return terminal;
        }
        let terminal = match self.protocol.poll(&mut self.stream.0, &self.shutdown) {
            Ok(Step::Pending | Step::Blocked(_)) => return StatusPoll::Pending,
            Ok(Step::Finished) => StatusPoll::Finished,
            Err(error) => StatusPoll::Failed(error),
        };
        self.terminal = Some(terminal);
        self.stream.close();
        terminal
    }
}

/// Handle one externally supervised, caller-owned connected socket.
///
/// `token` borrows the canonical credential from `RunCredential::token()`.
/// `expected_host` must be trusted host input: exactly `localhost`,
/// `host.docker.internal`, `[::1]`, or canonical IPv4 outside 0/8 and 224/3,
/// followed by `:` and a canonical decimal port in 1..=65535. It is not inferred
/// from the socket or request and does not itself grant authority.
///
/// Accepts only `GET /v1/status HTTP/1.1`, exactly one matching Host and one
/// `Authorization: Bearer <64 lowercase hex>` field. Field names are ASCII
/// case-insensitive; protected field values require exactly one leading space
/// and no trailing whitespace. No Origin, transfer encoding, body (except one
/// `Content-Length: 0`), query, obs-fold or control bytes are accepted.
///
/// A complete bounded head is required before status dispatch. Bytes already
/// read after its terminator cause rejection. Later bytes are never dispatched:
/// at most one response is sent with `Connection: close` and no CORS headers.
/// `Ok(())` means a complete response was written, including a 400/401 rejection;
/// it is **not** an authentication or readiness receipt.
///
/// The socket becomes nonblocking and is shut down in both directions on every
/// exit, even invalid host/limits, I/O failure or unwinding. The caller retains
/// the descriptor but must not reuse it, use cloned descriptors concurrently,
/// or perform concurrent I/O. No mode restoration or credential cleanup occurs.
/// Cancellation is cooperative; polling sleeps are at most 10 ms, not a
/// real-time scheduler guarantee. Already-written bytes cannot be recalled.
///
/// # Errors
/// Invalid host/limits fail before socket processing (apart from exit shutdown).
/// Cancellation/deadlines close without an error response; an I/O failure may
/// leave a partial response. Errors contain only static labels and I/O kinds.
pub fn handle_connection(
    stream: &mut TcpStream,
    token: &SecretToken,
    expected_host: &str,
    snapshot: Snapshot,
    limits: Limits,
    shutdown: &Shutdown,
) -> Result<(), StatusError> {
    let mut connection = CloseOnExit(&*stream);
    let mut protocol = Protocol::new(token, expected_host, snapshot, limits)?;
    connection.0.set_nonblocking(true)?;
    loop {
        match protocol.poll(&mut connection.0, shutdown)? {
            Step::Pending => {}
            Step::Blocked(deadline) => pause(deadline),
            Step::Finished => return Ok(()),
        }
    }
}

// No socket ownership, sleeping, acceptance or credential policy lives here.
// Both public drivers use this state and the same parser/write boundary.
struct Protocol {
    token: [u8; 64],
    expected_host: String,
    snapshot: Snapshot,
    limits: Limits,
    head: [u8; 8193],
    length: usize,
    read_deadline: Instant,
    response: Option<Response>,
}

struct Response {
    bytes: Vec<u8>,
    position: usize,
    deadline: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Pending,
    Blocked(Instant),
    Finished,
}

impl Protocol {
    fn new(
        token: &SecretToken,
        expected_host: &str,
        snapshot: Snapshot,
        limits: Limits,
    ) -> Result<Self, StatusError> {
        validate_host(expected_host)?;
        validate_limits(limits)?;
        // SecretToken can only be constructed by RunCredential with exactly
        // 64 canonical hex bytes. This private copy is never formatted/serialized.
        let mut secret = [0; 64];
        secret.copy_from_slice(token.expose_secret().as_bytes());
        Ok(Self {
            token: secret,
            expected_host: expected_host.to_owned(),
            snapshot,
            limits,
            head: [0; 8193],
            length: 0,
            read_deadline: Instant::now() + limits.read_timeout,
            response: None,
        })
    }

    // At most one read and one write, including Interrupted/WouldBlock. The
    // parser sees at most 8193 bytes; only a complete head/EOF/overflow dispatches.
    fn poll(
        &mut self,
        stream: &mut (impl Read + Write),
        shutdown: &Shutdown,
    ) -> Result<Step, StatusError> {
        if self.response.is_none() {
            if shutdown.is_requested() {
                return Err(StatusError::Cancelled);
            }
            if Instant::now() >= self.read_deadline {
                return Err(StatusError::ReadDeadline);
            }
            let count =
                match stream.read(&mut self.head[self.length..self.limits.max_header_bytes + 1]) {
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        return Ok(Step::Blocked(self.read_deadline));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        return Ok(Step::Pending);
                    }
                    Err(error) => return Err(error.into()),
                };
            self.length += count;
            if count != 0
                && self.length <= self.limits.max_header_bytes
                && !self.head[..self.length]
                    .windows(4)
                    .any(|w| w == b"\r\n\r\n")
            {
                return Ok(Step::Pending);
            }
            if shutdown.is_requested() {
                return Err(StatusError::Cancelled);
            }
            let decision = if self.length > self.limits.max_header_bytes {
                Err(Rejection::BadRequest)
            } else {
                parse_head(
                    &self.head[..self.length],
                    &self.token,
                    &self.expected_host,
                    self.limits.max_header_fields,
                )
            };
            let bytes = response_bytes(decision, self.snapshot);
            // Every fixed-schema response is <=256 bytes; also enforce the
            // configured cap here so future schema changes cannot bypass it.
            if bytes.len() > self.limits.max_response_bytes {
                return Err(StatusError::Io(io::ErrorKind::InvalidData));
            }
            self.response = Some(Response {
                bytes,
                position: 0,
                deadline: Instant::now() + self.limits.write_timeout,
            });
        }
        let response = self.response.as_mut().expect("response prepared above");
        poll_write(
            stream,
            &response.bytes,
            &mut response.position,
            response.deadline,
            shutdown,
        )
    }
}

fn response_bytes(decision: Result<(), Rejection>, snapshot: Snapshot) -> Vec<u8> {
    let (status, body) = match decision {
        Ok(()) => {
            let phase = match snapshot.phase {
                Phase::Preparing => "preparing",
                Phase::Ready => "ready",
                Phase::Stopping => "stopping",
                Phase::RecoveryRequired => "recovery_required",
            };
            (
                "200 OK",
                format!("{{\"version\":1,\"phase\":\"{phase}\"}}\n"),
            )
        }
        Err(Rejection::BadRequest) => (
            "400 Bad Request",
            "{\"error\":\"bad_request\"}\n".to_owned(),
        ),
        Err(Rejection::Unauthorized) => (
            "401 Unauthorized",
            "{\"error\":\"unauthorized\"}\n".to_owned(),
        ),
    };
    let challenge = if decision == Err(Rejection::Unauthorized) {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    format!(
        "HTTP/1.1 {status}\r\n{challenge}Content-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ).into_bytes()
}

fn poll_write(
    writer: &mut impl Write,
    response: &[u8],
    position: &mut usize,
    deadline: Instant,
    shutdown: &Shutdown,
) -> Result<Step, StatusError> {
    if shutdown.is_requested() {
        return Err(StatusError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(StatusError::WriteDeadline);
    }
    match writer.write(&response[*position..]) {
        Ok(0) => return Err(StatusError::Io(io::ErrorKind::WriteZero)),
        Ok(count) => *position += count,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Ok(Step::Blocked(deadline));
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
        Err(error) => return Err(error.into()),
    }
    Ok(if *position == response.len() {
        Step::Finished
    } else {
        Step::Pending
    })
}

// Keep the original write-boundary regression tests driving the shared step.
#[cfg(test)]
fn write_response(
    writer: &mut impl Write,
    response: &[u8],
    deadline: Instant,
    shutdown: &Shutdown,
) -> Result<(), StatusError> {
    let mut position = 0;
    loop {
        match poll_write(writer, response, &mut position, deadline, shutdown)? {
            Step::Pending => {}
            Step::Blocked(deadline) => pause(deadline),
            Step::Finished => return Ok(()),
        }
    }
}

fn pause(deadline: Instant) {
    std::thread::sleep(
        Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
    );
}

fn validate_limits(limits: Limits) -> Result<(), StatusError> {
    if !(4..=8192).contains(&limits.max_header_bytes)
        || !(1..=64).contains(&limits.max_header_fields)
        || !(256..=1024).contains(&limits.max_response_bytes)
        || limits.read_timeout.is_zero()
        || limits.read_timeout > Duration::from_secs(60)
        || limits.write_timeout.is_zero()
        || limits.write_timeout > Duration::from_secs(60)
    {
        return Err(StatusError::InvalidLimits);
    }
    Ok(())
}

// Same narrow authority syntax as RunCredential's HTTP endpoint. Validation
// proves syntax only, not ownership, routing, binding or secure transport.
fn validate_host(authority: &str) -> Result<(), StatusError> {
    let valid = || {
        if authority.len() > 57 {
            return None;
        }
        let (host, port) = authority.rsplit_once(':')?;
        if port.is_empty()
            || port.len() > 5
            || port.starts_with('0')
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().ok()? == 0
        {
            return None;
        }
        if !matches!(host, "localhost" | "host.docker.internal" | "[::1]") {
            let address = host.parse::<std::net::Ipv4Addr>().ok()?;
            if address.octets()[0] == 0 || address.octets()[0] >= 224 {
                return None;
            }
        }
        Some(())
    };
    valid().ok_or(StatusError::InvalidHost)
}

struct CloseOnExit<S: Borrow<TcpStream>>(S);

impl<S: Borrow<TcpStream>> CloseOnExit<S> {
    fn close(&self) {
        // Separate halves: after a peer FIN, macOS fails `Both` with ENOTCONN
        // and never sends our FIN, leaving the client waiting for EOF.
        let _ = self.0.borrow().shutdown(SocketShutdown::Write);
        let _ = self.0.borrow().shutdown(SocketShutdown::Read);
    }
}

impl<S: Borrow<TcpStream>> Drop for CloseOnExit<S> {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    BadRequest,
    Unauthorized,
}

fn parse_head(head: &[u8], token: &[u8], host: &str, max_fields: usize) -> Result<(), Rejection> {
    if head.len() > 8192 {
        return Err(Rejection::BadRequest);
    }
    let text = std::str::from_utf8(head).map_err(|_| Rejection::BadRequest)?;
    let text = text.strip_suffix("\r\n\r\n").ok_or(Rejection::BadRequest)?;
    let mut lines = text.split("\r\n");
    if lines.next() != Some("GET /v1/status HTTP/1.1") {
        return Err(Rejection::BadRequest);
    }
    let mut authorization = None;
    let mut authority = None;
    let mut content_length = false;
    for (index, line) in lines.enumerate() {
        if index >= max_fields.min(64) {
            return Err(Rejection::BadRequest);
        }
        if !line.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return Err(Rejection::BadRequest);
        }
        let (name, value) = line.split_once(':').ok_or(Rejection::BadRequest)?;
        if name.is_empty() || !name.bytes().all(is_field_name_byte) {
            return Err(Rejection::BadRequest);
        }
        if name.eq_ignore_ascii_case("origin") || name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Rejection::BadRequest);
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length || value != " 0" {
                return Err(Rejection::BadRequest);
            }
            content_length = true;
        }
        if name.eq_ignore_ascii_case("authorization") && authorization.replace(value).is_some() {
            return Err(Rejection::Unauthorized);
        }
        if name.eq_ignore_ascii_case("host") && authority.replace(value).is_some() {
            return Err(Rejection::BadRequest);
        }
    }
    if authority.and_then(|value| value.strip_prefix(' ')) != Some(host) {
        return Err(Rejection::BadRequest);
    }
    let candidate = authorization
        .and_then(|value| value.strip_prefix(" Bearer "))
        .ok_or(Rejection::Unauthorized)?;
    if candidate.len() != 64 || !candidate.bytes().all(is_lower_hex) || token.len() != 64 {
        return Err(Rejection::Unauthorized);
    }
    // Fixed 64-byte work with no mismatch-dependent exit. black_box discourages
    // replacement with an early-exit equality; this is not a formal compiler or
    // hardware constant-time guarantee.
    let mut difference = 0u8;
    for (&candidate_byte, &expected_byte) in candidate.as_bytes().iter().zip(token) {
        difference |= std::hint::black_box(candidate_byte ^ expected_byte);
    }
    if std::hint::black_box(difference) != 0 {
        return Err(Rejection::Unauthorized);
    }
    Ok(())
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'a'..=b'f')
}

fn is_field_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Vec<u8> {
        format!(
            "GET /v1/status HTTP/1.1\r\nHost: localhost:43127\r\nAuthorization: Bearer {}\r\n\r\n",
            "a".repeat(64)
        )
        .into_bytes()
    }

    fn parse(head: &[u8]) -> Result<(), Rejection> {
        parse_head(head, &[b'a'; 64], "localhost:43127", 64)
    }

    // Control only the I/O boundary: short reads/writes and retryable errors
    // cannot be induced deterministically with a <=256-byte TCP response.
    struct FragmentedIo {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
        reads: usize,
        writes: usize,
    }

    impl Read for FragmentedIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            match self.reads % 4 {
                0 => Err(io::ErrorKind::Interrupted.into()),
                1 => Err(io::ErrorKind::WouldBlock.into()),
                _ => self.input.read(&mut buffer[..1]),
            }
        }
    }

    impl Write for FragmentedIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            match self.writes % 4 {
                0 => Err(io::ErrorKind::Interrupted.into()),
                1 => Err(io::ErrorKind::WouldBlock.into()),
                _ => {
                    self.output.push(buffer[0]);
                    Ok(1)
                }
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("the protocol must not flush a nonblocking socket")
        }
    }

    #[test]
    fn each_protocol_step_has_one_read_write_budget_even_when_interrupted() {
        for cancel_after_partial_write in [false, true] {
            let mut protocol = Protocol {
                token: [b'a'; 64],
                expected_host: "localhost:43127".to_owned(),
                snapshot: Snapshot {
                    phase: Phase::Ready,
                },
                limits: Limits::default(),
                head: [0; 8193],
                length: 0,
                read_deadline: Instant::now() + Duration::from_secs(2),
                response: None,
            };
            let mut socket = FragmentedIo {
                input: io::Cursor::new(request()),
                output: Vec::new(),
                reads: 0,
                writes: 0,
            };
            let shutdown = Shutdown::new();
            let mut completed = false;
            for _ in 0..4096 {
                let (reads, writes) = (socket.reads, socket.writes);
                let step = protocol.poll(&mut socket, &shutdown);
                assert!(socket.reads - reads <= 1);
                assert!(socket.writes - writes <= 1);
                if shutdown.is_requested() {
                    assert_eq!(step, Err(StatusError::Cancelled));
                    assert_eq!(socket.output.len(), 1);
                    assert_eq!((socket.reads, socket.writes), (reads, writes));
                    completed = true;
                    break;
                }
                if step == Ok(Step::Finished) {
                    let text = std::str::from_utf8(&socket.output).unwrap();
                    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
                    assert!(text.ends_with("{\"version\":1,\"phase\":\"ready\"}\n"));
                    let (headers, body) = text.split_once("\r\n\r\n").unwrap();
                    assert!(headers.contains(&format!("Content-Length: {}", body.len())));
                    completed = true;
                    break;
                }
                assert!(matches!(step, Ok(Step::Pending | Step::Blocked(_))));
                if cancel_after_partial_write && !socket.output.is_empty() {
                    shutdown.request(crate::lifecycle::ShutdownReason::Requested);
                }
            }
            assert!(completed, "fixture step cap exceeded");
        }
    }

    #[test]
    fn io_errors_keep_only_kind_and_write_zero_is_not_success() {
        use std::error::Error;
        let error = StatusError::from(io::Error::other("synthetic-private-marker"));
        assert_eq!(error, StatusError::Io(io::ErrorKind::Other));
        assert_eq!(error.to_string(), "status I/O failed (Other)");
        assert_eq!(format!("{error:?}"), "Io(Other)");
        assert!(error.source().is_none());
        assert_eq!(
            write_response(
                &mut &mut [][..],
                b"response",
                Instant::now() + Duration::from_secs(1),
                &Shutdown::new()
            ),
            Err(StatusError::Io(io::ErrorKind::WriteZero))
        );
    }

    #[test]
    fn cancellation_during_partial_write_stops_remaining_output() {
        struct CancellingWriter<'a>(&'a Shutdown, usize);
        impl Write for CancellingWriter<'_> {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.1 += 1;
                self.0.request(crate::lifecycle::ShutdownReason::Requested);
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let shutdown = Shutdown::new();
        let mut writer = CancellingWriter(&shutdown, 0);
        assert_eq!(
            write_response(
                &mut writer,
                &[0; 16],
                Instant::now() + Duration::from_secs(1),
                &shutdown
            ),
            Err(StatusError::Cancelled)
        );
        assert_eq!(writer.1, 1);
    }

    #[test]
    fn partial_and_blocked_writes_keep_one_absolute_deadline() {
        struct TrickleWriter {
            calls: usize,
            written: usize,
        }
        impl Write for TrickleWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                match self.calls % 3 {
                    0 => {
                        self.written += 1;
                        Ok(1)
                    }
                    1 => Err(io::ErrorKind::Interrupted.into()),
                    _ => Err(io::ErrorKind::WouldBlock.into()),
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = TrickleWriter {
            calls: 0,
            written: 0,
        };
        let start = Instant::now();
        assert_eq!(
            write_response(
                &mut writer,
                &[0; 1024],
                start + Duration::from_millis(50),
                &Shutdown::new()
            ),
            Err(StatusError::WriteDeadline)
        );
        assert!(writer.written > 0);
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(start.elapsed() < Duration::from_millis(300));
    }

    #[test]
    fn head_bytes_and_field_count_are_bounded_inclusively() {
        let good = String::from_utf8(request()).unwrap();
        let padded = good.replace(
            "\r\n\r\n",
            &format!("\r\nX: {}\r\n\r\n", "x".repeat(8192 - good.len() - 5)),
        );
        assert_eq!(padded.len(), 8192);
        assert_eq!(parse(padded.as_bytes()), Ok(()));
        assert_eq!(
            parse(padded.replacen("X: ", "X: x", 1).as_bytes()),
            Err(Rejection::BadRequest)
        );
        let fields = good.replace("\r\n\r\n", &format!("\r\n{}\r\n", "X: x\r\n".repeat(62)));
        assert_eq!(parse(fields.as_bytes()), Ok(()));
        assert_eq!(
            parse(fields.replace("\r\n\r\n", "\r\nX: x\r\n\r\n").as_bytes()),
            Err(Rejection::BadRequest)
        );
        assert_eq!(
            parse_head(&request(), &[b'a'; 64], "localhost:43127", 1),
            Err(Rejection::BadRequest)
        );
    }

    #[test]
    fn origin_and_body_framing_are_forbidden_except_single_zero_length() {
        let good = String::from_utf8(request()).unwrap();
        for field in [
            "Origin: null",
            "oRiGiN:",
            "Transfer-Encoding: chunked",
            "TRANSFER-ENCODING:",
            "Content-Length: 1",
            "Content-Length: -0",
            "Content-Length: 00",
            "Content-Length: 0, 0",
            "Content-Length: 0 ",
            "Content-Length: 0\r\ncontent-length: 0",
        ] {
            let head = good.replace("\r\n\r\n", &format!("\r\n{field}\r\n\r\n"));
            assert_eq!(parse(head.as_bytes()), Err(Rejection::BadRequest));
        }
        let head = good.replace("\r\n\r\n", "\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(parse(head.as_bytes()), Ok(()));
    }

    #[test]
    fn exactly_one_exact_host_is_required() {
        let good = String::from_utf8(request()).unwrap();
        for replacement in [
            "",
            "Host: other:43127\r\n",
            "Host: LOCALHOST:43127\r\n",
            "Host: localhost:43127 \r\n",
            "Host:  localhost:43127\r\n",
            "Host: localhost:43127\r\nhost: localhost:43127\r\n",
        ] {
            assert_eq!(
                parse(
                    good.replace("Host: localhost:43127\r\n", replacement)
                        .as_bytes()
                ),
                Err(Rejection::BadRequest)
            );
        }
    }

    #[test]
    fn exactly_one_canonical_matching_bearer_is_required() {
        let auth = format!("Authorization: Bearer {}\r\n", "a".repeat(64));
        let good = String::from_utf8(request()).unwrap();
        for replacement in [
            String::new(),
            format!("Authorization: Bearer {}\r\n", "b".repeat(64)),
            format!("Authorization: bearer {}\r\n", "a".repeat(64)),
            format!("Authorization: Bearer {}\r\n", "A".repeat(64)),
            format!("Authorization: Bearer {}\r\n", "a".repeat(63)),
            format!("Authorization: Bearer {}\r\n", "a".repeat(65)),
            format!("Authorization:  Bearer {}\r\n", "a".repeat(64)),
            format!("Authorization: Bearer {} \r\n", "a".repeat(64)),
            format!("Authorization: Basic {}\r\n", "a".repeat(64)),
            format!("{auth}{auth}"),
            format!("{auth}authorization: Bearer {}\r\n", "b".repeat(64)),
        ] {
            assert_eq!(
                parse(good.replace(&auth, &replacement).as_bytes()),
                Err(Rejection::Unauthorized)
            );
        }
        for index in 0..64 {
            let mut wrong = [b'a'; 64];
            wrong[index] = b'b';
            assert_eq!(
                parse_head(&request(), &wrong, "localhost:43127", 64),
                Err(Rejection::Unauthorized)
            );
        }
    }

    #[test]
    fn malformed_framing_and_field_syntax_are_rejected() {
        let good = request();
        let mut cases = vec![good[..good.len() - 2].to_vec()];
        for tail in [b"extra".as_slice(), b"GET /v1/status HTTP/1.1\r\n\r\n"] {
            let mut head = good.clone();
            head.extend_from_slice(tail);
            cases.push(head);
        }
        for field in [
            b"NoColon".as_slice(),
            b"Bad Name: x",
            b"Host : x",
            b": x",
            b" continuation",
            b"\tcontinuation",
            b"X: a\tb",
            b"X: a\0b",
            b"X: a\x7fb",
            b"X: a\xffb",
            b"X: a\nb",
            b"X: a\rb",
            b"X(@): a",
            b"X: a\r\n\r\nX: b",
        ] {
            let mut head = good[..good.len() - 2].to_vec();
            head.extend_from_slice(field);
            head.extend_from_slice(b"\r\n\r\n");
            cases.push(head);
        }
        for head in cases {
            assert_eq!(parse(&head), Err(Rejection::BadRequest));
        }
    }

    #[test]
    fn only_exact_request_line_is_supported() {
        for line in [
            "POST /v1/status HTTP/1.1",
            "GET /v1/status HTTP/1.0",
            "GET /v1/status HTTP/2",
            "GET /v1/status?token=synthetic HTTP/1.1",
            "GET /v1/status/ HTTP/1.1",
            "GET http://localhost:43127/v1/status HTTP/1.1",
            "GET  /v1/status HTTP/1.1",
            "GET /v1/status HTTP/1.1 ",
        ] {
            let head =
                String::from_utf8(request())
                    .unwrap()
                    .replacen("GET /v1/status HTTP/1.1", line, 1);
            assert_eq!(parse(head.as_bytes()), Err(Rejection::BadRequest));
        }
    }

    #[test]
    fn canonical_complete_status_head_is_accepted() {
        assert_eq!(parse(&request()), Ok(()));
    }
}

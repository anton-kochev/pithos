//! Workspace route protocol: typed requests over the status framing.
//!
//! Same Host and Bearer rules as `status`, plus `POST /v1/apps/<op>` with a
//! strict JSON body. A connection yields at most one authenticated request;
//! the owner answers it with [`ApiConnection::respond`]. No Docker authority
//! lives here, and a parsed request is not a grant check.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use super::{
    credential::SecretToken,
    status::{
        CloseOnExit, Snapshot, StatusError, Step, bearer_matches, is_field_name_byte, poll_write,
        status_response, validate_host,
    },
};
use crate::lifecycle::Shutdown;
use serde::Deserialize;
use std::{
    io::{self, Read},
    net::TcpStream,
    time::{Duration, Instant},
};

const MAX_HEAD: usize = 8192;
const MAX_FIELDS: usize = 64;
const MAX_BODY: usize = 16 * 1024;
const MAX_RESPONSE: usize = 256 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_TAIL: u16 = 100;

/// A parsed, authenticated request. Values are syntactically bounded only;
/// the runtime still applies the grant and every owner check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiRequest {
    Status,
    Build {
        request_id: String,
        app: String,
        dockerfile: String,
        context: String,
    },
    Run {
        request_id: String,
        app: String,
        command: Vec<String>,
    },
    AppStatus {
        app: String,
    },
    Logs {
        app: String,
        tail: u16,
    },
    Stop {
        request_id: String,
        app: String,
    },
}

#[derive(Debug)]
pub enum ApiPoll {
    Pending,
    /// Answer with [`ApiConnection::respond`], then keep polling.
    Request(ApiRequest),
    /// A complete response was written (including a 400/401 rejection).
    Finished,
    Failed(StatusError),
}

enum State {
    Reading,
    Awaiting,
    Writing {
        bytes: Vec<u8>,
        position: usize,
        deadline: Instant,
    },
    Done(Result<(), StatusError>),
}

/// One owned, nonblocking exchange. Poll regularly; nothing runs in the
/// background. Debug is redacted; the token copy is never formatted.
pub struct ApiConnection {
    stream: CloseOnExit<TcpStream>,
    token: [u8; 64],
    host: String,
    shutdown: Shutdown,
    buffer: Vec<u8>,
    read_deadline: Instant,
    state: State,
}

impl std::fmt::Debug for ApiConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiConnection([REDACTED])")
    }
}

#[derive(Clone, Copy)]
enum Route {
    Status,
    Build,
    Run,
    AppStatus,
    Logs,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    BadRequest,
    Unauthorized,
}

impl ApiConnection {
    pub fn new(
        stream: TcpStream,
        token: &SecretToken,
        expected_host: &str,
        shutdown: Shutdown,
    ) -> Result<Self, StatusError> {
        let stream = CloseOnExit(stream);
        validate_host(expected_host)?;
        let mut secret = [0; 64];
        secret.copy_from_slice(token.expose_secret().as_bytes());
        stream.0.set_nonblocking(true)?;
        Ok(Self {
            stream,
            token: secret,
            host: expected_host.to_owned(),
            shutdown,
            buffer: Vec::new(),
            read_deadline: Instant::now() + READ_TIMEOUT,
            state: State::Reading,
        })
    }

    /// At most one read or one write per call.
    pub fn poll(&mut self) -> ApiPoll {
        match &mut self.state {
            State::Done(Ok(())) => ApiPoll::Finished,
            State::Done(Err(error)) => ApiPoll::Failed(*error),
            State::Awaiting => ApiPoll::Pending,
            State::Reading => match self.read() {
                Ok(None) => ApiPoll::Pending,
                Ok(Some(Ok(request))) => {
                    self.state = State::Awaiting;
                    ApiPoll::Request(request)
                }
                Ok(Some(Err(rejection))) => {
                    self.prepare(rejection_response(rejection));
                    ApiPoll::Pending
                }
                Err(error) => self.finish(Err(error)),
            },
            State::Writing {
                bytes,
                position,
                deadline,
            } => match poll_write(
                &mut self.stream.0,
                bytes,
                position,
                *deadline,
                &self.shutdown,
            ) {
                Ok(Step::Pending | Step::Blocked(_)) => ApiPoll::Pending,
                Ok(Step::Finished) => self.finish(Ok(())),
                Err(error) => self.finish(Err(error)),
            },
        }
    }

    /// Answer the surfaced request. Ignored in any other state. An oversized
    /// body becomes a 500 rather than a partial response.
    pub fn respond(&mut self, status: u16, body: &serde_json::Value) {
        if matches!(self.state, State::Awaiting) {
            let mut bytes = response(status, &format!("{body}\n"), false);
            if bytes.len() > MAX_RESPONSE {
                bytes = response(500, "{\"error\":\"response_too_large\"}\n", false);
            }
            self.prepare(bytes);
        }
    }

    /// Answer a status request with exactly the status protocol's body.
    pub fn respond_status(&mut self, snapshot: Snapshot) {
        if matches!(self.state, State::Awaiting) {
            self.prepare(status_response(snapshot));
        }
    }

    fn prepare(&mut self, bytes: Vec<u8>) {
        self.state = State::Writing {
            bytes,
            position: 0,
            deadline: Instant::now() + WRITE_TIMEOUT,
        };
    }

    fn finish(&mut self, result: Result<(), StatusError>) -> ApiPoll {
        self.stream.close();
        self.state = State::Done(result);
        match result {
            Ok(()) => ApiPoll::Finished,
            Err(error) => ApiPoll::Failed(error),
        }
    }

    /// `None` while more bytes are needed; otherwise the parse decision.
    fn read(&mut self) -> Result<Option<Result<ApiRequest, Rejection>>, StatusError> {
        if self.shutdown.is_requested() {
            return Err(StatusError::Cancelled);
        }
        if Instant::now() >= self.read_deadline {
            return Err(StatusError::ReadDeadline);
        }
        let mut chunk = [0; 4096];
        let room = (MAX_HEAD + MAX_BODY + 1).saturating_sub(self.buffer.len());
        let count = match self.stream.0.read(&mut chunk[..room.clamp(1, 4096)]) {
            Ok(count) => count,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        self.buffer.extend_from_slice(&chunk[..count]);
        let Some(end) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            if count == 0 || self.buffer.len() > MAX_HEAD {
                return Ok(Some(Err(Rejection::BadRequest)));
            }
            return Ok(None);
        };
        let head_len = end + 4;
        if head_len > MAX_HEAD {
            return Ok(Some(Err(Rejection::BadRequest)));
        }
        let (route, length) = match parse_head(&self.buffer[..head_len], &self.token, &self.host) {
            Ok(parsed) => parsed,
            Err(rejection) => return Ok(Some(Err(rejection))),
        };
        let total = head_len + length;
        if self.buffer.len() < total {
            return Ok(if count == 0 {
                Some(Err(Rejection::BadRequest))
            } else {
                None
            });
        }
        // Bytes past the declared body (pipelining, smuggling) are refused.
        if self.buffer.len() > total {
            return Ok(Some(Err(Rejection::BadRequest)));
        }
        Ok(Some(parse_body(route, &self.buffer[head_len..])))
    }
}

fn parse_head(head: &[u8], token: &[u8], host: &str) -> Result<(Route, usize), Rejection> {
    let text = std::str::from_utf8(head).map_err(|_| Rejection::BadRequest)?;
    let text = text.strip_suffix("\r\n\r\n").ok_or(Rejection::BadRequest)?;
    let mut lines = text.split("\r\n");
    let route = match lines.next() {
        Some("GET /v1/status HTTP/1.1") => Route::Status,
        Some("POST /v1/apps/build HTTP/1.1") => Route::Build,
        Some("POST /v1/apps/run HTTP/1.1") => Route::Run,
        Some("POST /v1/apps/status HTTP/1.1") => Route::AppStatus,
        Some("POST /v1/apps/logs HTTP/1.1") => Route::Logs,
        Some("POST /v1/apps/stop HTTP/1.1") => Route::Stop,
        _ => return Err(Rejection::BadRequest),
    };
    let mut authorization = None;
    let mut authority = None;
    let mut length = None;
    let mut content_type = None;
    for (index, line) in lines.enumerate() {
        if index >= MAX_FIELDS || !line.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return Err(Rejection::BadRequest);
        }
        let (name, value) = line.split_once(':').ok_or(Rejection::BadRequest)?;
        if name.is_empty() || !name.bytes().all(is_field_name_byte) {
            return Err(Rejection::BadRequest);
        }
        if name.eq_ignore_ascii_case("origin") || name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Rejection::BadRequest);
        }
        let slot = if name.eq_ignore_ascii_case("content-length") {
            &mut length
        } else if name.eq_ignore_ascii_case("content-type") {
            &mut content_type
        } else if name.eq_ignore_ascii_case("host") {
            &mut authority
        } else if name.eq_ignore_ascii_case("authorization") {
            if authorization.replace(value).is_some() {
                return Err(Rejection::Unauthorized);
            }
            continue;
        } else {
            continue;
        };
        if slot.replace(value).is_some() {
            return Err(Rejection::BadRequest);
        }
    }
    if authority.and_then(|v| v.strip_prefix(' ')) != Some(host) {
        return Err(Rejection::BadRequest);
    }
    let length = match length.map(|v| v.strip_prefix(' ')) {
        None => 0,
        Some(Some(digits))
            if !digits.is_empty()
                && digits.len() <= 5
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0')) =>
        {
            digits.parse::<usize>().map_err(|_| Rejection::BadRequest)?
        }
        Some(_) => return Err(Rejection::BadRequest),
    };
    match route {
        Route::Status if length != 0 || content_type.is_some() => {
            return Err(Rejection::BadRequest);
        }
        Route::Status => {}
        _ if length == 0 || length > MAX_BODY || content_type != Some(" application/json") => {
            return Err(Rejection::BadRequest);
        }
        _ => {}
    }
    let candidate = authorization
        .and_then(|v| v.strip_prefix(" Bearer "))
        .ok_or(Rejection::Unauthorized)?;
    if !bearer_matches(candidate, token) {
        return Err(Rejection::Unauthorized);
    }
    Ok((route, length))
}

/// Client request IDs leave room for the broker's own prefix within the
/// journal's 64-byte identity limit.
fn request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

fn parse_body(route: Route, body: &[u8]) -> Result<ApiRequest, Rejection> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Build {
        request_id: String,
        app: String,
        dockerfile: String,
        context: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Run {
        request_id: String,
        app: String,
        #[serde(default)]
        command: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct App {
        app: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Logs {
        app: String,
        tail: Option<u16>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Stop {
        request_id: String,
        app: String,
    }
    let bad = |_| Rejection::BadRequest;
    let request = match route {
        Route::Status => ApiRequest::Status,
        Route::Build => {
            let b: Build = serde_json::from_slice(body).map_err(bad)?;
            ApiRequest::Build {
                request_id: b.request_id,
                app: b.app,
                dockerfile: b.dockerfile,
                context: b.context,
            }
        }
        Route::Run => {
            let r: Run = serde_json::from_slice(body).map_err(bad)?;
            ApiRequest::Run {
                request_id: r.request_id,
                app: r.app,
                command: r.command,
            }
        }
        Route::AppStatus => ApiRequest::AppStatus {
            app: serde_json::from_slice::<App>(body).map_err(bad)?.app,
        },
        Route::Logs => {
            let l: Logs = serde_json::from_slice(body).map_err(bad)?;
            ApiRequest::Logs {
                app: l.app,
                tail: l.tail.unwrap_or(DEFAULT_TAIL),
            }
        }
        Route::Stop => {
            let s: Stop = serde_json::from_slice(body).map_err(bad)?;
            ApiRequest::Stop {
                request_id: s.request_id,
                app: s.app,
            }
        }
    };
    let valid = match &request {
        ApiRequest::Status => true,
        ApiRequest::Build {
            request_id: id,
            app,
            dockerfile,
            context,
        } => request_id(id) && [app, dockerfile, context].iter().all(|v| bounded(v)),
        ApiRequest::Run {
            request_id: id,
            app,
            command,
        } => {
            request_id(id)
                && bounded(app)
                && command.len() <= 32
                && command.iter().all(|v| bounded(v))
        }
        ApiRequest::AppStatus { app } | ApiRequest::Logs { app, .. } => bounded(app),
        ApiRequest::Stop {
            request_id: id,
            app,
        } => request_id(id) && bounded(app),
    };
    valid.then_some(request).ok_or(Rejection::BadRequest)
}

fn rejection_response(rejection: Rejection) -> Vec<u8> {
    match rejection {
        Rejection::BadRequest => response(400, "{\"error\":\"bad_request\"}\n", false),
        Rejection::Unauthorized => response(401, "{\"error\":\"unauthorized\"}\n", true),
    }
}

fn response(status: u16, body: &str, challenge: bool) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Content",
        503 => "Service Unavailable",
        _ => return response(500, "{\"error\":\"internal\"}\n", false),
    };
    let challenge = if challenge {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\n{challenge}Content-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

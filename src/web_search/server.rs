use super::provider::SearchService;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::net::{Ipv4Addr, TcpListener as Listener, TcpStream as Stream};
#[cfg(target_os = "linux")]
use std::{
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener as Listener, UnixStream as Stream},
        },
    },
    path::PathBuf,
};

const MAX_REQUEST_BYTES: usize = 4 * 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 4;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct SearchBroker {
    token: Arc<str>,
    stopped: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    #[cfg(target_os = "linux")]
    _directory: tempfile::TempDir,
    #[cfg(target_os = "linux")]
    socket: PathBuf,
    #[cfg(target_os = "macos")]
    port: u16,
}

impl SearchBroker {
    pub(super) fn start(service: Box<SearchService>, socket_parent: &Path) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let (listener, directory, socket) = {
            let directory = tempfile::Builder::new()
                .prefix("pithos-web-")
                .tempdir_in(socket_parent)?;
            let socket = directory.path().join("search.sock");
            // AF_UNIX limits the address to 107 bytes on Linux. Bind through the
            // directory fd; Docker gets the real path to the same socket inode.
            let parent = std::fs::File::open(directory.path())?;
            let listener =
                Listener::bind(format!("/proc/self/fd/{}/search.sock", parent.as_raw_fd()))?;
            // Ordinary Pi and the host may have different UIDs. The parent is
            // private on the host; Docker exposes only the socket inode to Pi.
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
            (listener, directory, socket)
        };
        #[cfg(target_os = "macos")]
        let (listener, port) = {
            let _ = socket_parent;
            // Docker Desktop forwards host.docker.internal to the host. Do not
            // listen on an external interface when only the VM needs access.
            let listener = Listener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let port = listener.local_addr()?.port();
            (listener, port)
        };
        listener.set_nonblocking(true)?;
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| io::Error::other("search bearer randomness unavailable"))?;
        let token: Arc<str> = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .into();
        let stopped = Arc::new(AtomicBool::new(false));
        let (server_token, server_stop) = (Arc::clone(&token), Arc::clone(&stopped));
        let acceptor = thread::Builder::new()
            .name("pithos-web-search".into())
            .spawn(move || serve(listener, service.into(), server_token, server_stop))?;
        Ok(Self {
            token,
            stopped,
            acceptor: Some(acceptor),
            #[cfg(target_os = "linux")]
            _directory: directory,
            #[cfg(target_os = "linux")]
            socket,
            #[cfg(target_os = "macos")]
            port,
        })
    }

    pub(super) fn token(&self) -> &str {
        &self.token
    }

    #[cfg(target_os = "linux")]
    pub(super) fn socket_path(&self) -> &Path {
        &self.socket
    }

    #[cfg(target_os = "macos")]
    pub(super) fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for SearchBroker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Join only the listener. An outbound HTTPS call may already be in
        // flight; its worker cannot accept more work or write a reply now.
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
    }
}

fn serve(
    listener: Listener,
    service: Arc<SearchService>,
    token: Arc<str>,
    stopped: Arc<AtomicBool>,
) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !stopped.load(Ordering::Acquire) {
        workers.retain(|worker| !worker.is_finished());
        if workers.len() >= MAX_CONNECTIONS {
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let (service, token, stopped) = (
                    Arc::clone(&service),
                    Arc::clone(&token),
                    Arc::clone(&stopped),
                );
                if let Ok(worker) =
                    thread::Builder::new()
                        .name("pithos-web-call".into())
                        .spawn(move || {
                            let _ = exchange(stream, &service, &token, &stopped);
                        })
                {
                    workers.push(worker);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status {
        version: u8,
        token: String,
    },
    Search {
        version: u8,
        token: String,
        query: String,
        #[serde(default = "default_count")]
        count: usize,
    },
}

fn default_count() -> usize {
    5
}

fn error(code: &str) -> Value {
    json!({ "ok": false, "error": code })
}

fn exchange(
    mut stream: Stream,
    service: &SearchService,
    token: &str,
    stopped: &AtomicBool,
) -> io::Result<()> {
    // Accepted descriptors can inherit nonblocking mode (e.g. on macOS).
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let request = read_request(&mut stream)?;
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let response = match request {
        Some(Request::Status {
            version: 1,
            token: supplied,
        }) if token_matches(token, &supplied) => {
            json!({ "ok": true, "provider": service.provider() })
        }
        Some(Request::Search {
            version: 1,
            token: supplied,
            query,
            count,
        }) if token_matches(token, &supplied) => {
            let query = query.trim();
            if !(1..=10).contains(&count) || !valid_query(query) {
                error("invalid_query")
            } else {
                match service.search(query, count) {
                    Ok(results) => {
                        json!({ "ok": true, "provider": service.provider(), "results": results })
                    }
                    Err(failure) => error(failure.code()),
                }
            }
        }
        Some(Request::Status { version: 1, .. } | Request::Search { version: 1, .. }) => {
            error("unauthorized")
        }
        _ => error("invalid_request"),
    };
    if !stopped.load(Ordering::Acquire) {
        let mut frame = serde_json::to_vec(&response)?;
        if frame.len() >= MAX_REPLY_BYTES {
            frame = serde_json::to_vec(&error("invalid_response"))?;
        }
        frame.push(b'\n');
        stream.write_all(&frame)?;
    }
    Ok(())
}

fn read_request(stream: &mut Stream) -> io::Result<Option<Request>> {
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut frame = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 || frame.len() + n > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        frame.extend_from_slice(&chunk[..n]);
        if let Some(newline) = chunk[..n].iter().position(|b| *b == b'\n') {
            if newline + 1 != n {
                return Ok(None);
            }
            let _ = frame.pop();
            return Ok(serde_json::from_slice(&frame).ok());
        }
    }
}

fn token_matches(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len()
        && expected
            .bytes()
            .zip(supplied.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

fn valid_query(query: &str) -> bool {
    !query.is_empty()
        // Match the JavaScript client's bound, including supplementary code points.
        && query.encode_utf16().count() <= 500
        && !query.chars().any(|c| c.is_control() || matches!(c, '\u{00ad}' | '\u{061c}' | '\u{200b}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::web_search::provider::adapter;
    use std::{fs, io::BufRead};

    #[test]
    fn a_second_path_to_the_long_socket_requires_auth_and_stops_at_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let long = root.path().join("long-home-".repeat(12));
        fs::create_dir(&long).unwrap();
        let service = SearchService::new(adapter("brave").unwrap(), "fixture-key".into());
        let broker = SearchBroker::start(Box::new(service), &long).unwrap();
        let socket = broker.socket_path().to_owned();

        // Like a Docker bind, the short path reaches the same inode and can
        // outlive the original name. No real mount or Docker is needed here.
        fs::hard_link(&socket, root.path().join("mounted.sock")).unwrap();
        let root_fd = fs::File::open(root.path()).unwrap();
        let address = format!("/proc/self/fd/{}/mounted.sock", root_fd.as_raw_fd());
        let status = |token: &str| -> Value {
            let mut client = Stream::connect(&address).unwrap();
            client.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let request = json!({ "version": 1, "operation": "status", "token": token });
            writeln!(client, "{request}").unwrap();
            let mut reply = String::new();
            io::BufReader::new(client).read_line(&mut reply).unwrap();
            serde_json::from_str(&reply).unwrap()
        };
        assert_eq!(status("wrong-token"), error("unauthorized"));
        assert_eq!(
            status(broker.token()),
            json!({ "ok": true, "provider": "brave" })
        );

        drop(broker);
        assert!(!socket.exists());
        assert!(Stream::connect(address).is_err());
    }
}

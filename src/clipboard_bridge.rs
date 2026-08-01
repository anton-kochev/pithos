mod host;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const HOST_FOR_CONTAINER: &str = "host.docker.internal";
const XCLIP_SHIM: &[u8] = include_bytes!("../scripts/pithos-xclip");
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_HEAD_BYTES: usize = 4096;

/// Host-side HTTP bridge that lets the Linux container read the host clipboard.
///
/// Pi already knows how to paste images through an `xclip` fallback. Pithos
/// mounts an `xclip` shim into the running container (and bakes it into the base
/// image); this bridge is the host endpoint that shim queries.
pub struct ClipboardBridge {
    port: u16,
    token: String,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    shim_path: std::path::PathBuf,
}

impl ClipboardBridge {
    pub fn start(shim_dir: &std::path::Path) -> std::io::Result<Self> {
        let bind_addr = bridge_bind_addr();
        let listener = TcpListener::bind((bind_addr, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = random_token()?;
        // The filename nonce is intentionally independent of the bearer token:
        // Docker includes bind-source paths in its argv.
        let shim_nonce = random_token()?;
        let shim_path = shim_dir.join(format!("pithos-xclip-{}-{shim_nonce}", std::process::id()));
        materialize_xclip_shim(&shim_path)?;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread_token = token.clone();
        let handle = thread::spawn(move || serve(listener, thread_token, thread_running));

        Ok(Self {
            port,
            token,
            running,
            handle: Some(handle),
            shim_path,
        })
    }

    /// URL visible from inside Docker. The bearer token is embedded in the path
    /// so the shell shim can stay dependency-free.
    pub fn container_url(&self) -> String {
        container_url(self.port, &self.token)
    }

    pub fn shim_path(&self) -> &std::path::Path {
        &self.shim_path
    }
}

impl Drop for ClipboardBridge {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.shim_path);
    }
}

fn container_url(port: u16, token: &str) -> String {
    format!("http://{HOST_FOR_CONTAINER}:{port}/{token}")
}

fn materialize_xclip_shim(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, XCLIP_SHIM)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Docker's `host-gateway` only reaches services bound on a non-loopback host
/// address on Linux. Docker Desktop on macOS/Windows forwards localhost through
/// `host.docker.internal`, so keep the listener loopback-only there.
fn bridge_bind_addr() -> &'static str {
    if cfg!(target_os = "linux") {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

fn serve(listener: TcpListener, token: String, running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => handle_client(&mut stream, &token, host::read_png),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    Types,
    Image,
}

fn endpoint(path: &str, token: &str) -> Option<Endpoint> {
    if path == format!("/{token}/types") {
        Some(Endpoint::Types)
    } else if path == format!("/{token}/image.png") {
        Some(Endpoint::Image)
    } else {
        None
    }
}

fn handle_client<F>(stream: &mut TcpStream, token: &str, image_reader: F)
where
    F: FnOnce() -> Option<Vec<u8>>,
{
    let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_TIMEOUT));
    let Some(request) = read_request_head(stream) else {
        let _ = write_response(stream, 400, "Bad Request", "text/plain", b"");
        return;
    };
    let request = String::from_utf8_lossy(&request);
    let Some(path) = request_path(&request) else {
        let _ = write_response(stream, 400, "Bad Request", "text/plain", b"");
        return;
    };

    match endpoint(path, token) {
        // Pi allows only one second for its xclip TARGETS probe. Advertise the
        // bridge's normalized PNG type immediately; read the clipboard only on
        // the subsequent image request, for which Pi allows longer.
        Some(Endpoint::Types) => {
            let _ = write_response(stream, 200, "OK", "text/plain", b"image/png\n");
        }
        Some(Endpoint::Image) => match image_reader() {
            Some(bytes) => {
                let _ = write_response(stream, 200, "OK", "image/png", &bytes);
            }
            None => {
                let _ = write_response(stream, 404, "Not Found", "text/plain", b"");
            }
        },
        None => {
            let _ = write_response(stream, 404, "Not Found", "text/plain", b"");
        }
    }
}

fn read_request_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut request = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];
    while request.len() < MAX_REQUEST_HEAD_BYTES {
        let remaining = MAX_REQUEST_HEAD_BYTES - request.len();
        let read_len = remaining.min(chunk.len());
        let n = stream.read(&mut chunk[..read_len]).ok()?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..n]);
        if request.contains(&b'\n') {
            break;
        }
    }
    (!request.is_empty() && request.contains(&b'\n')).then_some(request)
}

fn request_path(request: &str) -> Option<&str> {
    let first = request.lines().next()?;
    let mut parts = first.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(path)) if path.starts_with('/') => Some(path),
        _ => None,
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn random_token() -> std::io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("cannot generate clipboard token: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests;

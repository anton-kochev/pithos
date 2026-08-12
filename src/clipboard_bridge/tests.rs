use super::host::{escape_applescript_string, looks_like_png, remaining_at};
use super::*;
#[cfg(unix)]
use std::process::Command;

#[test]
fn request_path_accepts_get_paths() {
    assert_eq!(
        request_path("GET /abc/types HTTP/1.1\r\nHost: x\r\n"),
        Some("/abc/types")
    );
}

#[test]
fn request_path_rejects_non_get_requests() {
    assert_eq!(request_path("POST /abc/types HTTP/1.1\r\n"), None);
}

#[test]
fn endpoints_require_the_exact_token_and_path() {
    assert_eq!(endpoint("/secret/types", "secret"), Some(Endpoint::Types));
    assert_eq!(
        endpoint("/secret/image.png", "secret"),
        Some(Endpoint::Image)
    );
    assert_eq!(endpoint("/wrong/types", "secret"), None);
    assert_eq!(endpoint("/secret/types/extra", "secret"), None);
}

#[test]
fn bridge_starts_and_stops_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let bridge = ClipboardBridge::start(temp.path()).expect("bridge should bind an ephemeral port");
    let shim_path = bridge.shim_path().to_path_buf();
    assert_eq!(fs::read(&shim_path).unwrap(), XCLIP_SHIM);
    let mut stream = TcpStream::connect(("127.0.0.1", bridge.port)).unwrap();
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream
        .write_all(b"GET /wrong/types HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{response:?}"
    );
    drop(bridge);
    assert!(!shim_path.exists());
}

#[test]
fn concurrent_bridges_use_independent_shim_files() {
    let temp = tempfile::tempdir().unwrap();
    let first = ClipboardBridge::start(temp.path()).unwrap();
    let second = ClipboardBridge::start(temp.path()).unwrap();
    let first_path = first.shim_path().to_path_buf();
    let second_path = second.shim_path().to_path_buf();

    assert_ne!(first_path, second_path);
    assert!(!first_path.to_string_lossy().contains(&first.token));
    assert!(!second_path.to_string_lossy().contains(&second.token));
    assert!(first_path.exists());
    assert!(second_path.exists());
    drop(first);
    assert!(!first_path.exists());
    assert!(second_path.exists());
}

#[test]
fn types_endpoint_is_constant_time_and_does_not_read_clipboard() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        handle_client(&mut stream, "secret", || {
            panic!("clipboard must not be read")
        });
    });

    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream
        .write_all(b"GET /secret/types HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    server.join().unwrap();
    assert!(response.ends_with(b"image/png\n"));
}

#[test]
fn authenticated_image_endpoint_preserves_binary_bytes() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let image = b"\x89PNG\r\n\x1a\nclipboard".to_vec();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        handle_client(&mut stream, "secret", || Some(image));
    });

    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(CLIENT_TIMEOUT)).unwrap();
    stream
        .write_all(b"GET /secret/image.png HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    server.join().unwrap();

    let (_, body) =
        response.split_at(response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4);
    assert_eq!(body, b"\x89PNG\r\n\x1a\nclipboard");
}

#[test]
fn container_url_embeds_host_port_and_token() {
    assert_eq!(
        container_url(49152, "tok"),
        "http://host.docker.internal:49152/tok"
    );
}

#[test]
fn random_tokens_are_128_bit_lowercase_hex() {
    let first = random_token().unwrap();
    let second = random_token().unwrap();
    assert_eq!(first.len(), 32);
    assert!(
        first
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_ne!(first, second);
}

#[test]
fn remaining_clipboard_budget_uses_one_end_to_end_deadline() {
    let now = std::time::Instant::now();
    let deadline = now + Duration::from_secs(2);
    assert_eq!(
        remaining_at(deadline, now + Duration::from_millis(750)),
        Some(Duration::from_millis(1250))
    );
    assert_eq!(remaining_at(deadline, deadline), None);
    assert_eq!(
        remaining_at(deadline, deadline + Duration::from_millis(1)),
        None
    );
}

#[test]
fn applescript_paths_escape_backslashes_and_quotes() {
    assert_eq!(escape_applescript_string("a\\b\"c"), "a\\\\b\\\"c");
}

#[test]
fn png_signature_detection_is_strict() {
    assert!(looks_like_png(b"\x89PNG\r\n\x1a\nrest"));
    assert!(!looks_like_png(b"not png"));
}

#[cfg(unix)]
#[test]
fn xclip_shim_is_executable_and_valid_bash() {
    use std::os::unix::fs::PermissionsExt;

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/pithos-xclip");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "{} must be executable", path.display());
    assert!(
        Command::new("bash")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap()
            .success(),
        "bash -n rejected {}",
        path.display()
    );
}

#[cfg(unix)]
fn xclip_shim_invokes_bridge(args: &[&str]) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let curl_marker = temp.path().join("curl-was-called");
    let fake_curl = temp.path().join("curl");
    fs::write(
        &fake_curl,
        format!("#!/bin/sh\ntouch '{}'\n", curl_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let shim = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/pithos-xclip");
    let path = format!(
        "{}:{}",
        temp.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _ = Command::new("bash")
        .arg(&shim)
        .args(args)
        .env("PATH", path)
        .env("DISPLAY", "")
        .env("PITHOS_CLIPBOARD_URL", "http://bridge/token")
        .output()
        .unwrap();
    curl_marker.exists()
}

#[cfg(unix)]
#[test]
fn xclip_shim_does_not_proxy_non_clipboard_selections() {
    assert!(!xclip_shim_invokes_bridge(&[
        "-selection",
        "primary",
        "-t",
        "image/png",
        "-o",
    ]));
}

#[cfg(unix)]
#[test]
fn xclip_shim_does_not_allow_duplicate_selection_to_bypass_validation() {
    assert!(!xclip_shim_invokes_bridge(&[
        "-selection",
        "primary",
        "-selection",
        "clipboard",
        "-t",
        "image/png",
        "-o",
    ]));
}

#[cfg(unix)]
#[test]
fn xclip_shim_does_not_proxy_unknown_arguments() {
    assert!(!xclip_shim_invokes_bridge(&[
        "-selection",
        "clipboard",
        "-t",
        "image/png",
        "-o",
        "--unknown",
    ]));
}

#[cfg(unix)]
#[test]
fn xclip_shim_proxies_targets_and_png_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let fake_curl = temp.path().join("curl");
    fs::write(
        &fake_curl,
        "#!/bin/sh\nfor arg in \"$@\"; do url=$arg; done\ncase \"$url\" in\n  */types) printf 'image/png\\n' ;;\n  */image.png) printf '\\211PNG\\r\\n\\032\\nbytes' ;;\n  *) exit 22 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let shim = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/pithos-xclip");
    let path = format!(
        "{}:{}",
        temp.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |target: &str| {
        Command::new("bash")
            .arg(&shim)
            .args(["-selection", "clipboard", "-t", target, "-o"])
            .env("PATH", &path)
            .env("PITHOS_CLIPBOARD_URL", "http://bridge/token")
            .output()
            .unwrap()
    };

    let targets = run("TARGETS");
    assert!(targets.status.success());
    assert_eq!(targets.stdout, b"image/png\n");

    let image = run("image/png");
    assert!(image.status.success());
    assert_eq!(image.stdout, b"\x89PNG\r\n\x1a\nbytes");
}

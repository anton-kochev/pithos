#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Workspace routes: the same Host/Bearer framing as status, plus strict
//! JSON bodies turned into typed requests. No Docker authority here.

use pithos::{
    broker::{
        api::{ApiConnection, ApiPoll, ApiRequest},
        credential::RunCredential,
        status::{Phase, Snapshot},
    },
    lifecycle::Shutdown,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

const HOST: &str = "localhost:43127";

struct Setup {
    _dir: tempfile::TempDir,
    credential: RunCredential,
    token: String,
}
fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let credential = RunCredential::create(dir.path(), &format!("http://{HOST}")).unwrap();
    let file: Value =
        serde_json::from_slice(&fs::read(dir.path().join("broker-client.json")).unwrap()).unwrap();
    let token = file["token"].as_str().unwrap().to_owned();
    Setup {
        _dir: dir,
        credential,
        token,
    }
}

fn post(path: &str, token: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Send `bytes`, poll the connection until it yields a request or finishes.
/// Answers a surfaced request with `answer` and returns (request, response).
fn exchange(s: &Setup, bytes: &[u8], answer: Option<(u16, Value)>) -> (Option<ApiRequest>, String) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    client.write_all(bytes).unwrap();
    let mut connection =
        ApiConnection::new(server, s.credential.token(), HOST, Shutdown::new()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = None;
    loop {
        match connection.poll() {
            ApiPoll::Pending => {}
            ApiPoll::Request(ApiRequest::Status) => {
                request = Some(ApiRequest::Status);
                connection.respond_status(Snapshot {
                    phase: Phase::Ready,
                });
            }
            ApiPoll::Request(r) => {
                request = Some(r);
                let (status, body) = answer.clone().expect("no request expected");
                connection.respond(status, &body);
            }
            ApiPoll::Finished | ApiPoll::Failed(_) => break,
        }
        assert!(Instant::now() < deadline, "connection did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    // A settled connection has closed its socket; dropping it is a no-op.
    drop(connection);
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut response = String::new();
    let _ = client.read_to_string(&mut response);
    (request, response)
}

#[test]
fn status_route_answers_like_the_status_protocol() {
    let s = setup();
    let head = format!(
        "GET /v1/status HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {}\r\n\r\n",
        s.token
    );
    let (request, response) = exchange(&s, head.as_bytes(), None);
    assert_eq!(request, Some(ApiRequest::Status));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Cache-Control: no-store\r\nConnection: close\r\n"));
    assert!(response.ends_with("{\"version\":1,\"phase\":\"ready\"}\n"));
}

#[test]
fn every_app_route_becomes_a_typed_request() {
    let s = setup();
    for (path, body, expected) in [
        (
            "/v1/apps/build",
            r#"{"request_id":"b-1","app":"api","dockerfile":"api/Dockerfile","context":"api"}"#,
            ApiRequest::Build {
                request_id: "b-1".into(),
                app: "api".into(),
                dockerfile: "api/Dockerfile".into(),
                context: "api".into(),
            },
        ),
        (
            "/v1/apps/run",
            r#"{"request_id":"r-1","app":"api","command":["dotnet","Api.dll"]}"#,
            ApiRequest::Run {
                request_id: "r-1".into(),
                app: "api".into(),
                command: vec!["dotnet".into(), "Api.dll".into()],
            },
        ),
        (
            "/v1/apps/run",
            r#"{"request_id":"r-2","app":"api"}"#,
            ApiRequest::Run {
                request_id: "r-2".into(),
                app: "api".into(),
                command: vec![],
            },
        ),
        (
            "/v1/apps/status",
            r#"{"app":"api"}"#,
            ApiRequest::AppStatus { app: "api".into() },
        ),
        (
            "/v1/apps/logs",
            r#"{"app":"api","tail":20}"#,
            ApiRequest::Logs {
                app: "api".into(),
                tail: 20,
            },
        ),
        (
            "/v1/apps/logs",
            r#"{"app":"api"}"#,
            ApiRequest::Logs {
                app: "api".into(),
                tail: 100,
            },
        ),
        (
            "/v1/apps/stop",
            r#"{"request_id":"s-1","app":"api"}"#,
            ApiRequest::Stop {
                request_id: "s-1".into(),
                app: "api".into(),
            },
        ),
    ] {
        let (request, response) = exchange(
            &s,
            &post(path, &s.token, body),
            Some((200, json!({"ok": true}))),
        );
        assert_eq!(request, Some(expected), "{path} {body}");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    }
}

#[test]
fn bad_credentials_never_surface_a_request() {
    let s = setup();
    let wrong = "0".repeat(64);
    let (request, response) = exchange(
        &s,
        &post(
            "/v1/apps/stop",
            &wrong,
            r#"{"request_id":"s-1","app":"api"}"#,
        ),
        None,
    );
    assert_eq!(request, None);
    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized"),
        "{response}"
    );
}

#[test]
fn malformed_requests_are_rejected_before_dispatch() {
    let s = setup();
    let ok = r#"{"request_id":"s-1","app":"api"}"#;
    let mut cases: Vec<Vec<u8>> = vec![
        // Unknown field, wrong type, missing field, bad request ID, trailing JSON.
        post(
            "/v1/apps/stop",
            &s.token,
            r#"{"request_id":"s-1","app":"api","x":1}"#,
        ),
        post("/v1/apps/stop", &s.token, r#"{"request_id":1,"app":"api"}"#),
        post("/v1/apps/stop", &s.token, r#"{"app":"api"}"#),
        post(
            "/v1/apps/stop",
            &s.token,
            r#"{"request_id":"bad id","app":"api"}"#,
        ),
        post(
            "/v1/apps/stop",
            &s.token,
            r#"{"request_id":"s","app":"api"}{}"#,
        ),
        post("/v1/apps/unknown", &s.token, ok),
        post("/v1/apps/stop?x=1", &s.token, ok),
        post("/v1/status", &s.token, ok),
        // Oversized body.
        post(
            "/v1/apps/run",
            &s.token,
            &format!(
                r#"{{"request_id":"r","app":"api","command":["{}"]}}"#,
                "x".repeat(20000)
            ),
        ),
    ];
    // Missing content type, a body on GET, bytes after the declared body.
    let without_type = String::from_utf8(post("/v1/apps/stop", &s.token, ok))
        .unwrap()
        .replace("Content-Type: application/json\r\n", "");
    cases.push(without_type.into_bytes());
    cases.push(
        format!(
            "GET /v1/status HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {}\r\nContent-Length: 2\r\n\r\n{{}}",
            s.token
        )
        .into_bytes(),
    );
    let mut extra = post("/v1/apps/stop", &s.token, ok);
    extra.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n");
    cases.push(extra);
    for bytes in cases {
        let (request, response) = exchange(&s, &bytes, None);
        assert_eq!(request, None, "{}", String::from_utf8_lossy(&bytes));
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{}\n{response}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn responses_carry_the_status_code_and_json_body() {
    let s = setup();
    for (code, line) in [
        (403, "HTTP/1.1 403 Forbidden"),
        (404, "HTTP/1.1 404 Not Found"),
        (409, "HTTP/1.1 409 Conflict"),
        (422, "HTTP/1.1 422 Unprocessable Content"),
        (503, "HTTP/1.1 503 Service Unavailable"),
    ] {
        let (_, response) = exchange(
            &s,
            &post("/v1/apps/status", &s.token, r#"{"app":"api"}"#),
            Some((code, json!({"error": "x"}))),
        );
        assert!(response.starts_with(line), "{response}");
        assert!(response.ends_with("{\"error\":\"x\"}\n"));
    }
}

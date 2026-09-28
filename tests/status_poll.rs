#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::{
    broker::{
        credential::RunCredential,
        status::{Limits, Phase, Snapshot, StatusConnection, StatusError, StatusPoll},
    },
    lifecycle::{Shutdown, ShutdownReason},
};
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

const HOST: &str = "localhost:43127";

fn credential() -> (tempfile::TempDir, RunCredential) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let credential = RunCredential::create(directory.path(), &format!("http://{HOST}")).unwrap();
    (directory, credential)
}

fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    for stream in [&client, &server] {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.set_nodelay(true).unwrap();
    }
    (client, server)
}

fn request(credential: &RunCredential) -> Vec<u8> {
    format!(
        "GET /v1/status HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {}\r\n\r\n",
        credential.token().expose_secret()
    )
    .into_bytes()
}

fn connection(server: TcpStream, credential: &RunCredential) -> StatusConnection {
    StatusConnection::new(
        server,
        credential.token(),
        HOST,
        Snapshot {
            phase: Phase::Ready,
        },
        Limits::default(),
        Shutdown::new(),
    )
    .unwrap()
}

fn settle(connection: &mut StatusConnection) -> StatusPoll {
    let start = Instant::now();
    loop {
        match connection.poll() {
            StatusPoll::Pending => {
                assert!(start.elapsed() < Duration::from_secs(3), "fixture deadline");
                std::thread::sleep(Duration::from_millis(1));
            }
            terminal => return terminal,
        }
    }
}

// Closed with no response. macOS answers bytes sent after our read shutdown
// with RST, so the peer may see a reset instead of a clean EOF.
fn assert_closed_without_response(client: &mut TcpStream) {
    let mut bytes = Vec::new();
    match client.read_to_end(&mut bytes) {
        Ok(_) => assert!(bytes.is_empty()),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
    }
}

fn response(client: &mut TcpStream) -> String {
    let mut text = String::new();
    client.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn idle_and_partial_heads_yield_pending_without_waiting_for_deadline() {
    let (_directory, credential) = credential();
    for head in [b"".as_slice(), b"GET /v1/status HTTP/1.1\r\n"] {
        let (mut client, server) = pair();
        client.write_all(head).unwrap();
        let mut connection = StatusConnection::new(
            server,
            credential.token(),
            HOST,
            Snapshot {
                phase: Phase::Ready,
            },
            Limits {
                read_timeout: Duration::from_millis(300),
                ..Limits::default()
            },
            Shutdown::new(),
        )
        .unwrap();
        let start = Instant::now();
        let result = connection.poll();
        assert!(
            start.elapsed() < Duration::from_millis(25),
            "poll waited: {:?}",
            start.elapsed()
        );
        assert_eq!(result, StatusPoll::Pending);
        // Repeated idle polls must not hide a sleep inside the poller either.
        let start = Instant::now();
        for _ in 0..16 {
            assert_eq!(connection.poll(), StatusPoll::Pending);
        }
        assert!(start.elapsed() < Duration::from_millis(25));
    }
}

#[test]
fn complete_owned_exchange_writes_response_and_closes_before_drop() {
    let (_directory, credential) = credential();
    let (mut client, server) = pair();
    client.write_all(&request(&credential)).unwrap();
    let mut connection = connection(server, &credential);
    assert_eq!(settle(&mut connection), StatusPoll::Finished);
    let text = response(&mut client);
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.ends_with("{\"version\":1,\"phase\":\"ready\"}\n"));
    assert_eq!(connection.poll(), StatusPoll::Finished);
}

#[test]
fn fragmented_terminator_waits_and_snapshot_is_frozen() {
    let (_directory, credential) = credential();
    let (mut client, server) = pair();
    let mut snapshot = Snapshot {
        phase: Phase::Preparing,
    };
    let mut connection = StatusConnection::new(
        server,
        credential.token(),
        HOST,
        snapshot,
        Limits::default(),
        Shutdown::new(),
    )
    .unwrap();
    snapshot.phase = Phase::Ready;
    let head = request(&credential);
    client.set_nonblocking(true).unwrap();
    for fragment in [&head[..head.len() - 3], b"\n".as_slice(), b"\r"] {
        client.write_all(fragment).unwrap();
        assert_eq!(connection.poll(), StatusPoll::Pending);
        assert_eq!(format!("{connection:?}"), "StatusConnection([REDACTED])");
        assert!(
            matches!(client.read(&mut [0; 1]), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
    }
    client.write_all(b"\n").unwrap();
    assert_eq!(settle(&mut connection), StatusPoll::Finished);
    client.set_nonblocking(false).unwrap();
    assert!(response(&mut client).ends_with("{\"version\":1,\"phase\":\"preparing\"}\n"));
    assert_eq!(snapshot.phase, Phase::Ready);
}

#[test]
fn already_received_pipeline_or_body_tail_rejects_with_one_response() {
    let (_directory, credential) = credential();
    let head = request(&credential);
    for tail in [head.as_slice(), b"synthetic-private-marker"] {
        let (mut client, server) = pair();
        let mut connection = connection(server, &credential);
        // Force accumulation before supplying the final frame and its tail in
        // one write. No response is permitted for the unfinished first frame.
        client.write_all(&head[..head.len() - 2]).unwrap();
        assert_eq!(connection.poll(), StatusPoll::Pending);
        let mut remainder = b"\r\n".to_vec();
        remainder.extend_from_slice(tail);
        client.write_all(&remainder).unwrap();
        assert_eq!(settle(&mut connection), StatusPoll::Finished);
        let text = response(&mut client);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert_eq!(text.matches("HTTP/1.1").count(), 1);
        assert!(!text.contains("phase"));
        assert!(!text.contains("synthetic-private-marker"));
        assert_eq!(connection.poll(), StatusPoll::Finished);
    }
}

#[test]
fn read_deadline_starts_at_construction_and_does_not_reset_on_trickle() {
    let (_directory, credential) = credential();
    for trickle in [false, true] {
        let (mut client, server) = pair();
        let mut connection = StatusConnection::new(
            server,
            credential.token(),
            HOST,
            Snapshot {
                phase: Phase::Ready,
            },
            Limits {
                read_timeout: Duration::from_millis(200),
                ..Limits::default()
            },
            Shutdown::new(),
        )
        .unwrap();
        let start = Instant::now();
        if trickle {
            for target in [50, 100] {
                std::thread::sleep(Duration::from_millis(target).saturating_sub(start.elapsed()));
                client.write_all(b"G").unwrap();
                assert_eq!(connection.poll(), StatusPoll::Pending);
            }
        }
        std::thread::sleep(Duration::from_millis(220).saturating_sub(start.elapsed()));
        assert_eq!(
            connection.poll(),
            StatusPoll::Failed(StatusError::ReadDeadline)
        );
        assert_eq!(
            connection.poll(),
            StatusPoll::Failed(StatusError::ReadDeadline)
        );
        assert_closed_without_response(&mut client);
    }
}

#[test]
fn cancellation_closes_idle_partial_and_complete_heads_without_dispatch() {
    let (_directory, credential) = credential();
    let complete = request(&credential);
    for head in [b"".as_slice(), b"GET /v1/status", &complete] {
        for pre_requested in [false, true] {
            let (mut client, server) = pair();
            let shutdown = Shutdown::new();
            if pre_requested {
                shutdown.request(ShutdownReason::Requested);
            }
            let mut connection = StatusConnection::new(
                server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready,
                },
                Limits::default(),
                shutdown.clone(),
            )
            .unwrap();
            if !pre_requested {
                assert_eq!(connection.poll(), StatusPoll::Pending);
            }
            client.write_all(head).unwrap();
            if !pre_requested && head.len() < complete.len() {
                assert_eq!(connection.poll(), StatusPoll::Pending);
            }
            shutdown.request(ShutdownReason::Terminate);
            assert_eq!(
                connection.poll(),
                StatusPoll::Failed(StatusError::Cancelled)
            );
            assert_eq!(
                connection.poll(),
                StatusPoll::Failed(StatusError::Cancelled)
            );
            assert_closed_without_response(&mut client);
        }
    }
}

#[test]
fn drop_and_terminal_poll_shutdown_even_a_retained_descriptor_in_both_directions() {
    let (_directory, credential) = credential();
    for (finish, partial) in [(false, false), (false, true), (true, false)] {
        let (mut client, server) = pair();
        let mut retained = server.try_clone().unwrap();
        let mut connection = connection(server, &credential);
        if finish {
            client.write_all(&request(&credential)).unwrap();
            assert_eq!(settle(&mut connection), StatusPoll::Finished);
        } else {
            if partial {
                client.write_all(b"GET /v1/status").unwrap();
                assert_eq!(connection.poll(), StatusPoll::Pending);
            }
            drop(connection);
        }
        // No concurrent use: only inspect the retained descriptor after close.
        assert!(retained.write(b"not sent").is_err());
        assert_eq!(retained.read(&mut [0; 1]).unwrap(), 0);
        if finish {
            assert!(response(&mut client).starts_with("HTTP/1.1 200 OK\r\n"));
        } else {
            assert_closed_without_response(&mut client);
        }
    }
}

#[test]
fn invalid_constructor_inputs_close_without_waiting_or_exposing_them() {
    let (_directory, credential) = credential();
    let defaults = Limits::default();
    let mut cases = vec![
        ("localhost:0", defaults, StatusError::InvalidHost),
        (
            "localhost:43127?synthetic-private-marker",
            defaults,
            StatusError::InvalidHost,
        ),
    ];
    for limits in [
        Limits {
            max_header_bytes: 3,
            ..defaults
        },
        Limits {
            max_header_bytes: 8193,
            ..defaults
        },
        Limits {
            max_header_fields: 0,
            ..defaults
        },
        Limits {
            max_header_fields: 65,
            ..defaults
        },
        Limits {
            max_response_bytes: 255,
            ..defaults
        },
        Limits {
            max_response_bytes: 1025,
            ..defaults
        },
        Limits {
            read_timeout: Duration::ZERO,
            ..defaults
        },
        Limits {
            read_timeout: Duration::MAX,
            ..defaults
        },
        Limits {
            write_timeout: Duration::ZERO,
            ..defaults
        },
        Limits {
            write_timeout: Duration::MAX,
            ..defaults
        },
    ] {
        cases.push((HOST, limits, StatusError::InvalidLimits));
    }
    for (host, limits, expected) in cases {
        let (mut client, server) = pair();
        let start = Instant::now();
        let error = StatusConnection::new(
            server,
            credential.token(),
            host,
            Snapshot {
                phase: Phase::Ready,
            },
            limits,
            Shutdown::new(),
        )
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_millis(25));
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("synthetic-private-marker"));
        assert_closed_without_response(&mut client);
    }
}

#[test]
fn raw_byte_field_and_response_budgets_are_inclusive() {
    let (_directory, credential) = credential();
    let good = String::from_utf8(request(&credential)).unwrap();
    let exact = good.replace(
        "\r\n\r\n",
        &format!("\r\nX: {}\r\n\r\n", "x".repeat(8192 - good.len() - 5)),
    );
    let fields = good.replace("\r\n\r\n", &format!("\r\n{}\r\n", "X: x\r\n".repeat(62)));
    let mut raw = good.as_bytes()[..good.len() - 2].to_vec();
    raw.extend_from_slice(b"X: \xff\r\n\r\n");
    let cases = [
        (exact.as_bytes().to_vec(), 8192, 64, "200 OK"),
        (
            exact.replacen("X: ", "X: x", 1).into_bytes(),
            8192,
            64,
            "400 Bad Request",
        ),
        (fields.as_bytes().to_vec(), 8192, 64, "200 OK"),
        (
            fields.replace("\r\n\r\n", "\r\nX: x\r\n\r\n").into_bytes(),
            8192,
            64,
            "400 Bad Request",
        ),
        (raw, 8192, 64, "400 Bad Request"),
        (good.as_bytes().to_vec(), good.len(), 2, "200 OK"),
        (
            good.as_bytes().to_vec(),
            good.len() - 1,
            2,
            "400 Bad Request",
        ),
        (good.as_bytes().to_vec(), good.len(), 1, "400 Bad Request"),
        (
            format!("GET /v1/status HTTP/1.1\r\nHost: {HOST}\r\n\r\n").into_bytes(),
            8192,
            64,
            "401 Unauthorized",
        ),
    ];
    for (head, max_header_bytes, max_header_fields, status) in cases {
        let (mut client, server) = pair();
        client.write_all(&head).unwrap();
        let mut connection = StatusConnection::new(
            server,
            credential.token(),
            HOST,
            Snapshot {
                phase: Phase::RecoveryRequired,
            },
            Limits {
                max_header_bytes,
                max_header_fields,
                max_response_bytes: 256,
                ..Limits::default()
            },
            Shutdown::new(),
        )
        .unwrap();
        assert_eq!(settle(&mut connection), StatusPoll::Finished);
        let text = response(&mut client);
        assert!(text.starts_with(&format!("HTTP/1.1 {status}\r\n")));
        let (headers, body) = text.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains(&format!("\r\nContent-Length: {}", body.len())));
        assert!(headers.contains("\r\nConnection: close"));
        assert!(headers.contains("\r\nCache-Control: no-store"));
        assert!(!text.to_ascii_lowercase().contains("access-control-"));
        assert!(text.len() <= 256);
        assert!(!text.contains(credential.token().expose_secret()));
        assert_eq!(format!("{connection:?}"), "StatusConnection([REDACTED])");
    }
}

#[test]
fn eof_rejects_an_empty_or_partial_head_but_finishes_the_http_response() {
    let (_directory, credential) = credential();
    for head in [
        b"".as_slice(),
        b"GET /v1/status HTTP/1.1\r\nX: synthetic-private-marker",
    ] {
        let (mut client, server) = pair();
        client.write_all(head).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut connection = connection(server, &credential);
        assert_eq!(settle(&mut connection), StatusPoll::Finished);
        let text = response(&mut client);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(!text.contains("synthetic-private-marker"));
    }
}

// Bounded real-socket fixture; wait for delayed ACKs to settle without reading
// from the peer. This fills only test data before STATUS owns the socket.
fn fill_send_queue(server: &mut TcpStream) -> usize {
    server.set_nonblocking(true).unwrap();
    let start = Instant::now();
    let mut last_progress = start;
    let mut total = 0;
    loop {
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(total < 16 * 1024 * 1024);
        match server.write(&[b'x'; 4096]) {
            Ok(0) => panic!("unexpected write zero"),
            Ok(count) => {
                total += count;
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if last_progress.elapsed() >= Duration::from_millis(500) {
                    return total;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => panic!("fixture I/O kind: {:?}", error.kind()),
        }
    }
}

#[test]
fn write_deadline_starts_when_the_response_is_prepared_not_at_construction() {
    let (_directory, credential) = credential();
    let (mut client, server) = pair();
    let mut connection = StatusConnection::new(
        server,
        credential.token(),
        HOST,
        Snapshot {
            phase: Phase::Ready,
        },
        Limits {
            write_timeout: Duration::from_millis(100),
            ..Limits::default()
        },
        Shutdown::new(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(120));
    client.write_all(&request(&credential)).unwrap();
    assert_eq!(settle(&mut connection), StatusPoll::Finished);
    assert!(response(&mut client).starts_with("HTTP/1.1 200 OK\r\n"));
}

#[test]
fn backpressured_response_yields_and_obeys_write_deadline_cancel_drop_or_resume() {
    let (_directory, credential) = credential();
    for action in ["deadline", "cancel", "drop", "resume"] {
        let (mut client, mut server) = pair();
        // Request first: on macOS, peer data after the fill reopens send space.
        client.write_all(&request(&credential)).unwrap();
        let queued = fill_send_queue(&mut server);
        let shutdown = Shutdown::new();
        let mut connection = StatusConnection::new(
            server,
            credential.token(),
            HOST,
            Snapshot {
                phase: Phase::Ready,
            },
            Limits {
                write_timeout: Duration::from_millis(200),
                ..Limits::default()
            },
            shutdown.clone(),
        )
        .unwrap();
        let start = Instant::now();
        for _ in 0..16 {
            assert_eq!(connection.poll(), StatusPoll::Pending);
        }
        assert!(start.elapsed() < Duration::from_millis(25));
        match action {
            "deadline" => {
                std::thread::sleep(Duration::from_millis(100));
                assert_eq!(connection.poll(), StatusPoll::Pending);
                std::thread::sleep(Duration::from_millis(120));
                assert_eq!(
                    connection.poll(),
                    StatusPoll::Failed(StatusError::WriteDeadline)
                );
                assert_eq!(
                    connection.poll(),
                    StatusPoll::Failed(StatusError::WriteDeadline)
                );
            }
            "cancel" => {
                shutdown.request(ShutdownReason::Requested);
                assert_eq!(
                    connection.poll(),
                    StatusPoll::Failed(StatusError::Cancelled)
                );
            }
            "drop" => drop(connection),
            "resume" => {
                let mut prefix = vec![0; queued];
                client.read_exact(&mut prefix).unwrap();
                assert!(prefix.iter().all(|&b| b == b'x'));
                assert_eq!(settle(&mut connection), StatusPoll::Finished);
                assert!(response(&mut client).starts_with("HTTP/1.1 200 OK\r\n"));
                continue;
            }
            _ => unreachable!(),
        }
        let mut prefix = Vec::new();
        client.read_to_end(&mut prefix).unwrap();
        assert_eq!(prefix.len(), queued);
        assert!(prefix.iter().all(|&b| b == b'x'));
    }
}

#[test]
fn caller_round_robin_services_ready_client_behind_idle_and_partial_backlog() {
    let (_directory, credential) = credential();
    let shutdown = Shutdown::new();
    let mut clients = Vec::new();
    let mut connections = Vec::new();
    for index in 0..16 {
        let (mut client, server) = pair();
        if index % 2 == 0 {
            client.write_all(b"GET /v1/status").unwrap();
        }
        clients.push(client);
        connections.push(
            StatusConnection::new(
                server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready,
                },
                Limits::default(),
                shutdown.clone(),
            )
            .unwrap(),
        );
    }
    let (mut ready_client, server) = pair();
    ready_client.write_all(&request(&credential)).unwrap();
    let mut ready = connection(server, &credential);
    let start = Instant::now();
    loop {
        for connection in &mut connections {
            assert_eq!(connection.poll(), StatusPoll::Pending);
        }
        let result = ready.poll();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "backlog starved ready connection"
        );
        if result == StatusPoll::Finished {
            break;
        }
        assert_eq!(result, StatusPoll::Pending);
        std::thread::yield_now();
    }
    assert!(response(&mut ready_client).starts_with("HTTP/1.1 200 OK\r\n"));
    shutdown.request(ShutdownReason::Requested);
    for (connection, client) in connections.iter_mut().zip(&mut clients) {
        assert_eq!(
            connection.poll(),
            StatusPoll::Failed(StatusError::Cancelled)
        );
        assert!(response(client).is_empty());
    }
}

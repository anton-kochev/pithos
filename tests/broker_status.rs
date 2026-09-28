#![cfg(any(target_os = "linux", target_os = "macos"))]

use pithos::{
    broker::{
        credential::RunCredential,
        status::{
            Limits, Phase, Snapshot, StatusConnection, StatusError, StatusPoll, handle_connection,
        },
    },
    lifecycle::{Shutdown, ShutdownReason},
};
use std::{
    fs,
    io::{Read, Write},
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

// Closed with no response. macOS answers bytes sent after our read shutdown
// with RST, so the peer may see a reset instead of a clean EOF.
fn assert_closed_without_response(client: &mut TcpStream) {
    let mut bytes = Vec::new();
    match client.read_to_end(&mut bytes) {
        Ok(_) => assert!(bytes.is_empty()),
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
    }
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

fn exchange(request: &[u8], credential: &RunCredential, phase: Phase) -> Vec<u8> {
    let (mut client, mut server) = pair();
    client.write_all(request).unwrap();
    assert_eq!(
        handle_connection(
            &mut server,
            credential.token(),
            HOST,
            Snapshot { phase },
            Limits::default(),
            &Shutdown::new()
        ),
        Ok(())
    );
    // The caller deliberately retains its descriptor: the handler must close
    // both directions, not rely on dropping a connection that it only borrowed.
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();

    // Characterize the new driver against the same existing phase/rejection
    // matrix without replacing any assertions on the borrowed contract.
    let (mut client, server) = pair();
    client.write_all(request).unwrap();
    let mut connection = StatusConnection::new(
        server,
        credential.token(),
        HOST,
        Snapshot { phase },
        Limits::default(),
        Shutdown::new(),
    )
    .unwrap();
    let start = Instant::now();
    loop {
        match connection.poll() {
            StatusPoll::Pending => {
                assert!(start.elapsed() < Duration::from_secs(3));
                std::thread::yield_now();
            }
            StatusPoll::Finished => break,
            StatusPoll::Failed(error) => panic!("owned exchange failed: {error}"),
        }
    }
    let mut owned_response = Vec::new();
    client.read_to_end(&mut owned_response).unwrap();
    assert_eq!(owned_response, response);
    response
}

// Fill a real TCP send queue without consuming the peer's receive queue. The
// fixture itself has a time/byte cap and waits for delayed ACKs to settle.
fn fill_send_queue(server: &mut TcpStream) -> usize {
    server.set_nonblocking(true).unwrap();
    let start = Instant::now();
    let mut last_progress = start;
    let mut total = 0;
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "backpressure fixture deadline"
        );
        assert!(total < 16 * 1024 * 1024, "backpressure fixture byte cap");
        match server.write(&[b'x'; 4096]) {
            Ok(0) => panic!("unexpected write zero"),
            Ok(count) => {
                total += count;
                last_progress = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if last_progress.elapsed() >= Duration::from_millis(100) {
                    return total;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("fixture I/O kind: {:?}", error.kind()),
        }
    }
}

#[test]
fn unauthorized_response_has_only_a_static_bearer_challenge() {
    let (_directory, credential) = credential();
    let response = exchange(
        format!("GET /v1/status HTTP/1.1\r\nHost: {HOST}\r\n\r\n").as_bytes(),
        &credential,
        Phase::Ready,
    );
    let text = String::from_utf8(response).unwrap();
    assert!(text.contains("\r\nWWW-Authenticate: Bearer\r\n"));
    assert!(!text.contains(credential.token().expose_secret()));
    assert!(text.len() <= 256);
}

#[test]
fn exact_byte_boundary_zero_body_and_supported_authorities_are_accepted() {
    let (_directory, credential) = credential();
    for (host, phase, name) in [
        (HOST, Phase::Ready, "ready"),
        ("host.docker.internal:1234", Phase::Preparing, "preparing"),
        ("[::1]:65535", Phase::Stopping, "stopping"),
        ("192.0.2.1:1", Phase::RecoveryRequired, "recovery_required"),
    ] {
        // Host matching is syntax/authority validation, not a route test: all
        // these exchanges still use the independently test-owned loopback pair.
        let text = String::from_utf8(request(&credential))
            .unwrap()
            .replace(HOST, host)
            .replace("Host:", "hOsT:")
            .replace("Authorization:", "aUtHoRiZaTiOn:")
            .replace("\r\n\r\n", "\r\nContent-Length: 0\r\n\r\n");
        let padded = text.replace(
            "\r\n\r\n",
            &format!("\r\nX: {}\r\n\r\n", "x".repeat(8192 - text.len() - 5)),
        );
        assert_eq!(padded.len(), 8192);
        let (mut client, mut server) = pair();
        client.write_all(padded.as_bytes()).unwrap();
        assert_eq!(
            handle_connection(
                &mut server,
                credential.token(),
                host,
                Snapshot { phase },
                Limits {
                    max_response_bytes: 256,
                    ..Limits::default()
                },
                &Shutdown::new()
            ),
            Ok(())
        );
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with(&format!("{{\"version\":1,\"phase\":\"{name}\"}}\n")));
        assert!(response.len() <= 256);
    }
}

#[test]
fn eof_before_complete_head_rejects_without_echo() {
    let (_directory, credential) = credential();
    for head in [
        Vec::new(),
        b"GET /v1/status HTTP/1.1\r\nX: synthetic-private-marker".to_vec(),
    ] {
        let (mut client, mut server) = pair();
        client.write_all(&head).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(
            handle_connection(
                &mut server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready
                },
                Limits::default(),
                &Shutdown::new()
            ),
            Ok(())
        );
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(!response.contains("synthetic-private-marker"));
        assert!(!response.contains("phase"));
    }
}

#[test]
fn raw_bytes_oversize_and_field_overflow_never_dispatch_status() {
    let (_directory, credential) = credential();
    let good = request(&credential);
    let mut cases = Vec::new();
    for field in [b"X: \xff\r\n".as_slice(), b"X: \x7f\r\n", b"\tobs-fold\r\n"] {
        let mut head = good[..good.len() - 2].to_vec();
        head.extend_from_slice(field);
        head.extend_from_slice(b"\r\n");
        cases.push(head);
    }
    let mut oversized = good[..good.len() - 2].to_vec();
    oversized.extend_from_slice(b"X: ");
    oversized.resize(8193, b'x');
    cases.push(oversized);
    let mut too_many = good[..good.len() - 2].to_vec();
    too_many.extend_from_slice("X: x\r\n".repeat(63).as_bytes());
    too_many.extend_from_slice(b"\r\n");
    cases.push(too_many);
    for head in cases {
        let response = exchange(&head, &credential, Phase::Ready);
        assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
        assert!(!String::from_utf8(response).unwrap().contains("phase"));
    }
}

#[test]
fn real_socket_write_backpressure_obeys_deadline_and_cancellation() {
    let (_directory, credential) = credential();
    for cancel in [false, true] {
        let (mut client, mut server) = pair();
        // Request first: on macOS, peer data after the fill reopens send space.
        client.write_all(&request(&credential)).unwrap();
        let queued = fill_send_queue(&mut server);
        let shutdown = Shutdown::new();
        std::thread::scope(|scope| {
            let trigger = shutdown.clone();
            scope.spawn(move || {
                if cancel {
                    std::thread::sleep(Duration::from_millis(40));
                    trigger.request(ShutdownReason::Requested);
                }
            });
            let start = Instant::now();
            let result = handle_connection(
                &mut server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready,
                },
                Limits {
                    write_timeout: Duration::from_millis(100),
                    ..Limits::default()
                },
                &shutdown,
            );
            assert_eq!(
                result,
                Err(if cancel {
                    StatusError::Cancelled
                } else {
                    StatusError::WriteDeadline
                })
            );
            assert!(start.elapsed() < Duration::from_millis(500));
        });
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        assert_eq!(response.len(), queued);
        assert!(response.iter().all(|&byte| byte == b'x'));
    }
}

#[test]
fn shared_shutdown_closes_all_borrowed_consumers_without_dispatch() {
    let (_directory, credential) = credential();
    for pre_requested in [true, false] {
        let shutdown = Shutdown::new();
        let mut connections: Vec<_> = (0..4).map(|_| pair()).collect();
        if pre_requested {
            shutdown.request(ShutdownReason::Requested);
            for (client, _) in &mut connections {
                client.write_all(&request(&credential)).unwrap();
            }
        }
        std::thread::scope(|scope| {
            let workers: Vec<_> = connections
                .iter_mut()
                .map(|(_, server)| {
                    let shutdown = shutdown.clone();
                    let credential = &credential;
                    scope.spawn(move || {
                        handle_connection(
                            server,
                            credential.token(),
                            HOST,
                            Snapshot {
                                phase: Phase::Ready,
                            },
                            Limits::default(),
                            &shutdown,
                        )
                    })
                })
                .collect();
            if !pre_requested {
                std::thread::sleep(Duration::from_millis(40));
                shutdown.request(ShutdownReason::Terminate);
            }
            let start = Instant::now();
            for worker in workers {
                assert_eq!(worker.join().unwrap(), Err(StatusError::Cancelled));
            }
            assert!(start.elapsed() < Duration::from_millis(250));
        });
        for (mut client, _server) in connections {
            assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
        }
    }
}

#[test]
fn idle_and_slow_trickle_share_one_absolute_read_deadline() {
    let (_directory, credential) = credential();
    for trickle in [false, true] {
        let (mut client, mut server) = pair();
        server
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut sender = client.try_clone().unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                if trickle {
                    for _ in 0..20 {
                        if sender.write_all(b"G").is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            });
            let start = Instant::now();
            let result = handle_connection(
                &mut server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready,
                },
                Limits {
                    read_timeout: Duration::from_millis(80),
                    ..Limits::default()
                },
                &Shutdown::new(),
            );
            assert_eq!(result, Err(StatusError::ReadDeadline));
            assert!(start.elapsed() >= Duration::from_millis(80));
            assert!(start.elapsed() < Duration::from_millis(350));
            writer.join().unwrap();
        });
        assert_closed_without_response(&mut client);
    }
}

#[test]
fn configured_head_bounds_are_enforced_on_real_sockets() {
    let (_directory, credential) = credential();
    let head = request(&credential);
    for limits in [
        Limits {
            max_header_bytes: head.len() - 1,
            ..Limits::default()
        },
        Limits {
            max_header_fields: 1,
            ..Limits::default()
        },
    ] {
        let (mut client, mut server) = pair();
        client.write_all(&head).unwrap();
        assert_eq!(
            handle_connection(
                &mut server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready
                },
                limits,
                &Shutdown::new()
            ),
            Ok(())
        );
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    }
}

#[test]
fn fragmented_head_is_not_dispatched_until_final_crlf() {
    let (_directory, credential) = credential();
    let head = request(&credential);
    let (mut client, mut server) = pair();
    client
        .set_read_timeout(Some(Duration::from_millis(40)))
        .unwrap();
    client.write_all(&head[..head.len() - 2]).unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            handle_connection(
                &mut server,
                credential.token(),
                HOST,
                Snapshot {
                    phase: Phase::Ready,
                },
                Limits::default(),
                &Shutdown::new(),
            )
        });
        let mut byte = [0];
        assert!(
            matches!(client.read(&mut byte), Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
        );
        client.write_all(b"\r").unwrap();
        assert!(
            matches!(client.read(&mut byte), Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
        );
        client.write_all(b"\n").unwrap();
        assert_eq!(worker.join().unwrap(), Ok(()));
    });
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
}

#[test]
fn invalid_limits_are_rejected_before_reading_even_an_idle_socket() {
    let (_directory, credential) = credential();
    let default = Limits::default();
    for limits in [
        Limits {
            max_header_bytes: 0,
            ..default
        },
        Limits {
            max_header_bytes: 8193,
            ..default
        },
        Limits {
            max_header_fields: 0,
            ..default
        },
        Limits {
            max_header_fields: 65,
            ..default
        },
        Limits {
            max_response_bytes: 255,
            ..default
        },
        Limits {
            max_response_bytes: 1025,
            ..default
        },
        Limits {
            read_timeout: Duration::ZERO,
            ..default
        },
        Limits {
            read_timeout: Duration::MAX,
            ..default
        },
        Limits {
            write_timeout: Duration::ZERO,
            ..default
        },
        Limits {
            write_timeout: Duration::MAX,
            ..default
        },
    ] {
        let (mut client, mut server) = pair();
        server
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let error = handle_connection(
            &mut server,
            credential.token(),
            HOST,
            Snapshot {
                phase: Phase::Ready,
            },
            limits,
            &Shutdown::new(),
        )
        .unwrap_err();
        assert_eq!(error, StatusError::InvalidLimits);
        assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
    }
}

#[test]
fn invalid_host_is_rejected_before_reading_even_an_idle_socket() {
    let (_directory, credential) = credential();
    for host in [
        "",
        "localhost",
        "http://localhost:43127",
        "localhost:0",
        "localhost:01",
        "localhost:65536",
        "localhost:43127/",
        "user@localhost:43127",
        "localhost:43127?synthetic-private-marker",
        "localhost:43127\r\nX: y",
        "LOCALHOST:43127",
        "0.0.0.0:43127",
        "224.1.2.3:43127",
        "127.01.0.1:43127",
    ] {
        let (mut client, mut server) = pair();
        server
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let error = handle_connection(
            &mut server,
            credential.token(),
            host,
            Snapshot {
                phase: Phase::Ready,
            },
            Limits::default(),
            &Shutdown::new(),
        )
        .unwrap_err();
        assert_eq!(error, StatusError::InvalidHost);
        assert_eq!(error.to_string(), "invalid status host authority");
        assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
    }
}

#[test]
fn rejected_requests_have_static_responses_never_status_or_echo() {
    let (_directory, credential) = credential();
    let good = String::from_utf8(request(&credential)).unwrap();
    let auth = format!(
        "Authorization: Bearer {}\r\n",
        credential.token().expose_secret()
    );
    let mut cases = vec![
        (good.replace(&auth, ""), "401 Unauthorized"),
        (
            good.replace(credential.token().expose_secret(), &"b".repeat(64)),
            "401 Unauthorized",
        ),
        (
            good.replace(&auth, &format!("{auth}{auth}")),
            "401 Unauthorized",
        ),
        (
            good.replace("/v1/status", "/v1/status?token=synthetic-private-marker"),
            "400 Bad Request",
        ),
        (
            good.replace("/v1/status", "/synthetic-private-marker"),
            "400 Bad Request",
        ),
        (good.replace("GET ", "POST "), "400 Bad Request"),
        (good.replace("HTTP/1.1", "HTTP/1.0"), "400 Bad Request"),
        (good.replace(HOST, "other:43127"), "400 Bad Request"),
    ];
    for field in [
        "Origin: synthetic-private-marker",
        "Transfer-Encoding: chunked",
        "Content-Length: 8",
        "Host: localhost:43127",
        "Bad Name: synthetic-private-marker",
        " X: folded",
        "X: raw\0byte",
        "X: raw\tbyte",
        "X: raw\nbyte",
        "Content-Length: 0\r\nContent-Length: 0",
    ] {
        cases.push((
            good.replace("\r\n\r\n", &format!("\r\n{field}\r\n\r\n")),
            "400 Bad Request",
        ));
    }
    cases.push((format!("{good}synthetic-private-marker"), "400 Bad Request"));
    cases.push((format!("{good}{good}"), "400 Bad Request"));
    for (head, status) in cases {
        let response = exchange(head.as_bytes(), &credential, Phase::Ready);
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with(&format!("HTTP/1.1 {status}\r\n")));
        assert_eq!(text.matches("HTTP/1.1").count(), 1);
        assert!(!text.contains("phase"));
        assert!(!text.contains("synthetic-private-marker"));
        assert!(!text.contains(credential.token().expose_secret()));
        assert!(!text.to_ascii_lowercase().contains("access-control-"));
        assert!(text.len() <= 1024);
    }
}

#[test]
fn all_snapshot_phases_have_only_version_and_static_phase() {
    let (_directory, credential) = credential();
    for (phase, expected) in [
        (Phase::Preparing, "preparing"),
        (Phase::Ready, "ready"),
        (Phase::Stopping, "stopping"),
        (Phase::RecoveryRequired, "recovery_required"),
    ] {
        let response = exchange(&request(&credential), &credential, phase);
        let text = String::from_utf8(response).unwrap();
        let (_, body) = text.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            body,
            format!("{{\"version\":1,\"phase\":\"{expected}\"}}\n")
        );
    }
}

#[test]
fn valid_status_is_fixed_schema_bounded_and_closes_borrowed_socket() {
    let (_directory, credential) = credential();
    let response = exchange(&request(&credential), &credential, Phase::Ready);
    assert!(response.len() <= 1024);
    let response = String::from_utf8(response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.contains("\r\nConnection: close"));
    assert!(headers.contains("\r\nCache-Control: no-store"));
    assert!(headers.contains(&format!("\r\nContent-Length: {}", body.len())));
    assert_eq!(body, "{\"version\":1,\"phase\":\"ready\"}\n");
    assert!(!response.contains(credential.token().expose_secret()));
    assert!(!response.to_ascii_lowercase().contains("access-control-"));
}

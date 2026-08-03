use super::*;

#[test]
fn classify_probe_spawn_maps_to_exit_1_and_not_found_token() {
    let e = ProbeError::Spawn(std::io::Error::from(std::io::ErrorKind::NotFound));
    let (code, msg) = classify_probe(&e);
    assert_eq!(code, 1);
    assert!(
        msg.contains("docker not found in PATH"),
        "missing 'docker not found in PATH' token: {msg}"
    );
}

#[test]
fn classify_probe_unreachable_maps_to_exit_126_and_docker_desktop_token() {
    let e = ProbeError::Unreachable {
        code: Some(1),
        stderr: "cannot connect".into(),
    };
    let (code, msg) = classify_probe(&e);
    assert_eq!(code, 126);
    assert!(
        msg.contains("start Docker Desktop"),
        "missing 'start Docker Desktop' token: {msg}"
    );
}

#[test]
fn classify_probe_timeout_maps_to_exit_126_and_docker_desktop_token() {
    let e = ProbeError::Timeout(std::time::Duration::from_secs(3));
    let (code, msg) = classify_probe(&e);
    assert_eq!(code, 126);
    assert!(
        msg.contains("start Docker Desktop"),
        "missing 'start Docker Desktop' token: {msg}"
    );
}

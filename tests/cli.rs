use std::fs;

use assert_cmd::Command;
use tempfile::tempdir;

const MALFORMED: &str = "toolchains:\n  dotnet: \"10.0\"\nextras: [unclosed\n";
const VALID: &str = "toolchains: {}\n";

#[test]
fn cli_exit_2_on_parse_error() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), MALFORMED).unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("line "),
        "stderr missing 'line N' phrase: {stderr}"
    );
}

#[test]
fn cli_reads_from_cwd() {
    // Arrange
    let good = tempdir().unwrap();
    let bad = tempdir().unwrap();
    fs::write(good.path().join(".pithos"), VALID).unwrap();
    fs::write(bad.path().join(".pithos"), MALFORMED).unwrap();

    // Act
    let good_code = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&good)
        .assert()
        .get_output()
        .status
        .code();
    let bad_code = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&bad)
        .assert()
        .get_output()
        .status
        .code();

    // Assert
    assert_eq!(good_code, Some(0), "valid config should exit 0");
    assert_eq!(bad_code, Some(2), "malformed config should exit 2");
}

#[test]
fn cli_exit_2_when_pithos_missing_with_minimal_example_hint() {
    // Arrange
    let td = tempdir().unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".pithos not found"),
        "stderr missing '.pithos not found' phrase: {stderr}"
    );
    assert!(
        stderr.contains("toolchains: {}"),
        "stderr missing 'toolchains: {{}}' minimal example: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn cli_exit_2_on_missing_toolchains() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), "{}\n").unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("missing required key"),
        "stderr missing 'missing required key' phrase: {stderr}"
    );
    assert!(
        stderr.contains("toolchains"),
        "stderr missing 'toolchains' phrase: {stderr}"
    );
}

#[test]
fn cli_exit_2_on_unknown_toolchain() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(
        td.path().join(".pithos"),
        "toolchains:\n  python: \"3.12\"\n",
    )
    .unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("python"),
        "stderr missing offending toolchain name 'python': {stderr}"
    );
    assert!(
        stderr.contains("dotnet") && stderr.contains("rust") && stderr.contains("go"),
        "stderr missing valid toolchain names: {stderr}"
    );
}

#[test]
fn cli_exit_2_on_floating_version() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(
        td.path().join(".pithos"),
        "toolchains:\n  rust: \"stable\"\n",
    )
    .unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("stable"),
        "stderr missing offending value 'stable': {stderr}"
    );
}

#[test]
fn cli_exit_2_on_invalid_apt_name() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(
        td.path().join(".pithos"),
        "toolchains: {}\nextras:\n  apt: [Git]\n",
    )
    .unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("Git"),
        "stderr missing offending entry 'Git': {stderr}"
    );
}

#[test]
fn cli_exit_2_on_unknown_top_level_key() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), "toolchains: {}\nbogus: 1\n").unwrap();

    // Act
    let assert = Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("bogus"),
        "stderr missing offending key 'bogus': {stderr}"
    );
    assert!(
        stderr.contains("toolchains"),
        "stderr missing valid-key 'toolchains': {stderr}"
    );
}

#[test]
fn cli_writes_dockerfile_to_pithos_d() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), VALID).unwrap();

    // Act
    Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(0);

    // Assert
    let dockerfile = td.path().join(".pithos.d").join("Dockerfile");
    assert!(
        dockerfile.exists(),
        "expected .pithos.d/Dockerfile to exist at {}",
        dockerfile.display()
    );
    let content = fs::read_to_string(&dockerfile).unwrap();
    assert!(
        content.contains("Generated by pithos"),
        "Dockerfile content missing 'Generated by pithos' header: {content}"
    );
}

#[test]
fn cli_overwrites_existing_dockerfile() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), VALID).unwrap();
    let dir = td.path().join(".pithos.d");
    fs::create_dir_all(&dir).unwrap();
    let dockerfile = dir.join("Dockerfile");
    fs::write(&dockerfile, "STALE\n").unwrap();

    // Act
    Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(0);

    // Assert
    let content = fs::read_to_string(&dockerfile).unwrap();
    assert!(
        !content.contains("STALE"),
        "Dockerfile still contains stale content: {content}"
    );
    assert!(
        content.contains("Generated by pithos"),
        "Dockerfile content missing 'Generated by pithos' header: {content}"
    );
}

#[test]
fn cli_skips_dockerfile_on_config_error() {
    // Arrange
    let td = tempdir().unwrap();
    fs::write(td.path().join(".pithos"), MALFORMED).unwrap();

    // Act
    Command::cargo_bin("pithos")
        .unwrap()
        .current_dir(&td)
        .assert()
        .code(2);

    // Assert
    let dockerfile = td.path().join(".pithos.d").join("Dockerfile");
    assert!(
        !dockerfile.exists(),
        "expected .pithos.d/Dockerfile to NOT exist on config error, but found it at {}",
        dockerfile.display()
    );
}

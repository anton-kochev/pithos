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

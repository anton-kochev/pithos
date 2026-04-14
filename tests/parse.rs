use pithos::config::{load, ConfigError};

#[test]
fn valid_parses() {
    // Arrange
    let bytes = include_bytes!("fixtures/valid.pithos");

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn syntax_error_pins_line() {
    // Arrange
    let bytes = include_bytes!("fixtures/syntax_err.pithos");

    // Act
    let err = load(bytes).expect_err("expected parse error");

    // Assert
    let ConfigError::Parse { line, msg, .. } = err else {
        panic!("got {err:?}")
    };
    assert!(
        line >= 3,
        "expected error near line 3, got line {line}: {msg}"
    );
}

#[test]
fn non_utf8_rejected() {
    // Arrange
    let bytes: &[u8] = &[0xFF, 0xFE, 0x00, b'x'];

    // Act
    let err = load(bytes).expect_err("expected non-utf8 error");

    // Assert
    assert!(matches!(err, ConfigError::NotUtf8), "got {err:?}");
}

#[test]
fn missing_toolchains_rejected() {
    // Arrange
    let bytes = b"{}\n";

    // Act
    let err = load(bytes).expect_err("expected missing-toolchains error");

    // Assert
    assert!(matches!(err, ConfigError::MissingToolchains), "got {err:?}");
}

#[test]
fn empty_document_rejected() {
    // Arrange
    let bytes = b"";

    // Act
    let err = load(bytes).expect_err("expected missing-toolchains error");

    // Assert
    assert!(matches!(err, ConfigError::MissingToolchains), "got {err:?}");
}

#[test]
fn non_mapping_document_rejected() {
    // Arrange
    let bytes = b"- 1\n- 2\n";

    // Act
    let err = load(bytes).expect_err("expected missing-toolchains error");

    // Assert
    assert!(matches!(err, ConfigError::MissingToolchains), "got {err:?}");
}

#[test]
fn unknown_top_level_key_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nfoo: 1\n";

    // Act
    let err = load(bytes).expect_err("expected unknown-top-level-key error");

    // Assert
    let ConfigError::UnknownTopLevelKey { key, .. } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(key, "foo");
}

#[test]
fn empty_toolchains_mapping_parses() {
    // Arrange
    let bytes = b"toolchains: {}\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

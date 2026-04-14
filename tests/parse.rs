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

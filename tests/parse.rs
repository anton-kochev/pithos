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

#[test]
fn non_string_top_level_key_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\n123: x\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-top-level-key error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringTopLevelKey),
        "got {err:?}"
    );
}

#[test]
fn toolchains_scalar_rejected() {
    // Arrange
    let bytes = b"toolchains: foo\n";

    // Act
    let err = load(bytes).expect_err("expected toolchains-not-mapping error");

    // Assert
    assert!(
        matches!(err, ConfigError::ToolchainsNotMapping),
        "got {err:?}"
    );
}

#[test]
fn toolchains_sequence_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  - dotnet\n  - rust\n";

    // Act
    let err = load(bytes).expect_err("expected toolchains-not-mapping error");

    // Assert
    assert!(
        matches!(err, ConfigError::ToolchainsNotMapping),
        "got {err:?}"
    );
}

#[test]
fn valid_toolchain_versions_parse() {
    // Arrange
    let bytes = b"toolchains:\n  dotnet: \"10.0.102\"\n  rust: \"1.85.0\"\n  go: \"1\"\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn unknown_toolchain_name_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  python: \"3.12\"\n";

    // Act
    let err = load(bytes).expect_err("expected unknown-toolchain error");

    // Assert
    let ConfigError::UnknownToolchain { name } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(name, "python");
}

#[test]
fn unknown_toolchain_error_message_lists_valid_names() {
    // Arrange
    let bytes = b"toolchains:\n  python: \"3.12\"\n";

    // Act
    let err = load(bytes).expect_err("expected unknown-toolchain error");

    // Assert
    let msg = err.to_string();
    for expected in ["dotnet", "rust", "go"] {
        assert!(
            msg.contains(expected),
            "error message missing valid name `{expected}`: {msg}"
        );
    }
}

#[test]
fn invalid_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  dotnet: \"1.2.3.4\"\n";

    // Act
    let err = load(bytes).expect_err("expected invalid-version error");

    // Assert
    let ConfigError::InvalidVersion { toolchain, value } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "dotnet");
    assert_eq!(value, "1.2.3.4");
}

#[test]
fn non_numeric_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: \"1.85-beta\"\n";

    // Act
    let err = load(bytes).expect_err("expected invalid-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::InvalidVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn floating_stable_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: \"stable\"\n";

    // Act
    let err = load(bytes).expect_err("expected floating-version error");

    // Assert
    let ConfigError::FloatingVersionRejected { toolchain, value } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "rust");
    assert_eq!(value, "stable");
}

#[test]
fn floating_nightly_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: \"nightly\"\n";

    // Act
    let err = load(bytes).expect_err("expected floating-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::FloatingVersionRejected { .. }),
        "got {err:?}"
    );
}

#[test]
fn floating_latest_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  dotnet: \"latest\"\n";

    // Act
    let err = load(bytes).expect_err("expected floating-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::FloatingVersionRejected { .. }),
        "got {err:?}"
    );
}

#[test]
fn floating_version_message_suggests_exact_version() {
    // Arrange
    let bytes = b"toolchains:\n  rust: \"stable\"\n";

    // Act
    let err = load(bytes).expect_err("expected floating-version error");

    // Assert
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("exact"),
        "error message should suggest an exact version: {msg}"
    );
}

#[test]
fn non_string_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: 1.85\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let ConfigError::NonStringVersion { toolchain } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "rust");
}

#[test]
fn null_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust:\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let ConfigError::NonStringVersion { toolchain } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "rust");
}

#[test]
fn explicit_null_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: ~\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn unquoted_integer_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: 10\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let ConfigError::NonStringVersion { toolchain } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "rust");
}

#[test]
fn unquoted_negative_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: -1\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn unquoted_float_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  dotnet: 10.0\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let ConfigError::NonStringVersion { toolchain } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "dotnet");
}

#[test]
fn unquoted_scientific_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  dotnet: 1e5\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn unquoted_octal_like_version_rejected() {
    // Arrange — oracle test: saphyr behavior dictates outcome
    let bytes = b"toolchains:\n  dotnet: 010\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn explicit_true_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: true\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn explicit_false_version_rejected() {
    // Arrange
    let bytes = b"toolchains:\n  rust: false\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringVersion { .. }),
        "got {err:?}"
    );
}

#[test]
fn multi_toolchain_names_offending_key() {
    // Arrange — first toolchain valid, second has bad version
    let bytes = b"toolchains:\n  rust: \"1.85.0\"\n  dotnet: 10.0\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let ConfigError::NonStringVersion { toolchain } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(toolchain, "dotnet");
}

#[test]
fn non_string_version_message_instructs_quoting() {
    // Arrange
    let bytes = b"toolchains:\n  rust: 1.85\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-version error");

    // Assert
    let msg = err.to_string();
    let msg_lower = msg.to_lowercase();
    assert!(
        msg_lower.contains("toolchains.rust"),
        "message should name key path: {msg}"
    );
    assert!(
        msg_lower.contains("quote"),
        "message should tell user to quote: {msg}"
    );
    assert!(
        msg.contains('"'),
        "message should include a quoted example: {msg}"
    );
}

#[test]
fn non_string_toolchain_key_rejected() {
    // Arrange — numeric key inside toolchains mapping
    let bytes = b"toolchains:\n  42: \"1.0\"\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-toolchain-key error");

    // Assert
    assert!(
        matches!(err, ConfigError::NonStringToolchainKey),
        "got {err:?}"
    );
}

#[test]
fn extras_empty_mapping_ok() {
    // Arrange
    let bytes = b"toolchains: {}\nextras: {}\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn extras_null_ok() {
    // Arrange
    let bytes = b"toolchains: {}\nextras: ~\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn extras_apt_null_ok() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: ~\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn extras_apt_empty_sequence_ok() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: []\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn extras_apt_valid_entries_ok() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: [git, libssl3, g++]\n";

    // Act
    let result = load(bytes);

    // Assert
    result.unwrap();
}

#[test]
fn extras_not_mapping_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras: foo\n";

    // Act
    let err = load(bytes).expect_err("expected extras-not-mapping error");

    // Assert
    assert!(matches!(err, ConfigError::ExtrasNotMapping), "got {err:?}");
}

#[test]
fn extras_apt_not_sequence_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: foo\n";

    // Act
    let err = load(bytes).expect_err("expected apt-not-sequence error");

    // Assert
    assert!(matches!(err, ConfigError::AptNotSequence), "got {err:?}");
}

#[test]
fn extras_apt_entry_not_string_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: [1]\n";

    // Act
    let err = load(bytes).expect_err("expected apt-entry-not-string error");

    // Assert
    let ConfigError::AptEntryNotString { index } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(index, 0);
}

#[test]
fn extras_apt_invalid_name_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  apt: [Git]\n";

    // Act
    let err = load(bytes).expect_err("expected invalid-apt-package-name error");

    // Assert
    let msg = err.to_string();
    let ConfigError::InvalidAptPackageName { entry } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(entry, "Git");
    assert!(
        msg.contains("Git"),
        "error message should name the offending entry: {msg}"
    );
}

#[test]
fn extras_unknown_nested_key_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  aptt: [git]\n";

    // Act
    let err = load(bytes).expect_err("expected unknown-extras-key error");

    // Assert
    let msg = err.to_string();
    let ConfigError::UnknownExtrasKey { key } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(key, "aptt");
    assert!(
        msg.contains("apt"),
        "error message should list valid key `apt`: {msg}"
    );
}

#[test]
fn extras_non_string_nested_key_rejected() {
    // Arrange
    let bytes = b"toolchains: {}\nextras:\n  42: foo\n";

    // Act
    let err = load(bytes).expect_err("expected non-string-extras-key error");

    // Assert
    assert!(matches!(err, ConfigError::NonStringExtrasKey), "got {err:?}");
}

#[test]
fn extras_apt_first_offender_wins() {
    // Arrange — first entry valid, second invalid, third also invalid
    let bytes = b"toolchains: {}\nextras:\n  apt: [git, BAD, also_bad]\n";

    // Act
    let err = load(bytes).expect_err("expected invalid-apt-package-name error");

    // Assert
    let ConfigError::InvalidAptPackageName { entry } = err else {
        panic!("got {err:?}")
    };
    assert_eq!(entry, "BAD");
}

#[test]
fn first_invalid_toolchain_wins() {
    // Arrange — unknown toolchain comes first, invalid version second
    let bytes = b"toolchains:\n  python: \"3.12\"\n  dotnet: \"latest\"\n";

    // Act
    let err = load(bytes).expect_err("expected unknown-toolchain error");

    // Assert
    assert!(
        matches!(err, ConfigError::UnknownToolchain { .. }),
        "first offender should win, got {err:?}"
    );
}

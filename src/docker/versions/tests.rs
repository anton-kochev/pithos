use super::*;

#[test]
fn assemble_extract_run_args_emits_rm_and_entrypoint_sh() {
    let args = assemble_extract_run_args("pithos:demo", &["dotnet".into()]);
    assert!(args.contains(&OsString::from("--rm")));
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--entrypoint" && w[1] == "sh"),
        "missing --entrypoint sh pair in {args:?}"
    );
}

#[test]
fn assemble_extract_run_args_image_tag_precedes_dash_c() {
    let args = assemble_extract_run_args("pithos:demo", &["dotnet".into()]);
    let tag_idx = args
        .iter()
        .position(|a| a == "pithos:demo")
        .expect("tag present");
    let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
    assert!(tag_idx < c_idx, "image tag must precede -c in {args:?}");
}

#[test]
fn assemble_extract_run_args_passes_toolchains_as_positionals_not_interpolated() {
    let args = assemble_extract_run_args("pithos:demo", &["dotnet".into(), "rust".into()]);
    // Find the -c string and assert no toolchain name is baked into it.
    let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
    let script = args.get(c_idx + 1).expect("-c has value").clone();
    let script_s = script.to_str().expect("script is utf8");
    assert!(
        !script_s.contains("dotnet"),
        "toolchain name leaked into -c script: {script_s:?}"
    );
    assert!(
        !script_s.contains("rust"),
        "toolchain name leaked into -c script: {script_s:?}"
    );
    // And each toolchain appears as its own argv entry.
    assert!(args.contains(&OsString::from("dotnet")));
    assert!(args.contains(&OsString::from("rust")));
}

#[test]
fn assemble_extract_run_args_places_sh_dollar_zero_before_positionals() {
    let args = assemble_extract_run_args("pithos:demo", &["dotnet".into(), "rust".into()]);
    // Contract: `-c <SCRIPT> sh <tc1> <tc2>` — the "sh" is $0 to the shell.
    let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
    assert_eq!(args.get(c_idx + 2), Some(&OsString::from("sh")));
    assert_eq!(args.get(c_idx + 3), Some(&OsString::from("dotnet")));
    assert_eq!(args.get(c_idx + 4), Some(&OsString::from("rust")));
}

// parse_versions_stdout — pure parser

#[test]
fn parse_versions_stdout_happy_path() {
    let expected: Vec<String> = vec!["dotnet".into(), "rust".into()];
    let out = parse_versions_stdout("dotnet=10.0.102\nrust=1.85.0\n", &expected).unwrap();
    assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
    assert_eq!(out.get("rust").map(String::as_str), Some("1.85.0"));
    assert_eq!(out.len(), 2);
}

#[test]
fn parse_versions_stdout_tolerates_blank_lines() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("\ndotnet=10.0.102\n\n", &expected).unwrap();
    assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
}

#[test]
fn parse_versions_stdout_trims_whitespace_around_name_and_value() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("  dotnet  =  10.0.102  \n", &expected).unwrap();
    assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
}

#[test]
fn parse_versions_stdout_tolerates_crlf() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("dotnet=10.0.102\r\n", &expected).unwrap();
    assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
}

#[test]
fn parse_versions_stdout_empty_value_errors() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let err = parse_versions_stdout("dotnet=\n", &expected).unwrap_err();
    match err {
        ExtractError::EmptyValue(name) => assert_eq!(name, "dotnet"),
        other => panic!("expected EmptyValue, got {other:?}"),
    }
}

#[test]
fn parse_versions_stdout_whitespace_only_value_errors() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let err = parse_versions_stdout("dotnet=   \n", &expected).unwrap_err();
    assert!(matches!(err, ExtractError::EmptyValue(_)));
}

#[test]
fn parse_versions_stdout_missing_expected_name_errors() {
    let expected: Vec<String> = vec!["dotnet".into(), "rust".into()];
    let err = parse_versions_stdout("dotnet=10.0.102\n", &expected).unwrap_err();
    match err {
        ExtractError::MissingEntry(name) => assert_eq!(name, "rust"),
        other => panic!("expected MissingEntry, got {other:?}"),
    }
}

#[test]
fn parse_versions_stdout_ignores_unexpected_names() {
    // Forward compat: a future installer might write extras the
    // launcher has no label key for — must not break today.
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("dotnet=10.0.102\npython=3.12.0\n", &expected).unwrap();
    assert_eq!(out.len(), 1);
    assert!(out.contains_key("dotnet"));
    assert!(!out.contains_key("python"));
}

#[test]
fn parse_versions_stdout_duplicate_name_last_wins() {
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("dotnet=10.0.101\ndotnet=10.0.102\n", &expected).unwrap();
    assert_eq!(out.get("dotnet").map(String::as_str), Some("10.0.102"));
}

#[test]
fn parse_versions_stdout_splits_on_first_equals_only() {
    // A future installer could emit a value containing `=` (build metadata,
    // embedded config). Protect that invariant.
    let expected: Vec<String> = vec!["dotnet".into()];
    let out = parse_versions_stdout("dotnet=10.0.102+build=7\n", &expected).unwrap();
    assert_eq!(
        out.get("dotnet").map(String::as_str),
        Some("10.0.102+build=7")
    );
}

// classify_probe — pure (exit code, message) mapping for the daemon probe.
// Locks the 126 / "start Docker Desktop" contract (NFR-12 / T-504) and the
// pre-6.5 "docker-missing → exit 1" contract via unconditional pure tests —
// no docker-on-PATH needed, unlike the gated integration test in tests/cli.rs.

#[test]
fn toolchain_ordering_is_sorted_end_to_end() {
    let mut names: Vec<String> = vec!["rust".into(), "dotnet".into()];
    names.sort();
    assert_eq!(names, vec!["dotnet".to_string(), "rust".to_string()]);

    let args = assemble_extract_run_args("pithos:demo", &names);
    let c_idx = args.iter().position(|a| a == "-c").expect("-c present");
    // Positional order mirrors sorted input.
    assert_eq!(args.get(c_idx + 3), Some(&OsString::from("dotnet")));
    assert_eq!(args.get(c_idx + 4), Some(&OsString::from("rust")));

    let parsed = parse_versions_stdout("rust=1.85.0\ndotnet=10.0.102\n", &names).unwrap();
    // BTreeMap iterates sort-by-key regardless of insertion/stdout order.
    let keys: Vec<&String> = parsed.keys().collect();
    assert_eq!(keys, vec![&"dotnet".to_string(), &"rust".to_string()]);
}

use pithos::config::load;

#[test]
fn browser_key_presence_requires_cli_migration_for_every_value() {
    for value in [
        "",
        "null",
        "false",
        "true",
        "[]",
        "{}",
        "{enabled: false}",
        "{enabled: true, mode: headless}",
        "{mode: invalid}",
        "{1: [secret-canary]}",
    ] {
        let text = format!("toolchains: {{}}\nbrowser: {value}\n");
        let error = load(text.as_bytes())
            .expect_err("obsolete browser key accepted")
            .to_string();
        assert!(error.contains("remove `browser` from .pithos"), "{error}");
        assert!(error.contains("pithos [run] --browser"), "{error}");
        assert!(
            error.contains("--browser=interactive") && error.contains("--browser=headless"),
            "{error}"
        );
        assert!(
            !error.contains("secret-canary"),
            "values must not be echoed: {error}"
        );
    }
}

#[test]
fn explicit_client_layer_changes_emission_and_legacy_key_without_mode_splitting() {
    use pithos::browser::{BrowserClientLayer, BrowserMode, BrowserSelection};
    let raw = b"toolchains: {}\n";
    let yaml = load(raw).unwrap();
    let absent = pithos::dockerfile::emit_with_browser(&yaml, BrowserClientLayer::Absent);
    let included = pithos::dockerfile::emit_with_browser(&yaml, BrowserClientLayer::Included);
    assert_eq!(absent, pithos::dockerfile::emit(&yaml));
    assert!(!absent.contains("/opt/pithos-browser"));
    assert!(included.contains("/opt/pithos-browser"));
    assert!(included.contains(&pithos::browser::assets::fingerprint()));
    assert!(!included.contains("COPY browser/skills"));
    let hash = |text: &str| {
        pithos::fingerprint::compute(
            text,
            raw,
            &Default::default(),
            b"compat",
            b"entry",
            "sha256:base",
        )
    };
    assert_ne!(hash(&absent), hash(&included));
    for mode in [BrowserMode::Interactive, BrowserMode::Headless] {
        let selected = pithos::dockerfile::emit_with_browser(
            &yaml,
            BrowserSelection::Enabled(mode).client_layer(),
        );
        assert_eq!(hash(&selected), hash(&included));
    }
    let identity = pithos::docker::HostIdentity::new(501, 20).unwrap();
    assert_eq!(
        pithos::dockerfile::emit_with_identity(&yaml, identity),
        pithos::dockerfile::emit_with_identity_and_browser(
            &yaml,
            identity,
            BrowserClientLayer::Absent
        )
    );
    assert!(
        pithos::dockerfile::emit_with_identity_and_browser(
            &yaml,
            identity,
            BrowserClientLayer::Included
        )
        .contains(&included)
    );
}

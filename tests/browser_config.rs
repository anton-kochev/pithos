use pithos::config::{BrowserMode, browser_config, load};

#[test]
fn absent_and_disabled_are_default_off() {
    for text in [
        "toolchains: {}",
        "toolchains: {}\nbrowser: {}",
        "toolchains: {}\nbrowser: {enabled: false}",
    ] {
        let config = browser_config(&load(text.as_bytes()).unwrap()).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.mode, BrowserMode::Interactive);
    }
}

#[test]
fn enabled_defaults_interactive_and_accepts_headless() {
    for (suffix, mode) in [
        ("", BrowserMode::Interactive),
        (", mode: headless", BrowserMode::Headless),
    ] {
        let text = format!("toolchains: {{}}\nbrowser: {{enabled: true{suffix}}}");
        let config = browser_config(&load(text.as_bytes()).unwrap()).unwrap();
        assert!(config.enabled);
        assert_eq!(config.mode, mode);
    }
}

#[test]
fn malformed_browser_is_rejected_even_when_disabled() {
    for browser in [
        "null",
        "true",
        "[]",
        "{enabled: 'true'}",
        "{enabled: 1}",
        "{mode: null}",
        "{mode: false}",
        "{mode: []}",
        "{enabled: false, mode: unknown}",
        "{image: arbitrary}",
        "{1: true}",
    ] {
        let text = format!("toolchains: {{}}\nbrowser: {browser}");
        assert!(load(text.as_bytes()).is_err(), "accepted {browser}");
    }
}

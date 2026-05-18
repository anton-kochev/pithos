/// Shared predicate for numeric dotted versions accepted across `.pithos`
/// keys (`toolchains.*`, `pi.version`). Centralised so the accepted shape
/// cannot silently diverge between fields — change the policy here, both
/// callers follow.
///
/// Accepts `N`, `N.N`, `N.N.N` where each segment is one or more ASCII
/// digits. Empty segments and trailing/leading dots are rejected.
/// Floating keywords (`stable`, `nightly`, `latest`) are handled separately
/// by each caller before this predicate runs, so they're not covered here.
pub(super) fn is_valid_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::is_valid_version;

    #[test]
    fn accepts_one_two_three_segment_numeric_versions() {
        assert!(is_valid_version("1"));
        assert!(is_valid_version("10"));
        assert!(is_valid_version("1.85"));
        assert!(is_valid_version("0.75"));
        assert!(is_valid_version("0.75.3"));
        assert!(is_valid_version("10.0.102"));
    }

    #[test]
    fn rejects_four_segment_versions() {
        assert!(!is_valid_version("1.2.3.4"));
        assert!(!is_valid_version("0.75.3.1"));
    }

    #[test]
    fn rejects_non_digit_segments() {
        assert!(!is_valid_version("1.85-beta"));
        assert!(!is_valid_version("0.75-beta"));
        assert!(!is_valid_version("v1.2"));
        assert!(!is_valid_version("v0.75"));
    }

    #[test]
    fn rejects_empty_and_malformed_separators() {
        assert!(!is_valid_version(""));
        assert!(!is_valid_version(".1"));
        assert!(!is_valid_version("1."));
        assert!(!is_valid_version("1..2"));
        assert!(!is_valid_version("0..75"));
    }
}

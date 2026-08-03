use super::*;

fn build_request(extra_labels: &BTreeMap<String, String>) -> BuildRequest<'_> {
    BuildRequest {
        context: Path::new("/ctx"),
        dockerfile: Path::new("/ctx/Dockerfile"),
        project: "demo",
        fingerprint: "abc123",
        extra_labels,
    }
}

#[test]
fn merge_tails_both_empty_returns_empty() {
    let out: Vec<String> = merge_tails(vec![], vec![], 20);
    assert!(out.is_empty());
}

#[test]
fn merge_tails_under_cap_preserves_all_in_order() {
    // Stderr first, stdout last — reconstructs "stderr emitted progress,
    // stdout emitted the image id at the end".
    let out = merge_tails(vec!["a".into(), "b".into()], vec!["c".into()], 20);
    assert_eq!(out, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn merge_tails_over_cap_truncates_from_front_to_last_n() {
    let out = merge_tails(
        vec!["a".into(), "b".into(), "c".into()],
        vec!["d".into(), "e".into()],
        3,
    );
    assert_eq!(out, vec!["c".to_string(), "d".to_string(), "e".to_string()]);
}

#[test]
fn assemble_build_args_emits_fingerprint_label_when_extras_empty() {
    let args = assemble_build_args(build_request(&BTreeMap::new()));
    // Exactly one --label, carrying the fingerprint.
    assert_eq!(
        args.iter().filter(|a| *a == "--label").count(),
        1,
        "expected exactly one --label with empty extras, got {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--label" && w[1] == "dev.pithos.fingerprint=abc123"),
        "missing --label dev.pithos.fingerprint=abc123 in {args:?}"
    );
}

#[test]
fn assemble_build_args_includes_core_flags_and_positionals() {
    let args = assemble_build_args(build_request(&BTreeMap::new()));
    assert_eq!(args.first(), Some(&OsString::from("build")));
    assert!(args.contains(&OsString::from("--pull")));
    assert!(args.contains(&OsString::from("--progress=plain")));
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-f" && w[1] == "/ctx/Dockerfile"),
        "missing -f /ctx/Dockerfile pair in {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--tag" && w[1] == "pithos:demo"),
        "missing --tag pithos:demo pair in {args:?}"
    );
    // Context path is the final positional.
    assert_eq!(args.last(), Some(&OsString::from("/ctx")));
}

#[test]
fn assemble_base_build_args_uses_dockerfile_base_and_base_tag() {
    let args = assemble_base_build_args(Path::new("/srv/pithos"));
    assert_eq!(args.first(), Some(&OsString::from("build")));
    assert!(args.contains(&OsString::from("--progress=plain")));
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-f" && w[1] == "Dockerfile.base"),
        "missing -f Dockerfile.base pair in {args:?}"
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-t" && w[1] == BASE_IMAGE_REF),
        "missing -t <BASE_IMAGE_REF> pair in {args:?}"
    );
    // Context path is the final positional.
    assert_eq!(args.last(), Some(&OsString::from("/srv/pithos")));
}

#[test]
fn assemble_base_build_args_emits_no_labels() {
    // The base image carries no pithos labels — those are per-project.
    let args = assemble_base_build_args(Path::new("/srv/pithos"));
    assert!(
        !args.contains(&OsString::from("--label")),
        "base build should not emit --label, got {args:?}"
    );
}

#[test]
fn assemble_build_args_renders_extra_labels_in_btreemap_order() {
    // Insertion order reversed — if someone swaps BTreeMap for HashMap,
    // order becomes non-deterministic and this test flakes.
    let mut extras: BTreeMap<String, String> = BTreeMap::new();
    extras.insert("dev.pithos.rust-version".into(), "1.85.0".into());
    extras.insert("dev.pithos.dotnet-version".into(), "10.0.102".into());
    let args = assemble_build_args(build_request(&extras));
    // Three --label args total: fingerprint + two extras.
    assert_eq!(args.iter().filter(|a| *a == "--label").count(), 3);
    // Collect the arg immediately following each --label.
    let rendered: Vec<String> = args
        .windows(2)
        .filter_map(|w| {
            if w[0] == "--label" {
                w[1].to_str().map(String::from)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            "dev.pithos.fingerprint=abc123".to_string(),
            "dev.pithos.dotnet-version=10.0.102".to_string(),
            "dev.pithos.rust-version=1.85.0".to_string(),
        ]
    );
}

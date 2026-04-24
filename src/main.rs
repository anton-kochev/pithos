use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use saphyr::YamlOwned;

use pithos::output::{Style, narrate};

#[derive(Debug, PartialEq, Eq)]
enum RejectKind {
    Subcommand,
    Flag,
}

/// Execution mode for the `run` (and internally for `build`) pipeline.
///
/// Encodes the `--rebuild` / `--no-build` flags as a closed enum rather
/// than two correlated bools, so the parser-precluded combo is
/// unrepresentable by construction instead of by convention.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RunMode {
    Default,
    Rebuild,
    NoBuild,
}

// Short usage line for fail-fast reject paths. `pithos help` prints the full usage.
const USAGE: &str = "usage: pithos [run | build | help | version] [options]";

#[derive(Debug, PartialEq, Eq)]
enum Subcommand {
    Build { rebuild: bool },
    Run { mode: RunMode, cmd: Vec<String> },
    Help,
    Version,
    Reject { kind: RejectKind, value: String },
}

impl Subcommand {
    fn from_args(args: &[String]) -> Self {
        match args.get(1).map(String::as_str) {
            // Bare `pithos` = `pithos run`: matches common-invocation muscle memory.
            None => Self::Run {
                mode: RunMode::Default,
                cmd: Vec::new(),
            },
            Some("build") => {
                let mut rebuild = false;
                for arg in args.iter().skip(2) {
                    match arg.as_str() {
                        "--rebuild" => rebuild = true,
                        other => {
                            return Self::Reject {
                                kind: RejectKind::Flag,
                                value: other.to_string(),
                            };
                        }
                    }
                }
                Self::Build { rebuild }
            }
            Some("run") => {
                let mut mode = RunMode::Default;
                let mut cmd: Vec<String> = Vec::new();
                let mut iter = args.iter().skip(2);
                while let Some(arg) = iter.next() {
                    match arg.as_str() {
                        "--" => {
                            cmd.extend(iter.by_ref().cloned());
                            break;
                        }
                        "--rebuild" => match mode {
                            RunMode::NoBuild => {
                                return Self::Reject {
                                    kind: RejectKind::Flag,
                                    value: "--rebuild and --no-build are mutually exclusive"
                                        .to_string(),
                                };
                            }
                            RunMode::Default | RunMode::Rebuild => mode = RunMode::Rebuild,
                        },
                        "--no-build" => match mode {
                            RunMode::Rebuild => {
                                return Self::Reject {
                                    kind: RejectKind::Flag,
                                    value: "--rebuild and --no-build are mutually exclusive"
                                        .to_string(),
                                };
                            }
                            RunMode::Default | RunMode::NoBuild => mode = RunMode::NoBuild,
                        },
                        s if s.starts_with("--") => {
                            return Self::Reject {
                                kind: RejectKind::Flag,
                                value: s.to_string(),
                            };
                        }
                        _ => {
                            cmd.push(arg.clone());
                            cmd.extend(iter.by_ref().cloned());
                            break;
                        }
                    }
                }
                Self::Run { mode, cmd }
            }
            Some("help") => match args.get(2) {
                None => Self::Help,
                Some(extra) => Self::Reject {
                    kind: RejectKind::Flag,
                    value: extra.clone(),
                },
            },
            Some("version") => match args.get(2) {
                None => Self::Version,
                Some(extra) => Self::Reject {
                    kind: RejectKind::Flag,
                    value: extra.clone(),
                },
            },
            Some(other) => Self::Reject {
                kind: RejectKind::Subcommand,
                value: other.to_string(),
            },
        }
    }

    fn requires_daemon(&self) -> bool {
        matches!(self, Self::Build { .. } | Self::Run { .. })
    }
}

fn main() -> ExitCode {
    // §6.3: SIGINT → 130. Installed first so Ctrl-C at any stage (arg parse,
    // docker probe, build, run) exits with the standardized code and runs
    // destructors that would otherwise be skipped by default signal death.
    let _ = ctrlc::set_handler(|| std::process::exit(130));

    let style = Style::detect();
    let args: Vec<String> = env::args().collect();
    let subcommand = Subcommand::from_args(&args);

    // Fail fast on unknown subcommand/flag before any I/O — typos like `pithos buidl`
    // or `pithos build --nope` shouldn't require a `.pithos` file or mutate
    // `.pithos.d/Dockerfile`.
    if let Subcommand::Reject { kind, value } = &subcommand {
        match kind {
            RejectKind::Subcommand => {
                narrate(style, ">> ERROR:", &format!("unknown subcommand: {value}"))
            }
            RejectKind::Flag => narrate(style, ">> ERROR:", &format!("unknown flag: {value}")),
        }
        narrate(style, ">>", USAGE);
        return ExitCode::from(2);
    }

    // Discovery subcommands short-circuit before any I/O — no `.pithos`, no
    // Dockerfile emission, no daemon probe. Output goes to stdout (Unix
    // convention for requested output), distinct from stderr narration.
    match &subcommand {
        Subcommand::Help => {
            println!("{}", help_text());
            return ExitCode::SUCCESS;
        }
        Subcommand::Version => {
            println!("{}", version_text());
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            narrate(style, ">> ERROR:", &format!("cannot read cwd: {e}"));
            return ExitCode::from(1);
        }
    };
    let pithos_bytes = match read_pithos(&cwd, style) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let yaml = match pithos::config::load(&pithos_bytes) {
        Ok(y) => y,
        Err(e) => {
            narrate(style, ">> ERROR:", &format!("{e}"));
            return ExitCode::from(2);
        }
    };
    let dockerfile_path = cwd.join(".pithos.d").join("Dockerfile");
    let dockerfile_content = pithos::dockerfile::emit(&yaml);
    if let Err(code) = write_dockerfile(&dockerfile_path, &dockerfile_content, style) {
        return code;
    }

    if subcommand.requires_daemon() {
        if let Err(code) = require_daemon(style) {
            return code;
        }
    }

    match subcommand {
        Subcommand::Build { rebuild } => run_build(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
            rebuild,
            style,
        ),
        Subcommand::Run { mode, cmd } => run_run(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
            mode,
            &cmd,
            style,
        ),
        Subcommand::Help | Subcommand::Version => {
            unreachable!("handled by fail-fast guard above")
        }
        Subcommand::Reject { .. } => unreachable!("handled by fail-fast guard above"),
    }
}

fn read_pithos(cwd: &Path, style: Style) -> Result<Vec<u8>, ExitCode> {
    let path = cwd.join(".pithos");
    match fs::read(&path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            narrate(style, ">> ERROR:", ".pithos not found");
            narrate(
                style,
                ">>",
                "Create a .pithos file at the project root. Minimal example:",
            );
            narrate(style, ">>", "");
            narrate(style, ">>", "  toolchains: {}");
            Err(ExitCode::from(2))
        }
        Err(e) => {
            narrate(style, ">> ERROR:", &format!("{}: {e}", path.display()));
            Err(ExitCode::from(1))
        }
    }
}

fn write_dockerfile(path: &Path, content: &str, style: Style) -> Result<(), ExitCode> {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            narrate(
                style,
                ">> ERROR:",
                &format!("cannot create {}: {e}", parent.display()),
            );
            return Err(ExitCode::from(1));
        }
    }
    if let Err(e) = fs::write(path, content) {
        narrate(
            style,
            ">> ERROR:",
            &format!("cannot write {}: {e}", path.display()),
        );
        return Err(ExitCode::from(1));
    }
    Ok(())
}

/// Action to take after a fingerprint cache lookup, given the `--rebuild`
/// and `--no-build` flags. `Reuse(id)` short-circuits with the existing
/// image; `Build` falls through to the build path; `Abort` signals that
/// `--no-build` was set and no matching image exists (exit 4).
#[derive(Debug, PartialEq, Eq)]
enum BuildAction<'a> {
    Reuse(&'a str),
    Build,
    Abort,
}

/// Resolve the build decision from the run mode and fingerprint lookup.
/// Pure — no I/O. Total over its domain by construction of `RunMode`.
fn resolve_build_action(mode: RunMode, cached: Option<&str>) -> BuildAction<'_> {
    match (mode, cached) {
        (RunMode::Rebuild, _) => BuildAction::Build,
        (RunMode::NoBuild, None) => BuildAction::Abort,
        (RunMode::NoBuild, Some(id)) => BuildAction::Reuse(id),
        (RunMode::Default, Some(id)) => BuildAction::Reuse(id),
        (RunMode::Default, None) => BuildAction::Build,
    }
}

/// Narration body for the `--no-build` + cache-miss abort. Pure so the
/// wording is lockable in a unit test without reaching through `ensure_image`.
fn abort_message(project: &str) -> String {
    format!("image pithos:{project} not found; run `pithos build` to create it (--no-build is set)")
}

/// Return `<cwd>/.env` if it exists, else `None`. Extracted so the
/// conditional `--env-file` branch in `run_run` is unit-testable. No I/O
/// beyond a `Path::exists()` probe.
fn discover_env_file(cwd: &Path) -> Option<std::path::PathBuf> {
    let p = cwd.join(".env");
    p.exists().then_some(p)
}

fn version_text() -> String {
    format!("pithos {}", env!("CARGO_PKG_VERSION"))
}

fn help_text() -> String {
    format!(
        "pithos {} — declarative Docker development containers\n\
         \n\
         usage: pithos <COMMAND> [OPTIONS]\n\
         \n\
         Commands:\n  \
           run [cmd...]   Build-if-needed, then launch container (default when no command given)\n  \
           build          Build the image without launching\n  \
           help           Print this help\n  \
           version        Print the pithos version\n\
         \n\
         Options:\n  \
           run:    --rebuild, --no-build, -- <cmd...>\n  \
           build:  --rebuild\n\
         \n\
         All narration is written to stderr; stdout is reserved for container output and\n\
         for `pithos help` / `pithos version`.",
        env!("CARGO_PKG_VERSION")
    )
}

/// Probe the Docker daemon and translate the outcome into an exit code
/// + user-facing narration via the pure `classify_probe` helper.
fn require_daemon(style: Style) -> Result<(), ExitCode> {
    match pithos::docker::probe_daemon(std::time::Duration::from_secs(3)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let (code, message) = pithos::docker::classify_probe(&e);
            narrate(style, ">> ERROR:", message);
            Err(ExitCode::from(code))
        }
    }
}

struct EnsuredImage {
    tag: String,
    project: String,
}

/// Ensure `pithos:<project>` exists locally; build it if the fingerprint
/// cache misses or `--rebuild` is set. On `Err(ExitCode)`, diagnostic
/// output has already been narrated via `narrate(style, ">> ERROR:", ...)`
/// — callers should just propagate the code, not re-narrate.
fn ensure_image(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_path: &Path,
    dockerfile_content: &str,
    mode: RunMode,
    style: Style,
) -> Result<EnsuredImage, ExitCode> {
    let project = match pithos::project::name_from_path(cwd) {
        Some(n) => n,
        None => {
            narrate(
                style,
                ">> ERROR:",
                &format!("cannot derive project name from {}", cwd.display()),
            );
            return Err(ExitCode::from(1));
        }
    };

    let mut installers = BTreeMap::new();
    let toolchain_names: Vec<String> = pithos::dockerfile::toolchain_names(yaml).collect();
    for name in &toolchain_names {
        let Some(bytes) = pithos::embed::installer_bytes(name) else {
            narrate(
                style,
                ">> ERROR:",
                &format!(
                    "no baked installer for toolchain {name:?} \
                     (config validator and embed bundle are out of sync)"
                ),
            );
            return Err(ExitCode::from(1));
        };
        installers.insert(name.clone(), bytes.to_vec());
    }
    let hash = pithos::fingerprint::compute(dockerfile_content, pithos_bytes, &installers);

    let cached = match pithos::docker::find_image_by_fingerprint(&hash) {
        Ok(opt) => opt,
        Err(e) => {
            narrate(style, ">> ERROR:", &format!("{e}"));
            return Err(ExitCode::from(1));
        }
    };
    let tag = format!("pithos:{project}");
    match resolve_build_action(mode, cached.as_deref()) {
        BuildAction::Reuse(id) => {
            narrate(
                style,
                ">>",
                &format!("cached image {id} matches fingerprint; skipping build"),
            );
            return Ok(EnsuredImage { tag, project });
        }
        BuildAction::Abort => {
            narrate(style, ">> ERROR:", &abort_message(&project));
            return Err(ExitCode::from(4));
        }
        BuildAction::Build => {}
    }

    // `TempDir` cleans on Drop; SIGINT runs Drop, SIGKILL leaks under `$TMPDIR` for the OS to reap.
    let context = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            narrate(
                style,
                ">> ERROR:",
                &format!("cannot create build-context tempdir: {e}"),
            );
            return Err(ExitCode::from(1));
        }
    };
    if let Err(e) = pithos::embed::extract_to(context.path()) {
        narrate(
            style,
            ">> ERROR:",
            &format!("cannot extract build context: {e}"),
        );
        return Err(ExitCode::from(1));
    }

    // First pass: fingerprint label only. Installer RUN steps populate
    // /opt/pithos-versions/<tc> inside the image as a side-effect.
    match pithos::docker::build(
        context.path(),
        dockerfile_path,
        &project,
        &hash,
        &BTreeMap::new(),
        style,
    ) {
        Ok(()) => {}
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(style, ">> ERROR:", &format!("docker build: {e}"));
            return Err(ExitCode::from(1));
        }
        Err(pithos::docker::BuildError::NonZero { code, tail }) => {
            let code_str = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            narrate(
                style,
                ">> ERROR:",
                &format!(
                    "docker build failed (exit {code_str}); last {} lines:",
                    tail.len()
                ),
            );
            for line in &tail {
                eprintln!("{}", pithos::output::format_docker_line(line, style));
            }
            // Exit 3 is reserved for first-pass user-config-caused build failure.
            return Err(ExitCode::from(3));
        }
    }

    if toolchain_names.is_empty() {
        // Nothing to extract; single-pass is sufficient.
        return Ok(EnsuredImage { tag, project });
    }

    rebuild_with_version_labels(
        &toolchain_names,
        context.path(),
        dockerfile_path,
        &project,
        &hash,
        &tag,
        style,
    )?;
    Ok(EnsuredImage { tag, project })
}

/// Second pass: read resolved versions from the first-pass image, then
/// rebuild with `dev.pithos.<tc>-version=<v>` labels stacked on top of the
/// fingerprint label. BuildKit cache-hits every layer; the `pithos:<project>`
/// tag gets reassigned to the new (labeled) image without re-doing work.
///
/// All failure modes here are internal-error exits (code 1), not user-build
/// failures (code 3) — an installer or launcher contract violation, not a
/// broken `.pithos`.
fn rebuild_with_version_labels(
    toolchain_names: &[String],
    context: &Path,
    dockerfile_path: &Path,
    project: &str,
    hash: &str,
    tag: &str,
    style: Style,
) -> Result<(), ExitCode> {
    let versions = match pithos::docker::extract_versions(tag, toolchain_names) {
        Ok(v) => v,
        Err(e) => {
            narrate(
                style,
                ">> ERROR:",
                &format!("internal: version extraction failed: {e}"),
            );
            return Err(ExitCode::from(1));
        }
    };

    // BTreeMap iteration is sort-by-key, so the narration is deterministic.
    let resolved: Vec<String> = versions
        .iter()
        .map(|(name, version)| format!("{name}={version}"))
        .collect();
    narrate(
        style,
        ">>",
        &format!("resolved: {}", resolved.join(", ")),
    );

    let mut extra_labels: BTreeMap<String, String> = BTreeMap::new();
    for (name, version) in &versions {
        extra_labels.insert(
            pithos::fingerprint::version_label_key(name),
            version.clone(),
        );
    }

    match pithos::docker::build(
        context,
        dockerfile_path,
        project,
        hash,
        &extra_labels,
        style,
    ) {
        Ok(()) => Ok(()),
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(
                style,
                ">> ERROR:",
                &format!("internal: metadata rebuild failed: {e}"),
            );
            Err(ExitCode::from(1))
        }
        Err(pithos::docker::BuildError::NonZero { code, tail }) => {
            let code_str = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            narrate(
                style,
                ">> ERROR:",
                &format!(
                    "internal: metadata rebuild failed (exit {code_str}); last {} lines:",
                    tail.len()
                ),
            );
            for line in &tail {
                eprintln!("{}", pithos::output::format_docker_line(line, style));
            }
            Err(ExitCode::from(1))
        }
    }
}

fn run_build(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_path: &Path,
    dockerfile_content: &str,
    rebuild: bool,
    style: Style,
) -> ExitCode {
    // `build` never aborts: --no-build is a `run`-only flag, so NoBuild is
    // unreachable here by construction. Collapsing to Default/Rebuild keeps
    // that invariant in the type system.
    let mode = if rebuild {
        RunMode::Rebuild
    } else {
        RunMode::Default
    };
    match ensure_image(
        cwd,
        yaml,
        pithos_bytes,
        dockerfile_path,
        dockerfile_content,
        mode,
        style,
    ) {
        Ok(_) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

/// Translate a completed child `ExitStatus` into a launcher exit byte per
/// §6.3. Normal exits propagate the child's code; signal-death on Unix maps
/// to `128 + sig` (SIGINT→130, SIGTERM→143). `1` is reserved for the
/// exotic "neither code nor signal" state.
#[cfg(unix)]
fn exit_code_from_status(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        return code as u8;
    }
    if let Some(sig) = status.signal() {
        return (128 + sig) as u8;
    }
    1
}

#[cfg(not(unix))]
fn exit_code_from_status(status: std::process::ExitStatus) -> u8 {
    status.code().unwrap_or(1) as u8
}

fn run_run(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_path: &Path,
    dockerfile_content: &str,
    mode: RunMode,
    cmd: &[String],
    style: Style,
) -> ExitCode {
    let ensured = match ensure_image(
        cwd,
        yaml,
        pithos_bytes,
        dockerfile_path,
        dockerfile_content,
        mode,
        style,
    ) {
        Ok(e) => e,
        Err(code) => return code,
    };
    // Empty-string PITHOS_REPO would silently resolve relative to cwd on
    // join — treat it as unset.
    let pithos_repo = env::var_os("PITHOS_REPO")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let env_file = discover_env_file(cwd);

    match pithos::docker::run(
        &ensured.tag,
        &ensured.project,
        cwd,
        pithos_repo.as_deref(),
        env_file.as_deref(),
        cmd,
    ) {
        Ok(status) => ExitCode::from(exit_code_from_status(status)),
        Err(pithos::docker::RunError::Spawn(e)) => {
            narrate(style, ">> ERROR:", &format!("docker run: {e}"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_from_status_propagates_normal_exit_code() {
        use std::os::unix::process::ExitStatusExt;
        let s = std::process::ExitStatus::from_raw(7 << 8);
        assert_eq!(exit_code_from_status(s), 7);
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_from_status_maps_sigint_to_130() {
        use std::os::unix::process::ExitStatusExt;
        let s = std::process::ExitStatus::from_raw(2); // SIGINT
        assert_eq!(exit_code_from_status(s), 130);
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_from_status_maps_sigterm_to_143() {
        use std::os::unix::process::ExitStatusExt;
        let s = std::process::ExitStatus::from_raw(15); // SIGTERM
        assert_eq!(exit_code_from_status(s), 143);
    }

    #[test]
    fn resolve_build_action_rebuild_with_cache_builds() {
        // --rebuild overrides a cache hit → build
        assert_eq!(
            resolve_build_action(RunMode::Rebuild, Some("abc123")),
            BuildAction::Build
        );
    }

    #[test]
    fn resolve_build_action_rebuild_without_cache_builds() {
        assert_eq!(
            resolve_build_action(RunMode::Rebuild, None),
            BuildAction::Build
        );
    }

    #[test]
    fn resolve_build_action_no_build_with_cache_reuses() {
        // --no-build + cache hit → reuse, don't build
        assert_eq!(
            resolve_build_action(RunMode::NoBuild, Some("abc123")),
            BuildAction::Reuse("abc123")
        );
    }

    #[test]
    fn resolve_build_action_no_build_without_cache_aborts() {
        // --no-build + cache miss → exit 4 signal
        assert_eq!(
            resolve_build_action(RunMode::NoBuild, None),
            BuildAction::Abort
        );
    }

    #[test]
    fn resolve_build_action_default_with_cache_reuses() {
        assert_eq!(
            resolve_build_action(RunMode::Default, Some("abc123")),
            BuildAction::Reuse("abc123")
        );
    }

    #[test]
    fn resolve_build_action_default_without_cache_builds() {
        assert_eq!(
            resolve_build_action(RunMode::Default, None),
            BuildAction::Build
        );
    }

    #[test]
    fn abort_message_mentions_pithos_build_and_flag_and_project() {
        // Lock the three user-facing tokens so the guidance can't silently
        // drift — CI runners grep for "pithos build" to diagnose exit 4.
        let m = abort_message("widgets");
        assert!(m.contains("pithos build"), "missing 'pithos build': {m}");
        assert!(m.contains("--no-build"), "missing '--no-build': {m}");
        assert!(m.contains("widgets"), "missing project name: {m}");
    }

    #[test]
    fn from_args_build_without_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "build"])),
            Subcommand::Build { rebuild: false }
        );
    }

    #[test]
    fn from_args_build_with_rebuild() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "build", "--rebuild"])),
            Subcommand::Build { rebuild: true }
        );
    }

    #[test]
    fn from_args_build_rejects_unknown_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "build", "--nope"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--nope".to_string(),
            }
        );
    }

    #[test]
    fn from_args_build_rejects_trailing_positional() {
        // Stray positionals after a flag are rejected — `build` takes none.
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "build", "--rebuild", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn from_args_flag_before_subcommand_is_unknown_subcommand() {
        // `--rebuild` sitting in args[1] falls through the subcommand path.
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "--rebuild", "build"])),
            Subcommand::Reject {
                kind: RejectKind::Subcommand,
                value: "--rebuild".to_string(),
            }
        );
    }

    #[test]
    fn from_args_no_subcommand_defaults_to_run() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec![],
            }
        );
    }

    #[test]
    fn from_args_typo_is_unknown() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "buidl"])),
            Subcommand::Reject {
                kind: RejectKind::Subcommand,
                value: "buidl".to_string(),
            }
        );
    }

    #[test]
    fn from_args_run_without_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec![],
            }
        );
    }

    #[test]
    fn from_args_run_with_rebuild() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild"])),
            Subcommand::Run {
                mode: RunMode::Rebuild,
                cmd: vec![],
            }
        );
    }

    #[test]
    fn from_args_run_rejects_unknown_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--nope"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--nope".to_string(),
            }
        );
    }

    #[test]
    fn from_args_run_accepts_positional_as_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "extra"])),
            Subcommand::Run {
                mode: RunMode::Rebuild,
                cmd: vec!["extra".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_accepts_bare_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["bash".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_accepts_cmd_with_args() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash", "-c", "echo"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["bash".to_string(), "-c".to_string(), "echo".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_accepts_cmd_after_double_dash() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--", "bash", "-c", "echo"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["bash".to_string(), "-c".to_string(), "echo".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_rebuild_before_double_dash() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--", "bash"])),
            Subcommand::Run {
                mode: RunMode::Rebuild,
                cmd: vec!["bash".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_trailing_double_dash_yields_empty_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--"])),
            Subcommand::Run {
                mode: RunMode::Rebuild,
                cmd: vec![],
            }
        );
    }

    // Locks design choice #3: once a non-flag token is seen, the parser does
    // NOT re-enter flag mode. Without this, a refactor that starts re-recognizing
    // --rebuild after a positional would pass CI silently.
    #[test]
    fn from_args_run_flag_after_positional_is_cmd_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash", "--rebuild"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["bash".to_string(), "--rebuild".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_with_no_build() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--no-build"])),
            Subcommand::Run {
                mode: RunMode::NoBuild,
                cmd: vec![],
            }
        );
    }

    #[test]
    fn from_args_run_no_build_before_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--no-build", "bash"])),
            Subcommand::Run {
                mode: RunMode::NoBuild,
                cmd: vec!["bash".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_rejects_rebuild_then_no_build_combo() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--no-build"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--rebuild and --no-build are mutually exclusive".to_string(),
            }
        );
    }

    #[test]
    fn from_args_run_rejects_no_build_then_rebuild_combo() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--no-build", "--rebuild"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--rebuild and --no-build are mutually exclusive".to_string(),
            }
        );
    }

    #[test]
    fn from_args_run_no_build_after_double_dash_is_cmd_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--", "--no-build"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["--no-build".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_no_build_after_positional_is_cmd_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash", "--no-build"])),
            Subcommand::Run {
                mode: RunMode::Default,
                cmd: vec!["bash".to_string(), "--no-build".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_no_build_twice_is_idempotent() {
        // Mirrors how --rebuild --rebuild silently coalesces; keeps the
        // rejection surface minimal (combo-with-rebuild is the only invalid
        // shape).
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--no-build", "--no-build"])),
            Subcommand::Run {
                mode: RunMode::NoBuild,
                cmd: vec![],
            }
        );
    }

    // Combo rejection must fire inside the iteration loop, not post-loop —
    // otherwise a trailing positional like `extra` would get captured into
    // `cmd` before the incoherent combo is noticed.
    #[test]
    fn from_args_run_combo_rejection_fires_before_trailing_positional() {
        assert_eq!(
            Subcommand::from_args(&args(&[
                "pithos",
                "run",
                "--rebuild",
                "--no-build",
                "extra"
            ])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--rebuild and --no-build are mutually exclusive".to_string(),
            }
        );
    }

    #[test]
    fn discover_env_file_returns_some_when_env_exists() {
        let td = tempfile::tempdir().unwrap();
        let env_path = td.path().join(".env");
        std::fs::write(&env_path, "FOO=bar").unwrap();
        assert_eq!(discover_env_file(td.path()), Some(env_path));
    }

    #[test]
    fn discover_env_file_returns_none_when_env_absent() {
        let td = tempfile::tempdir().unwrap();
        assert!(discover_env_file(td.path()).is_none());
    }

    #[test]
    fn from_args_help_bare() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "help"])),
            Subcommand::Help
        );
    }

    #[test]
    fn from_args_version_bare() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "version"])),
            Subcommand::Version
        );
    }

    #[test]
    fn from_args_help_rejects_trailing_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "help", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn from_args_version_rejects_trailing_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "version", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn help_text_lists_all_wired_subcommands() {
        let t = help_text();
        for name in ["run", "build", "help", "version"] {
            assert!(t.contains(name), "help missing subcommand {name:?}: {t}");
        }
    }

    #[test]
    fn help_text_lists_all_wired_flags() {
        let t = help_text();
        assert!(t.contains("--rebuild"), "help missing --rebuild: {t}");
        assert!(t.contains("--no-build"), "help missing --no-build: {t}");
    }

    #[test]
    fn version_text_matches_cargo_pkg_version() {
        assert_eq!(
            version_text(),
            format!("pithos {}", env!("CARGO_PKG_VERSION"))
        );
    }
}

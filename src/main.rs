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
    /// A recognized flag used wrongly — a missing value, or a combination the
    /// CLI does not accept. Carries a full sentence, so it narrates without
    /// the "unknown flag:" prefix that would misdescribe it.
    Usage,
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

/// What `pithos run` launches after preparing the container.
///
/// Pi arguments stay opaque to pithos. This lets the Pi version inside the
/// image define its own CLI (including extension-provided flags) without the
/// launcher having to duplicate and continually update Pi's parser.
#[derive(Debug, PartialEq, Eq, Clone)]
enum RunTarget {
    /// Launch the image's Pi command with these arguments. An empty vector
    /// falls through to the image `CMD`; a non-empty one is materialized from
    /// `PI_LAUNCH_ARGV` before the arguments are appended.
    Pi(Vec<String>),
    /// Replace the image `CMD` with an explicit container command.
    Command(Vec<String>),
}

// Short usage line for fail-fast reject paths. `pithos help` prints the full usage.
const USAGE: &str =
    "usage: pithos [run | build | info | clean | rebuild-base | help | version] [options]";

// Content written when the user accepts the prompt to create a missing `.pithos`.
// Mirrors the example in the prompt narration; validates cleanly through `pithos::config::load`.
const MINIMAL_PITHOS: &str = "toolchains: {}\n";

#[derive(Debug, PartialEq, Eq)]
enum Subcommand {
    Build {
        rebuild: bool,
    },
    Run {
        mode: RunMode,
        target: RunTarget,
        tmux: bool,
    },
    Info,
    Clean {
        all: bool,
    },
    RebuildBase,
    Help,
    Version,
    Reject {
        kind: RejectKind,
        value: String,
    },
}

impl Subcommand {
    fn from_args(args: &[String]) -> Self {
        match args.get(1).map(String::as_str) {
            // Bare `pithos` = `pithos run`: matches common-invocation muscle memory.
            None => Self::parse_run(&[]),
            Some("run") => Self::parse_run(&args[2..]),
            // A leading flag implies `run` too. Pithos consumes its own option
            // prefix; the first other option starts the opaque Pi argument tail.
            Some(s) if s.starts_with('-') => Self::parse_run(&args[1..]),
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
            Some("info") => match args.get(2) {
                None => Self::Info,
                Some(extra) => Self::Reject {
                    kind: RejectKind::Flag,
                    value: extra.clone(),
                },
            },
            Some("clean") => {
                let mut all = false;
                for arg in args.iter().skip(2) {
                    match arg.as_str() {
                        "--all" => all = true,
                        other => {
                            return Self::Reject {
                                kind: RejectKind::Flag,
                                value: other.to_string(),
                            };
                        }
                    }
                }
                Self::Clean { all }
            }
            Some("rebuild-base") => match args.get(2) {
                None => Self::RebuildBase,
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

    /// Parse the argument tail of a `run` invocation: everything after
    /// `pithos run`, or after `pithos` itself when a leading flag implies the
    /// subcommand.
    ///
    /// Pithos options form a prefix. The first option pithos does not own starts
    /// an opaque Pi argument tail, which is forwarded verbatim. `--pi` can be
    /// used to start that tail explicitly (including with positional arguments).
    /// A positional or `--` before a Pi tail retains the existing explicit
    /// container-command behavior.
    fn parse_run(rest: &[String]) -> Self {
        let mut mode = RunMode::Default;
        let mut target = RunTarget::Pi(Vec::new());
        let mut tmux = false;
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--pi" => {
                    target = RunTarget::Pi(rest[i + 1..].to_vec());
                    break;
                }
                "--" => {
                    target = RunTarget::Command(rest[i + 1..].to_vec());
                    break;
                }
                "--tmux" => tmux = true,
                "--rebuild" => match mode {
                    RunMode::NoBuild => {
                        return Self::Reject {
                            kind: RejectKind::Usage,
                            value: "--rebuild and --no-build are mutually exclusive".to_string(),
                        };
                    }
                    RunMode::Default | RunMode::Rebuild => mode = RunMode::Rebuild,
                },
                "--no-build" => match mode {
                    RunMode::Rebuild => {
                        return Self::Reject {
                            kind: RejectKind::Usage,
                            value: "--rebuild and --no-build are mutually exclusive".to_string(),
                        };
                    }
                    RunMode::Default | RunMode::NoBuild => mode = RunMode::NoBuild,
                },
                s if s.starts_with('-') => {
                    target = RunTarget::Pi(rest[i..].to_vec());
                    break;
                }
                _ => {
                    target = RunTarget::Command(rest[i..].to_vec());
                    break;
                }
            }
            i += 1;
        }
        Self::Run { mode, target, tmux }
    }

    fn requires_daemon(&self) -> bool {
        matches!(
            self,
            Self::Build { .. } | Self::Run { .. } | Self::Clean { .. } | Self::RebuildBase
        )
    }

    fn writes_dockerfile(&self) -> bool {
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

    // Fail fast on parser rejections before any I/O — typos like `pithos buidl`
    // or `pithos build --nope` shouldn't require a `.pithos` file or mutate
    // `.pithos.d/Dockerfile`.
    if let Subcommand::Reject { kind, value } = &subcommand {
        match kind {
            RejectKind::Subcommand => {
                narrate(style, "» ERROR:", &format!("unknown subcommand: {value}"))
            }
            RejectKind::Flag => narrate(style, "» ERROR:", &format!("unknown flag: {value}")),
            RejectKind::Usage => narrate(style, "» ERROR:", value),
        }
        narrate(style, "»", USAGE);
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

    // Clean needs the daemon but neither cwd resolution nor Dockerfile emit —
    // short-circuiting here keeps the prelude single-purpose for build/run/info.
    if let Subcommand::Clean { all } = &subcommand {
        let all = *all;
        if let Err(code) = require_daemon(style) {
            return code;
        }
        return run_clean(all, style);
    }

    // RebuildBase short-circuits like Clean: needs the daemon, but does not
    // touch `.pithos`, does not emit a per-project Dockerfile, and resolves
    // its context from the cwd of the pithos source tree (not a project dir).
    if matches!(subcommand, Subcommand::RebuildBase) {
        if let Err(code) = require_daemon(style) {
            return code;
        }
        let cwd = match env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                narrate(style, "» ERROR:", &format!("cannot read cwd: {e}"));
                return ExitCode::from(1);
            }
        };
        return run_rebuild_base(&cwd, style);
    }

    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            narrate(style, "» ERROR:", &format!("cannot read cwd: {e}"));
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
            narrate(style, "» ERROR:", &format!("{e}"));
            return ExitCode::from(2);
        }
    };
    let dockerfile_path = cwd.join(".pithos.d").join("Dockerfile");
    let dockerfile_content = pithos::dockerfile::emit(&yaml);
    let extensions_manifest_path = cwd.join(".pithos.d").join("extensions.list");
    let extensions_manifest_content = pithos::extensions::manifest(&yaml);
    if subcommand.writes_dockerfile() {
        if let Err(code) = write_dockerfile(&dockerfile_path, &dockerfile_content, style) {
            return code;
        }
        if let Err(code) = write_dockerfile(
            &extensions_manifest_path,
            &extensions_manifest_content,
            style,
        ) {
            return code;
        }
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
        Subcommand::Run { mode, target, tmux } => run_run(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
            mode,
            &target,
            tmux,
            style,
        ),
        Subcommand::Info => run_info(&cwd, &yaml, &pithos_bytes, &dockerfile_content, style),
        Subcommand::Clean { .. } | Subcommand::RebuildBase => {
            unreachable!("handled by short-circuit above")
        }
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
            narrate(style, "» ERROR:", ".pithos not found");
            narrate(
                style,
                "»",
                "Create a minimal .pithos here? It will contain:",
            );
            narrate(style, "»", "");
            narrate(style, "»", "  toolchains: {}");
            narrate(style, "»", "");
            if prompt_confirm_default_yes("Create .pithos? [Y/n]:", style) {
                match fs::write(&path, MINIMAL_PITHOS) {
                    Ok(()) => {
                        narrate(style, "»", &format!("Created {}.", path.display()));
                        Ok(MINIMAL_PITHOS.as_bytes().to_vec())
                    }
                    Err(e) => {
                        narrate(
                            style,
                            "» ERROR:",
                            &format!("cannot write {}: {e}", path.display()),
                        );
                        Err(ExitCode::from(1))
                    }
                }
            } else {
                narrate(style, "»", "aborted; .pithos not created.");
                Err(ExitCode::from(2))
            }
        }
        Err(e) => {
            narrate(style, "» ERROR:", &format!("{}: {e}", path.display()));
            Err(ExitCode::from(1))
        }
    }
}

fn write_dockerfile(path: &Path, content: &str, style: Style) -> Result<(), ExitCode> {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            narrate(
                style,
                "» ERROR:",
                &format!("cannot create {}: {e}", parent.display()),
            );
            return Err(ExitCode::from(1));
        }
    }
    if let Err(e) = fs::write(path, content) {
        narrate(
            style,
            "» ERROR:",
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

/// Materialize Pi's launch argv when arguments must be appended. An empty
/// argument list returns an empty vector so Docker falls through to the image
/// `CMD`. Built from [`pithos::dockerfile::PI_LAUNCH_ARGV`] so pass-through
/// launches and the generated `CMD` cannot drift.
fn pi_argv(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        return Vec::new();
    }
    pithos::dockerfile::PI_LAUNCH_ARGV
        .iter()
        .map(|s| s.to_string())
        .chain(args.iter().cloned())
        .collect()
}

/// Pick the argv `docker run` appends after the image tag.
fn effective_command(target: &RunTarget) -> Vec<String> {
    match target {
        RunTarget::Pi(args) => pi_argv(args),
        RunTarget::Command(cmd) => cmd.clone(),
    }
}

fn version_text() -> String {
    format!("pithos {}", env!("CARGO_PKG_VERSION"))
}

fn help_text() -> String {
    format!(
        "pithos {} — declarative Docker development containers\n\
         \n\
         usage: pithos [run] [PITHOS_OPTIONS] [PI_OPTIONS...]\n\
                pithos run [PITHOS_OPTIONS] [--] <cmd...>\n\
         \n\
         Commands:\n  \
           run [cmd...]   Build-if-needed, then launch Pi or an explicit command\n  \
           build          Build the image without launching\n  \
           info           Print project config, fingerprint, and image status\n  \
           clean          Remove dangling pithos images (or all with --all)\n  \
           rebuild-base   Build Dockerfile.base into local :base for dev iteration\n  \
           help           Print this help\n  \
           version        Print the pithos version\n\
         \n\
         Options:\n  \
           run:    --rebuild, --no-build, --tmux, --pi <args...>, -- <cmd...>\n  \
           build:  --rebuild\n\
         \n\
         Pi arguments:\n  \
           Any other leading option starts an opaque argument tail that is forwarded\n  \
           verbatim to Pi. Put pithos options first. Use --pi to forward a tail\n  \
           beginning with a positional, --, or a pithos option name. Examples:\n    \
             pithos --session 01a0335e\n    \
             pithos --fork 01a0335e\n    \
             pithos --provider openai --model gpt-4o -p \"Review this\"\n    \
             pithos --pi \"Start with this prompt\"\n\
         \n\
         Config (.pithos):\n  \
           Toolchains use the flat form — a quoted exact version per name; nested\n  \
           `version:` keys are not supported:\n    \
             toolchains:\n      \
               dotnet: \"10.0.0\"\n      \
               rust: \"1.85.0\"\n\
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
            narrate(style, "» ERROR:", message);
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
/// output has already been narrated via `narrate(style, "» ERROR:", ...)`
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
                "» ERROR:",
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
                "» ERROR:",
                &format!(
                    "no baked installer for toolchain {name:?} \
                     (config validator and embed bundle are out of sync)"
                ),
            );
            return Err(ExitCode::from(1));
        };
        installers.insert(name.clone(), bytes.to_vec());
    }
    let base_image_id = match pithos::docker::inspect_image_id(pithos::docker::BASE_IMAGE_REF) {
        Ok(id) => id,
        Err(e) => {
            narrate(style, "» ERROR:", &format!("{e}"));
            return Err(ExitCode::from(1));
        }
    };
    let hash = pithos::fingerprint::compute(
        dockerfile_content,
        pithos_bytes,
        &installers,
        pithos::embed::PI_BUN_COMPAT_MJS,
        &base_image_id,
    );

    let cached = match pithos::docker::find_image_by_fingerprint(&hash) {
        Ok(opt) => opt,
        Err(e) => {
            narrate(style, "» ERROR:", &format!("{e}"));
            return Err(ExitCode::from(1));
        }
    };
    let tag = format!("pithos:{project}");
    match resolve_build_action(mode, cached.as_deref()) {
        BuildAction::Reuse(id) => {
            if let Err(e) = pithos::docker::tag_image(&id, &tag) {
                narrate(style, "» ERROR:", &format!("{e}"));
                return Err(ExitCode::from(1));
            }
            narrate(
                style,
                "»",
                &format!("cached image {id} matches fingerprint; reusing as {tag}"),
            );
            return Ok(EnsuredImage { tag, project });
        }
        BuildAction::Abort => {
            narrate(style, "» ERROR:", &abort_message(&project));
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
                "» ERROR:",
                &format!("cannot create build-context tempdir: {e}"),
            );
            return Err(ExitCode::from(1));
        }
    };
    if let Err(e) = pithos::embed::extract_to(context.path()) {
        narrate(
            style,
            "» ERROR:",
            &format!("cannot extract build context: {e}"),
        );
        return Err(ExitCode::from(1));
    }

    // First pass: fingerprint label only. Installer RUN steps populate
    // /opt/pithos-versions/<tc> inside the image as a side-effect.
    match pithos::docker::build_request(
        pithos::docker::BuildRequest {
            context: context.path(),
            dockerfile: dockerfile_path,
            project: &project,
            fingerprint: &hash,
            extra_labels: &BTreeMap::new(),
        },
        style,
    ) {
        Ok(()) => {}
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(style, "» ERROR:", &format!("docker build: {e}"));
            return Err(ExitCode::from(1));
        }
        Err(pithos::docker::BuildError::NonZero { code, tail }) => {
            let code_str = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            narrate(
                style,
                "» ERROR:",
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
                "» ERROR:",
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
    narrate(style, "»", &format!("resolved: {}", resolved.join(", ")));

    let mut extra_labels: BTreeMap<String, String> = BTreeMap::new();
    for (name, version) in &versions {
        extra_labels.insert(
            pithos::fingerprint::version_label_key(name),
            version.clone(),
        );
    }

    match pithos::docker::build_request(
        pithos::docker::BuildRequest {
            context,
            dockerfile: dockerfile_path,
            project,
            fingerprint: hash,
            extra_labels: &extra_labels,
        },
        style,
    ) {
        Ok(()) => Ok(()),
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(
                style,
                "» ERROR:",
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
                "» ERROR:",
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
    target: &RunTarget,
    tmux: bool,
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
    let extensions_manifest_path = cwd.join(".pithos.d").join("extensions.list");
    let clipboard_shim_dir = cwd.join(".pithos.d");
    let clipboard_bridge =
        match pithos::clipboard_bridge::ClipboardBridge::start(&clipboard_shim_dir) {
            Ok(bridge) => Some(bridge),
            Err(e) => {
                narrate(
                    style,
                    "» clipboard:",
                    &format!("host clipboard bridge unavailable: {e}"),
                );
                None
            }
        };
    let clipboard_url = clipboard_bridge
        .as_ref()
        .map(pithos::clipboard_bridge::ClipboardBridge::container_url);

    let command = effective_command(target);

    // When --tmux is set, run the effective command inside a named tmux
    // session so a second terminal can attach and co-drive it. The wrapper
    // must pass a concrete command, so an empty command is materialized to the
    // Pi launch argv inside `tmux_wrap`.
    let wrapped;
    let effective_cmd: &[String] = if tmux {
        narrate(
            style,
            "» tmux:",
            &format!(
                "observe from another terminal: docker exec -it pithos-{}-{} tmux attach -t pithos",
                ensured.project,
                std::process::id()
            ),
        );
        wrapped = pithos::docker::tmux_wrap(&command);
        &wrapped
    } else {
        &command
    };

    match pithos::docker::run_request(pithos::docker::RunRequest {
        image_tag: &ensured.tag,
        project: &ensured.project,
        workspace: cwd,
        pithos_repo: pithos_repo.as_deref(),
        extensions_manifest: Some(&extensions_manifest_path),
        environment: pithos::docker::RunEnvironment {
            env_file: env_file.as_deref(),
            clipboard_url: clipboard_url.as_deref(),
            clipboard_shim: clipboard_bridge.as_ref().map(|bridge| bridge.shim_path()),
        },
        command: effective_cmd,
    }) {
        Ok(status) => ExitCode::from(exit_code_from_status(status)),
        Err(pithos::docker::RunError::Spawn(e)) => {
            narrate(style, "» ERROR:", &format!("docker run: {e}"));
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RebuildStatus {
    NotBuilt,
    Cached,
    RebuildNeeded,
    Unavailable,
}

fn rebuild_status(current_fp: &str, image: Option<&pithos::docker::ImageInfo>) -> RebuildStatus {
    match image {
        None => RebuildStatus::NotBuilt,
        Some(info) => match &info.fingerprint {
            Some(label) if label == current_fp => RebuildStatus::Cached,
            _ => RebuildStatus::RebuildNeeded,
        },
    }
}

fn summarize_config(yaml: &YamlOwned) -> String {
    let toolchains: Vec<String> = pithos::dockerfile::toolchain_names(yaml).collect();
    let apt_count = pithos::dockerfile::apt_package_count(yaml);
    let tc_part = if toolchains.is_empty() {
        "toolchains: none".to_string()
    } else {
        format!("toolchains: {}", toolchains.join(", "))
    };
    let apt_part = match apt_count {
        0 => "extras.apt: none".to_string(),
        1 => "extras.apt: 1 package".to_string(),
        n => format!("extras.apt: {n} packages"),
    };
    format!("{tc_part}; {apt_part}")
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn render_info(
    project: &str,
    config_summary: &str,
    fingerprint: &str,
    tag: &str,
    image: Option<&pithos::docker::ImageInfo>,
    status: RebuildStatus,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("project:      {project}\n"));
    out.push_str(&format!("config:       {config_summary}\n"));
    out.push_str(&format!("fingerprint:  {fingerprint}\n"));
    out.push_str(&format!("image:        {tag}\n"));
    if let Some(info) = image {
        out.push_str(&format!("image id:     {}\n", info.id));
        out.push_str(&format!("image size:   {}\n", format_size(info.size_bytes)));
        out.push_str(&format!("created:      {}\n", info.created));
    }
    let status_str = match status {
        RebuildStatus::NotBuilt => "not-built",
        RebuildStatus::Cached => "cached",
        RebuildStatus::RebuildNeeded => "rebuild-needed",
        RebuildStatus::Unavailable => "unavailable",
    };
    out.push_str(&format!("status:       {status_str}\n"));
    out
}

fn run_info(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_content: &str,
    style: Style,
) -> ExitCode {
    let project = match pithos::project::name_from_path(cwd) {
        Some(n) => n,
        None => {
            narrate(
                style,
                "» ERROR:",
                &format!("cannot derive project name from {}", cwd.display()),
            );
            return ExitCode::from(1);
        }
    };
    let tag = format!("pithos:{project}");

    let mut installers = BTreeMap::new();
    for name in pithos::dockerfile::toolchain_names(yaml) {
        let Some(bytes) = pithos::embed::installer_bytes(&name) else {
            narrate(
                style,
                "» ERROR:",
                &format!(
                    "no baked installer for toolchain {name:?} \
                     (config validator and embed bundle are out of sync)"
                ),
            );
            return ExitCode::from(1);
        };
        installers.insert(name, bytes.to_vec());
    }
    let base_image_id = match pithos::docker::inspect_image_id(pithos::docker::BASE_IMAGE_REF) {
        Ok(id) => id,
        Err(e) => {
            narrate(style, "» ERROR:", &format!("{e}"));
            return ExitCode::from(1);
        }
    };
    let fingerprint = pithos::fingerprint::compute(
        dockerfile_content,
        pithos_bytes,
        &installers,
        pithos::embed::PI_BUN_COMPAT_MJS,
        &base_image_id,
    );

    let (image, status) = match pithos::docker::inspect_image(&tag) {
        Ok(img_opt) => {
            let status = rebuild_status(&fingerprint, img_opt.as_ref());
            (img_opt, status)
        }
        Err(e) => {
            narrate(
                style,
                "»",
                &format!("docker unreachable; image status unavailable: {e}"),
            );
            (None, RebuildStatus::Unavailable)
        }
    };

    let config_summary = summarize_config(yaml);
    let rendered = render_info(
        &project,
        &config_summary,
        &fingerprint,
        &tag,
        image.as_ref(),
        status,
    );
    print!("{rendered}");
    ExitCode::SUCCESS
}

/// Pure: lowercase, trim, accept `y` or `yes` only. Treats CRLF and trailing
/// whitespace as ignorable. Empty string → false (used by the EOF path in
/// `prompt_confirm`, so piped/CI input without a tty defaults to "no").
fn parse_confirm_answer(line: &str) -> bool {
    let trimmed = line.trim().to_ascii_lowercase();
    trimmed == "y" || trimmed == "yes"
}

/// Narrate the prompt to stderr (newline-terminated, matching house style),
/// read one line from stdin, classify with `parse_confirm_answer`. EOF maps
/// to false — safer default for piped/non-interactive use.
fn prompt_confirm(prompt_msg: &str, style: Style) -> bool {
    narrate(style, "»", prompt_msg);
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => false,
        Ok(_) => parse_confirm_answer(&buf),
        Err(_) => false,
    }
}

/// Pure: lowercase, trim, accept anything except `n`/`no` as yes.
/// Empty/whitespace-only → yes. Mirrors `parse_confirm_answer` but flips
/// the default for the `.pithos` create-on-missing prompt where Enter
/// should accept the offered minimal stub.
fn parse_confirm_answer_default_yes(line: &str) -> bool {
    let trimmed = line.trim().to_ascii_lowercase();
    !(trimmed == "n" || trimmed == "no")
}

/// Like `prompt_confirm` but yes-default for typed input. EOF still maps
/// to false so non-interactive callers (closed stdin under `assert_cmd`,
/// CI without redirect) don't silently mutate the filesystem.
fn prompt_confirm_default_yes(prompt_msg: &str, style: Style) -> bool {
    narrate(style, "»", prompt_msg);
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) => false,
        Ok(_) => parse_confirm_answer_default_yes(&buf),
        Err(_) => false,
    }
}

/// Pure: render one indented row per image for the candidate list narration.
/// Format: `<tag-or-none>  <id-12>  <created>` — single spaces between fields,
/// the caller adds the `»   ` prefix when narrating each row.
fn render_candidate_lines(images: &[pithos::docker::PithosImage]) -> Vec<String> {
    images
        .iter()
        .map(|img| {
            let tag = img.tag.as_deref().unwrap_or("<none>");
            let id_short: String = img.id.chars().take(12).collect();
            format!("{tag}  {id_short}  {created}", created = img.created)
        })
        .collect()
}

/// Pure: union of dangling and tagged candidate lists, dedupe-by-id.
/// Docker semantics put tagged and dangling-labeled images in disjoint sets,
/// so dedupe is belt-and-suspenders against an upstream bug. Order: dangling
/// first, then tagged (insertion order preserved within each input).
fn merge_candidates(
    dangling: Vec<pithos::docker::PithosImage>,
    tagged: Vec<pithos::docker::PithosImage>,
) -> Vec<pithos::docker::PithosImage> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(dangling.len() + tagged.len());
    for img in dangling.into_iter().chain(tagged.into_iter()) {
        if seen.insert(img.id.clone()) {
            out.push(img);
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum CleanDecision {
    Nothing,
    Prompt(Vec<pithos::docker::PithosImage>),
}

/// Pure: decide what `run_clean` should do given the lists. `all=false`
/// ignores the tagged list entirely (default mode = dangling-only).
fn decide_clean(
    all: bool,
    dangling: Vec<pithos::docker::PithosImage>,
    tagged: Vec<pithos::docker::PithosImage>,
) -> CleanDecision {
    let candidates = if all {
        merge_candidates(dangling, tagged)
    } else {
        dangling
    };
    if candidates.is_empty() {
        CleanDecision::Nothing
    } else {
        CleanDecision::Prompt(candidates)
    }
}

/// Pure: classify per-image removal results into a single (exit_code, lines)
/// pair. Best-effort: every result is rendered. Exit 1 if any `Err` present,
/// 0 if all `Ok`. The trailing `done.` line is emitted only on full success.
fn summarize_removal_outcome(results: &[Result<String, String>]) -> (u8, Vec<String>) {
    let mut lines = Vec::with_capacity(results.len() + 1);
    let mut had_err = false;
    for r in results {
        match r {
            Ok(label) => lines.push(format!("removed {label}")),
            Err(msg) => {
                lines.push(format!("ERROR: {msg}"));
                had_err = true;
            }
        }
    }
    if had_err {
        (1, lines)
    } else {
        lines.push("done.".to_string());
        (0, lines)
    }
}

/// Impure wrapper: list candidates, decide, prompt, remove. Best-effort
/// removal — keeps going past failures, reports each, exits 1 if any failed.
/// Empty list → "No images to clean." exit 0. User says no → "aborted." exit 0.
fn run_clean(all: bool, style: Style) -> ExitCode {
    let dangling = match pithos::docker::list_dangling_pithos_images() {
        Ok(v) => v,
        Err(e) => {
            narrate(style, "» ERROR:", &format!("{e}"));
            return ExitCode::from(1);
        }
    };
    let tagged = if all {
        match pithos::docker::list_tagged_pithos_images() {
            Ok(v) => v,
            Err(e) => {
                narrate(style, "» ERROR:", &format!("{e}"));
                return ExitCode::from(1);
            }
        }
    } else {
        Vec::new()
    };

    let decision = decide_clean(all, dangling, tagged);
    let candidates = match decision {
        CleanDecision::Nothing => {
            narrate(style, "»", "No images to clean.");
            return ExitCode::SUCCESS;
        }
        CleanDecision::Prompt(c) => c,
    };

    narrate(style, "»", &format!("Found {} image(s):", candidates.len()));
    for line in render_candidate_lines(&candidates) {
        narrate(style, "»", &format!("  {line}"));
    }

    let prompt = format!("Remove {} image(s)? [y/N]:", candidates.len());
    if !prompt_confirm(&prompt, style) {
        narrate(style, "»", "aborted.");
        return ExitCode::SUCCESS;
    }

    let mut results: Vec<Result<String, String>> = Vec::with_capacity(candidates.len());
    for img in &candidates {
        let label = img
            .tag
            .clone()
            .unwrap_or_else(|| img.id.chars().take(12).collect::<String>());
        match pithos::docker::remove_image(&img.id) {
            Ok(()) => results.push(Ok(label)),
            Err(e) => results.push(Err(format!("cannot remove {label}: {e}"))),
        }
    }

    let (code, lines) = summarize_removal_outcome(&results);
    for line in lines {
        narrate(style, "»", &line);
    }
    ExitCode::from(code)
}

/// Build `Dockerfile.base` in `cwd` into the local `ghcr.io/anton-kochev/pithos:base`
/// tag, for dev-iteration use. `cwd` must be the pithos source tree (the
/// repository checkout where `Dockerfile.base` lives at the root).
///
/// Exit codes:
/// - 2 if `Dockerfile.base` is not found in `cwd` (wrong directory).
/// - 1 if `docker build` fails to spawn or returns non-zero.
/// - 0 on success, after narrating the newly-resolved Image ID.
fn run_rebuild_base(cwd: &Path, style: Style) -> ExitCode {
    let dockerfile_base = cwd.join("Dockerfile.base");
    if !dockerfile_base.exists() {
        narrate(
            style,
            "» ERROR:",
            &format!(
                "must be run from the pithos source tree (Dockerfile.base not found in {})",
                cwd.display()
            ),
        );
        return ExitCode::from(2);
    }

    narrate(
        style,
        "»",
        &format!("building {} ...", pithos::docker::BASE_IMAGE_REF),
    );
    match pithos::docker::build_base(cwd, style) {
        Ok(()) => {}
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(style, "» ERROR:", &format!("docker build: {e}"));
            return ExitCode::from(1);
        }
        Err(pithos::docker::BuildError::NonZero { code, tail }) => {
            let code_str = code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            narrate(
                style,
                "» ERROR:",
                &format!(
                    "docker build failed (exit {code_str}); last {} lines:",
                    tail.len()
                ),
            );
            for line in &tail {
                eprintln!("{}", pithos::output::format_docker_line(line, style));
            }
            return ExitCode::from(1);
        }
    }

    match pithos::docker::inspect_image_id(pithos::docker::BASE_IMAGE_REF) {
        Ok(id) => {
            narrate(
                style,
                "»",
                &format!("rebuilt {} -> {id}", pithos::docker::BASE_IMAGE_REF),
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            // Build succeeded but inspect failed — unusual, but report rather
            // than masking with a success exit.
            narrate(
                style,
                "» ERROR:",
                &format!("build succeeded but inspect failed: {e}"),
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saphyr::LoadableYamlNode;

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
    fn from_args_no_subcommand_defaults_to_pi() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec![]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_typo_is_unknown_subcommand() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "buidl"])),
            Subcommand::Reject {
                kind: RejectKind::Subcommand,
                value: "buidl".to_string(),
            }
        );
    }

    #[test]
    fn from_args_run_accepts_pithos_options() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--tmux", "--rebuild"])),
            Subcommand::Run {
                mode: RunMode::Rebuild,
                target: RunTarget::Pi(vec![]),
                tmux: true,
            }
        );
    }

    #[test]
    fn from_args_run_rejects_conflicting_build_options() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--no-build"])),
            Subcommand::Reject {
                kind: RejectKind::Usage,
                value: "--rebuild and --no-build are mutually exclusive".to_string(),
            }
        );
    }

    #[test]
    fn from_args_fork_and_following_arguments_are_forwarded_to_pi() {
        assert_eq!(
            Subcommand::from_args(&args(&[
                "pithos",
                "--fork",
                "01a0335e",
                "Continue from here",
            ])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec![
                    "--fork".to_string(),
                    "01a0335e".to_string(),
                    "Continue from here".to_string(),
                ]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_all_pi_options_are_opaque() {
        assert_eq!(
            Subcommand::from_args(&args(&[
                "pithos",
                "--provider",
                "openai",
                "--model",
                "gpt-4o",
                "--no-session",
                "-p",
                "Review this",
            ])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec![
                    "--provider".to_string(),
                    "openai".to_string(),
                    "--model".to_string(),
                    "gpt-4o".to_string(),
                    "--no-session".to_string(),
                    "-p".to_string(),
                    "Review this".to_string(),
                ]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_pi_extension_flag_is_forwarded_without_registration() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "--plan", "carefully"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec!["--plan".to_string(), "carefully".to_string()]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_pithos_options_precede_pi_argument_tail() {
        assert_eq!(
            Subcommand::from_args(&args(&[
                "pithos",
                "--tmux",
                "--no-build",
                "--session",
                "01a0335e",
            ])),
            Subcommand::Run {
                mode: RunMode::NoBuild,
                target: RunTarget::Pi(vec!["--session".to_string(), "01a0335e".to_string(),]),
                tmux: true,
            }
        );
    }

    #[test]
    fn from_args_does_not_parse_pithos_options_inside_pi_argument_tail() {
        assert_eq!(
            Subcommand::from_args(&args(&[
                "pithos",
                "--fork",
                "01a0335e",
                "--rebuild",
                "--tmux",
            ])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec![
                    "--fork".to_string(),
                    "01a0335e".to_string(),
                    "--rebuild".to_string(),
                    "--tmux".to_string(),
                ]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_pi_marker_forwards_positional_arguments() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "--pi", "Review this repository"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec!["Review this repository".to_string()]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_pi_marker_forwards_double_dash_to_pi() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "--pi", "--", "- bullet point"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec!["--".to_string(), "- bullet point".to_string()]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_pi_marker_forwards_pithos_option_names() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "--pi", "--tmux", "--rebuild"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec!["--tmux".to_string(), "--rebuild".to_string(),]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_run_accepts_bare_container_command() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash", "-lc", "echo hi"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Command(vec![
                    "bash".to_string(),
                    "-lc".to_string(),
                    "echo hi".to_string(),
                ]),
                tmux: false,
            }
        );
    }

    #[test]
    fn from_args_double_dash_starts_container_command() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--tmux", "--", "bash"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Command(vec!["bash".to_string()]),
                tmux: true,
            }
        );
    }

    #[test]
    fn from_args_pi_double_dash_after_option_stays_in_pi_tail() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "-p", "--", "- bullet point"])),
            Subcommand::Run {
                mode: RunMode::Default,
                target: RunTarget::Pi(vec![
                    "-p".to_string(),
                    "--".to_string(),
                    "- bullet point".to_string(),
                ]),
                tmux: false,
            }
        );
    }

    #[test]
    fn pi_argv_empty_falls_through_to_image_cmd() {
        assert!(pi_argv(&[]).is_empty());
    }

    #[test]
    fn pi_argv_prepends_image_launch_argv_and_preserves_arguments() {
        let pi_args = vec![
            "--fork".to_string(),
            "01a0335e".to_string(),
            "Continue from here".to_string(),
        ];

        let argv = pi_argv(&pi_args);

        let launch = pithos::dockerfile::PI_LAUNCH_ARGV;
        assert_eq!(&argv[..launch.len()], &launch[..]);
        assert_eq!(&argv[launch.len()..], pi_args.as_slice());
    }

    #[test]
    fn effective_command_materializes_pi_target() {
        let target = RunTarget::Pi(vec!["--continue".to_string()]);

        let command = effective_command(&target);

        assert_eq!(command.last().unwrap(), "--continue");
        assert_eq!(
            &command[..pithos::dockerfile::PI_LAUNCH_ARGV.len()],
            &pithos::dockerfile::PI_LAUNCH_ARGV[..]
        );
    }

    #[test]
    fn effective_command_preserves_explicit_command() {
        let target = RunTarget::Command(vec![
            "bash".to_string(),
            "-lc".to_string(),
            "echo hi".to_string(),
        ]);

        assert_eq!(
            effective_command(&target),
            vec!["bash".to_string(), "-lc".to_string(), "echo hi".to_string(),]
        );
    }

    #[test]
    fn pi_arguments_survive_tmux_wrapper() {
        let target = RunTarget::Pi(vec!["--fork".to_string(), "01a0335e".to_string()]);
        let command = effective_command(&target);

        let wrapped = pithos::docker::tmux_wrap(&command);

        assert_eq!(&wrapped[wrapped.len() - 2..], &["--fork", "01a0335e"]);
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
        for name in [
            "run",
            "build",
            "info",
            "clean",
            "rebuild-base",
            "help",
            "version",
        ] {
            assert!(t.contains(name), "help missing subcommand {name:?}: {t}");
        }
    }

    #[test]
    fn help_text_documents_pithos_and_pi_arguments() {
        let t = help_text();
        for flag in ["--rebuild", "--no-build", "--tmux", "--pi"] {
            assert!(t.contains(flag), "help missing pithos option {flag}: {t}");
        }
        for flag in ["--session", "--fork", "--provider", "--model"] {
            assert!(t.contains(flag), "help missing Pi example {flag}: {t}");
        }
        assert!(
            t.contains("forwarded") && t.contains("verbatim"),
            "help does not explain Pi argument forwarding: {t}"
        );
    }

    #[test]
    fn help_text_documents_toolchain_flat_form() {
        // Lock the .pithos toolchain-format section so a refactor that drops
        // it from the prose still fails CI.
        let t = help_text();
        assert!(
            t.contains("Config (.pithos):"),
            "help missing config section: {t}"
        );
        assert!(t.contains("flat form"), "help missing flat-form note: {t}");
        assert!(
            t.contains("dotnet: \"10.0.0\""),
            "help missing flat-form example: {t}"
        );
        assert!(
            t.contains("`version:` keys are not supported"),
            "help missing nested-form warning: {t}"
        );
    }

    #[test]
    fn version_text_matches_cargo_pkg_version() {
        assert_eq!(
            version_text(),
            format!("pithos {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn rebuild_status_not_built_when_no_image() {
        assert_eq!(rebuild_status("abc", None), RebuildStatus::NotBuilt);
    }

    #[test]
    fn rebuild_status_cached_when_fingerprint_matches() {
        let img = pithos::docker::ImageInfo {
            id: "sha256:x".into(),
            size_bytes: 1,
            created: "now".into(),
            fingerprint: Some("abc".into()),
        };
        assert_eq!(rebuild_status("abc", Some(&img)), RebuildStatus::Cached);
    }

    #[test]
    fn rebuild_status_rebuild_needed_when_fingerprint_differs() {
        let img = pithos::docker::ImageInfo {
            id: "sha256:x".into(),
            size_bytes: 1,
            created: "now".into(),
            fingerprint: Some("other".into()),
        };
        assert_eq!(
            rebuild_status("abc", Some(&img)),
            RebuildStatus::RebuildNeeded
        );
    }

    #[test]
    fn rebuild_status_rebuild_needed_when_label_absent() {
        let img = pithos::docker::ImageInfo {
            id: "sha256:x".into(),
            size_bytes: 1,
            created: "now".into(),
            fingerprint: None,
        };
        assert_eq!(
            rebuild_status("abc", Some(&img)),
            RebuildStatus::RebuildNeeded
        );
    }

    #[test]
    fn format_size_bytes_below_kb() {
        assert_eq!(format_size(512), "512 B");
    }

    #[test]
    fn format_size_kb() {
        assert_eq!(format_size(2048), "2.0 KB");
    }

    #[test]
    fn format_size_mb() {
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn format_size_gb() {
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn summarize_config_lists_toolchains_and_apt_count() {
        let yaml = saphyr::YamlOwned::load_from_str(
            "toolchains:\n  rust: \"1.85.0\"\n  dotnet: \"10.0.102\"\nextras:\n  apt: [git, libssl3]\n",
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
        let s = summarize_config(&yaml);
        assert!(s.contains("dotnet"), "{s}");
        assert!(s.contains("rust"), "{s}");
        assert!(s.contains("2 packages"), "{s}");
    }

    #[test]
    fn summarize_config_with_no_toolchains_and_no_apt() {
        let yaml = saphyr::YamlOwned::load_from_str("toolchains: {}\n")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let s = summarize_config(&yaml);
        assert!(s.contains("toolchains: none"), "{s}");
        assert!(s.contains("extras.apt: none"), "{s}");
    }

    #[test]
    fn summarize_config_with_single_apt_uses_singular_package() {
        let yaml = saphyr::YamlOwned::load_from_str("toolchains: {}\nextras:\n  apt: [git]\n")
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let s = summarize_config(&yaml);
        assert!(s.contains("1 package"), "{s}");
        assert!(!s.contains("1 packages"), "{s}");
    }

    #[test]
    fn render_info_includes_all_fields_when_image_present() {
        let img = pithos::docker::ImageInfo {
            id: "sha256:abc".into(),
            size_bytes: 1024 * 1024,
            created: "2026-04-24T10:30:00Z".into(),
            fingerprint: Some("ff".into()),
        };
        let out = render_info(
            "widgets",
            "toolchains: rust; extras.apt: none",
            "ff",
            "pithos:widgets",
            Some(&img),
            RebuildStatus::Cached,
        );
        assert!(out.contains("project:      widgets"), "{out}");
        assert!(out.contains("fingerprint:  ff"), "{out}");
        assert!(out.contains("image:        pithos:widgets"), "{out}");
        assert!(out.contains("image id:     sha256:abc"), "{out}");
        assert!(out.contains("image size:   1.0 MB"), "{out}");
        assert!(out.contains("created:      2026-04-24T10:30:00Z"), "{out}");
        assert!(out.contains("status:       cached"), "{out}");
    }

    #[test]
    fn render_info_omits_image_rows_when_not_built() {
        let out = render_info(
            "widgets",
            "toolchains: none; extras.apt: none",
            "ff",
            "pithos:widgets",
            None,
            RebuildStatus::NotBuilt,
        );
        assert!(out.contains("status:       not-built"), "{out}");
        assert!(!out.contains("image id:"), "{out}");
        assert!(!out.contains("image size:"), "{out}");
        assert!(!out.contains("created:"), "{out}");
    }

    #[test]
    fn render_info_shows_unavailable_status_when_daemon_unreachable() {
        let out = render_info(
            "widgets",
            "toolchains: rust; extras.apt: none",
            "ff",
            "pithos:widgets",
            None,
            RebuildStatus::Unavailable,
        );
        assert!(out.contains("status:       unavailable"), "{out}");
        assert!(!out.contains("image id:"), "{out}");
    }

    #[test]
    fn from_args_info_bare() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "info"])),
            Subcommand::Info
        );
    }

    #[test]
    fn from_args_info_rejects_trailing_arg() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "info", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn from_args_clean_bare() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean"])),
            Subcommand::Clean { all: false }
        );
    }

    #[test]
    fn from_args_clean_all() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean", "--all"])),
            Subcommand::Clean { all: true }
        );
    }

    #[test]
    fn from_args_clean_double_all_is_idempotent() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean", "--all", "--all"])),
            Subcommand::Clean { all: true }
        );
    }

    #[test]
    fn from_args_clean_rejects_unknown_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean", "--nope"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--nope".to_string(),
            }
        );
    }

    #[test]
    fn from_args_clean_rejects_trailing_positional() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn from_args_clean_rejects_all_after_positional() {
        // Locks the no-positional-cmd policy: a positional anywhere in `clean`
        // is a reject, even if `--all` follows.
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "clean", "extra", "--all"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn subcommand_clean_does_not_write_dockerfile() {
        assert!(!Subcommand::Clean { all: false }.writes_dockerfile());
        assert!(!Subcommand::Clean { all: true }.writes_dockerfile());
    }

    #[test]
    fn subcommand_clean_requires_daemon() {
        assert!(Subcommand::Clean { all: false }.requires_daemon());
        assert!(Subcommand::Clean { all: true }.requires_daemon());
    }

    #[test]
    fn from_args_rebuild_base_bare() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "rebuild-base"])),
            Subcommand::RebuildBase
        );
    }

    #[test]
    fn from_args_rebuild_base_rejects_unknown_flag() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "rebuild-base", "--foo"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "--foo".to_string(),
            }
        );
    }

    #[test]
    fn from_args_rebuild_base_rejects_trailing_positional() {
        // Mirrors `help extra` / `version extra` / `info extra` — a positional
        // after a zero-arg subcommand is a Flag-kind reject.
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "rebuild-base", "extra"])),
            Subcommand::Reject {
                kind: RejectKind::Flag,
                value: "extra".to_string(),
            }
        );
    }

    #[test]
    fn subcommand_rebuild_base_requires_daemon() {
        assert!(Subcommand::RebuildBase.requires_daemon());
    }

    #[test]
    fn subcommand_rebuild_base_does_not_write_dockerfile() {
        // The per-project Dockerfile under .pithos.d/ is never emitted by
        // rebuild-base — locks the short-circuit in main().
        assert!(!Subcommand::RebuildBase.writes_dockerfile());
    }

    #[test]
    fn help_text_lists_rebuild_base() {
        // Lock the rebuild-base entry in `pithos help` so a refactor that
        // drops it from the prose still fails CI.
        let t = help_text();
        assert!(t.contains("rebuild-base"), "help missing rebuild-base: {t}");
    }

    #[test]
    fn parse_confirm_answer_accepts_y_lowercase() {
        assert!(parse_confirm_answer("y"));
        assert!(parse_confirm_answer("y\n"));
    }

    #[test]
    fn parse_confirm_answer_accepts_yes_lowercase() {
        assert!(parse_confirm_answer("yes"));
        assert!(parse_confirm_answer("yes\n"));
    }

    #[test]
    fn parse_confirm_answer_accepts_mixed_case_yes() {
        assert!(parse_confirm_answer("Y"));
        assert!(parse_confirm_answer("YES"));
        assert!(parse_confirm_answer("Yes"));
        assert!(parse_confirm_answer("YeS"));
    }

    #[test]
    fn parse_confirm_answer_treats_crlf_like_lf() {
        assert!(parse_confirm_answer("y\r\n"));
        assert!(parse_confirm_answer("yes\r\n"));
    }

    #[test]
    fn parse_confirm_answer_rejects_n_no_empty_garbage() {
        assert!(!parse_confirm_answer("n"));
        assert!(!parse_confirm_answer("no"));
        assert!(!parse_confirm_answer("garbage"));
    }

    #[test]
    fn parse_confirm_answer_rejects_whitespace_only() {
        assert!(!parse_confirm_answer("   "));
        assert!(!parse_confirm_answer("\t\n"));
    }

    #[test]
    fn parse_confirm_answer_rejects_yep_and_other_near_matches() {
        assert!(!parse_confirm_answer("yep"));
        assert!(!parse_confirm_answer("yeah"));
        assert!(!parse_confirm_answer("ya"));
    }

    #[test]
    fn parse_confirm_answer_rejects_empty_string() {
        // Locks the EOF→false invariant via the pure path. `prompt_confirm`'s
        // Ok(0) branch returns false directly, but if a future refactor pipes
        // the empty string through `parse_confirm_answer`, the answer stays no.
        assert!(!parse_confirm_answer(""));
    }

    #[test]
    fn parse_confirm_answer_default_yes_accepts_empty() {
        // Empty line / Enter without typing → yes (default-accept).
        assert!(parse_confirm_answer_default_yes(""));
        assert!(parse_confirm_answer_default_yes("\n"));
        assert!(parse_confirm_answer_default_yes("\r\n"));
    }

    #[test]
    fn parse_confirm_answer_default_yes_accepts_whitespace_only() {
        assert!(parse_confirm_answer_default_yes("   "));
        assert!(parse_confirm_answer_default_yes("\t\n"));
    }

    #[test]
    fn parse_confirm_answer_default_yes_accepts_y_yes_mixed_case() {
        assert!(parse_confirm_answer_default_yes("y"));
        assert!(parse_confirm_answer_default_yes("Y"));
        assert!(parse_confirm_answer_default_yes("yes"));
        assert!(parse_confirm_answer_default_yes("YES"));
        assert!(parse_confirm_answer_default_yes("Yes\n"));
    }

    #[test]
    fn parse_confirm_answer_default_yes_rejects_n_and_no() {
        assert!(!parse_confirm_answer_default_yes("n"));
        assert!(!parse_confirm_answer_default_yes("N"));
        assert!(!parse_confirm_answer_default_yes("no"));
        assert!(!parse_confirm_answer_default_yes("NO"));
        assert!(!parse_confirm_answer_default_yes("no\n"));
        assert!(!parse_confirm_answer_default_yes("No\r\n"));
    }

    #[test]
    fn parse_confirm_answer_default_yes_accepts_garbage_as_yes() {
        // Non-`n`/`no` typed input is treated as accept — symmetric with the
        // default-no parser's rule that anything not `y`/`yes` is reject.
        assert!(parse_confirm_answer_default_yes("garbage"));
        assert!(parse_confirm_answer_default_yes("yep"));
        assert!(parse_confirm_answer_default_yes("nope"));
    }

    #[test]
    fn render_candidate_lines_empty_list_returns_empty_vec() {
        assert!(render_candidate_lines(&[]).is_empty());
    }

    #[test]
    fn render_candidate_lines_emits_one_row_per_image() {
        let imgs = vec![
            pithos::docker::PithosImage {
                id: "sha256:abc123def456ghi".into(),
                tag: Some("pithos:widgets".into()),
                created: "2026-04-24".into(),
            },
            pithos::docker::PithosImage {
                id: "sha256:zzz999".into(),
                tag: None,
                created: "2026-04-25".into(),
            },
        ];
        let lines = render_candidate_lines(&imgs);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn render_candidate_lines_preserves_input_order() {
        let imgs = vec![
            pithos::docker::PithosImage {
                id: "sha256:first".into(),
                tag: Some("pithos:a".into()),
                created: "2026-04-24".into(),
            },
            pithos::docker::PithosImage {
                id: "sha256:second".into(),
                tag: Some("pithos:b".into()),
                created: "2026-04-25".into(),
            },
        ];
        let lines = render_candidate_lines(&imgs);
        assert!(lines[0].contains("pithos:a"));
        assert!(lines[1].contains("pithos:b"));
    }

    #[test]
    fn render_candidate_lines_handles_dangling_tag() {
        let imgs = vec![pithos::docker::PithosImage {
            id: "sha256:abc".into(),
            tag: None,
            created: "2026-04-24".into(),
        }];
        let lines = render_candidate_lines(&imgs);
        assert!(lines[0].contains("<none>"), "{:?}", lines);
    }

    fn img(id: &str, tag: Option<&str>) -> pithos::docker::PithosImage {
        pithos::docker::PithosImage {
            id: id.to_string(),
            tag: tag.map(String::from),
            created: "2026-04-24".into(),
        }
    }

    #[test]
    fn merge_candidates_dedupes_by_id() {
        let dangling = vec![img("sha256:a", None)];
        let tagged = vec![
            img("sha256:a", Some("pithos:x")),
            img("sha256:b", Some("pithos:y")),
        ];
        let merged = merge_candidates(dangling, tagged);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "sha256:a");
        // First wins: dangling entry preserved over the tagged duplicate.
        assert!(merged[0].tag.is_none());
        assert_eq!(merged[1].id, "sha256:b");
    }

    #[test]
    fn merge_candidates_preserves_dangling_first_then_tagged_order() {
        let dangling = vec![img("sha256:d1", None), img("sha256:d2", None)];
        let tagged = vec![img("sha256:t1", Some("pithos:x"))];
        let merged = merge_candidates(dangling, tagged);
        assert_eq!(merged[0].id, "sha256:d1");
        assert_eq!(merged[1].id, "sha256:d2");
        assert_eq!(merged[2].id, "sha256:t1");
    }

    #[test]
    fn merge_candidates_with_empty_inputs_returns_empty() {
        assert!(merge_candidates(Vec::new(), Vec::new()).is_empty());
    }

    #[test]
    fn decide_clean_returns_nothing_for_empty_dangling_when_not_all() {
        assert_eq!(decide_clean(false, vec![], vec![]), CleanDecision::Nothing);
    }

    #[test]
    fn decide_clean_returns_nothing_for_empty_union_when_all() {
        assert_eq!(decide_clean(true, vec![], vec![]), CleanDecision::Nothing);
    }

    #[test]
    fn decide_clean_returns_prompt_for_dangling_only_when_not_all() {
        let d = vec![img("sha256:a", None)];
        match decide_clean(false, d, vec![]) {
            CleanDecision::Prompt(v) => assert_eq!(v.len(), 1),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn decide_clean_returns_prompt_for_union_when_all() {
        let d = vec![img("sha256:a", None)];
        let t = vec![img("sha256:b", Some("pithos:x"))];
        match decide_clean(true, d, t) {
            CleanDecision::Prompt(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn decide_clean_ignores_tagged_when_not_all() {
        // Locks the default-mode contract: tagged images are never considered
        // candidates unless --all is set.
        let t = vec![
            img("sha256:t1", Some("pithos:x")),
            img("sha256:t2", Some("pithos:y")),
        ];
        assert_eq!(decide_clean(false, vec![], t), CleanDecision::Nothing);
    }

    #[test]
    fn summarize_removal_outcome_all_ok_returns_zero() {
        let results: Vec<Result<String, String>> =
            vec![Ok("pithos:a".into()), Ok("pithos:b".into())];
        let (code, lines) = summarize_removal_outcome(&results);
        assert_eq!(code, 0);
        assert!(lines.iter().any(|l| l.contains("removed pithos:a")));
        assert!(lines.iter().any(|l| l.contains("removed pithos:b")));
        assert!(lines.iter().any(|l| l.contains("done")));
    }

    #[test]
    fn summarize_removal_outcome_partial_failure_renders_all_and_returns_one() {
        let results: Vec<Result<String, String>> = vec![
            Ok("pithos:a".into()),
            Err("nope".into()),
            Ok("pithos:c".into()),
        ];
        let (code, lines) = summarize_removal_outcome(&results);
        assert_eq!(code, 1);
        assert!(lines.iter().any(|l| l.contains("removed pithos:a")));
        assert!(lines.iter().any(|l| l.contains("ERROR: nope")));
        assert!(lines.iter().any(|l| l.contains("removed pithos:c")));
        assert!(!lines.iter().any(|l| l.contains("done.")));
    }
}

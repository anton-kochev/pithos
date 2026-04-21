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

#[derive(Debug, PartialEq, Eq)]
enum Subcommand {
    None,
    Build { rebuild: bool },
    Run { rebuild: bool, cmd: Vec<String> },
    Reject { kind: RejectKind, value: String },
}

impl Subcommand {
    fn from_args(args: &[String]) -> Self {
        match args.get(1).map(String::as_str) {
            None => Self::None,
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
                let mut rebuild = false;
                let mut cmd: Vec<String> = Vec::new();
                let mut iter = args.iter().skip(2);
                while let Some(arg) = iter.next() {
                    match arg.as_str() {
                        "--" => {
                            cmd.extend(iter.by_ref().cloned());
                            break;
                        }
                        "--rebuild" => rebuild = true,
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
                Self::Run { rebuild, cmd }
            }
            Some(other) => Self::Reject {
                kind: RejectKind::Subcommand,
                value: other.to_string(),
            },
        }
    }
}

fn main() -> ExitCode {
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
        return ExitCode::from(1);
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

    match subcommand {
        Subcommand::None => ExitCode::SUCCESS,
        Subcommand::Build { rebuild } => run_build(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
            rebuild,
            style,
        ),
        Subcommand::Run { rebuild, cmd } => run_run(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
            rebuild,
            &cmd,
            style,
        ),
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

/// Return the cached image id to reuse, if any, given the `--rebuild` flag
/// and the result of a fingerprint cache lookup. Pure — no I/O.
fn cached_image_to_reuse(rebuild: bool, cached: Option<&str>) -> Option<&str> {
    if rebuild { None } else { cached }
}

/// Return `<cwd>/.env` if it exists, else `None`. Extracted so the
/// conditional `--env-file` branch in `run_run` is unit-testable. No I/O
/// beyond a `Path::exists()` probe.
fn discover_env_file(cwd: &Path) -> Option<std::path::PathBuf> {
    let p = cwd.join(".env");
    p.exists().then_some(p)
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
    rebuild: bool,
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
    for name in pithos::dockerfile::toolchain_names(yaml) {
        let Some(bytes) = pithos::embed::installer_bytes(&name) else {
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
        installers.insert(name, bytes.to_vec());
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
    if let Some(id) = cached_image_to_reuse(rebuild, cached.as_deref()) {
        narrate(
            style,
            ">>",
            &format!("cached image {id} matches fingerprint; skipping build"),
        );
        return Ok(EnsuredImage { tag, project });
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

    match pithos::docker::build(context.path(), dockerfile_path, &project, &hash, style) {
        Ok(()) => Ok(EnsuredImage { tag, project }),
        Err(pithos::docker::BuildError::Spawn(e)) => {
            narrate(style, ">> ERROR:", &format!("docker build: {e}"));
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
                    "docker build failed (exit {code_str}); last {} lines:",
                    tail.len()
                ),
            );
            for line in &tail {
                eprintln!("{}", pithos::output::format_docker_line(line, style));
            }
            Err(ExitCode::from(3))
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
    match ensure_image(
        cwd,
        yaml,
        pithos_bytes,
        dockerfile_path,
        dockerfile_content,
        rebuild,
        style,
    ) {
        Ok(_) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn run_run(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_path: &Path,
    dockerfile_content: &str,
    rebuild: bool,
    cmd: &[String],
    style: Style,
) -> ExitCode {
    let ensured = match ensure_image(
        cwd,
        yaml,
        pithos_bytes,
        dockerfile_path,
        dockerfile_content,
        rebuild,
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
        // status.code() is None on signal-death (128+N convention); report
        // 1 rather than synthesize a signal code — 7.3 owns SIGINT → 130
        // properly.
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
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

    #[test]
    fn cached_image_to_reuse_rebuild_with_cache_returns_none() {
        // --rebuild overrides a cache hit → run the build
        assert_eq!(cached_image_to_reuse(true, Some("abc123")), None);
    }

    #[test]
    fn cached_image_to_reuse_rebuild_without_cache_returns_none() {
        // --rebuild with no cache → still run the build
        assert_eq!(cached_image_to_reuse(true, None), None);
    }

    #[test]
    fn cached_image_to_reuse_no_rebuild_without_cache_returns_none() {
        // No flag, no cache → build
        assert_eq!(cached_image_to_reuse(false, None), None);
    }

    #[test]
    fn cached_image_to_reuse_no_rebuild_with_cache_returns_id() {
        // No flag, cache hit → skip and surface the id
        assert_eq!(cached_image_to_reuse(false, Some("abc123")), Some("abc123"));
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
    fn from_args_no_subcommand_is_none() {
        assert_eq!(Subcommand::from_args(&args(&["pithos"])), Subcommand::None);
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
            Subcommand::Run { rebuild: false, cmd: vec![] }
        );
    }

    #[test]
    fn from_args_run_with_rebuild() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild"])),
            Subcommand::Run { rebuild: true, cmd: vec![] }
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
            Subcommand::Run { rebuild: true, cmd: vec!["extra".to_string()] }
        );
    }

    #[test]
    fn from_args_run_accepts_bare_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash"])),
            Subcommand::Run { rebuild: false, cmd: vec!["bash".to_string()] }
        );
    }

    #[test]
    fn from_args_run_accepts_cmd_with_args() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "bash", "-c", "echo"])),
            Subcommand::Run {
                rebuild: false,
                cmd: vec!["bash".to_string(), "-c".to_string(), "echo".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_accepts_cmd_after_double_dash() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--", "bash", "-c", "echo"])),
            Subcommand::Run {
                rebuild: false,
                cmd: vec!["bash".to_string(), "-c".to_string(), "echo".to_string()],
            }
        );
    }

    #[test]
    fn from_args_run_rebuild_before_double_dash() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--", "bash"])),
            Subcommand::Run { rebuild: true, cmd: vec!["bash".to_string()] }
        );
    }

    #[test]
    fn from_args_run_trailing_double_dash_yields_empty_cmd() {
        assert_eq!(
            Subcommand::from_args(&args(&["pithos", "run", "--rebuild", "--"])),
            Subcommand::Run { rebuild: true, cmd: vec![] }
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
                rebuild: false,
                cmd: vec!["bash".to_string(), "--rebuild".to_string()],
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
}

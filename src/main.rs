use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use saphyr::YamlOwned;

enum Subcommand {
    None,
    Build,
    Unknown(String),
}

impl Subcommand {
    fn from_args(args: &[String]) -> Self {
        match args.get(1).map(String::as_str) {
            None => Self::None,
            Some("build") => Self::Build,
            Some(other) => Self::Unknown(other.to_string()),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let subcommand = Subcommand::from_args(&args);

    // Fail fast on unknown subcommand before any I/O — typos like `pithos buidl`
    // shouldn't require a `.pithos` file or mutate `.pithos.d/Dockerfile`.
    if let Subcommand::Unknown(name) = &subcommand {
        eprintln!(">> ERROR: unknown subcommand: {name}");
        return ExitCode::from(1);
    }

    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(">> ERROR: cannot read cwd: {e}");
            return ExitCode::from(1);
        }
    };
    let pithos_bytes = match read_pithos(&cwd) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let yaml = match pithos::config::load(&pithos_bytes) {
        Ok(y) => y,
        Err(e) => {
            eprintln!(">> ERROR: {e}");
            return ExitCode::from(2);
        }
    };
    let dockerfile_path = cwd.join(".pithos.d").join("Dockerfile");
    let dockerfile_content = pithos::dockerfile::emit(&yaml);
    if let Err(code) = write_dockerfile(&dockerfile_path, &dockerfile_content) {
        return code;
    }

    match subcommand {
        Subcommand::None => ExitCode::SUCCESS,
        Subcommand::Build => run_build(
            &cwd,
            &yaml,
            &pithos_bytes,
            &dockerfile_path,
            &dockerfile_content,
        ),
        Subcommand::Unknown(_) => unreachable!("handled by fail-fast guard above"),
    }
}

fn read_pithos(cwd: &Path) -> Result<Vec<u8>, ExitCode> {
    let path = cwd.join(".pithos");
    match fs::read(&path) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!(">> ERROR: .pithos not found");
            eprintln!(">> Create a .pithos file at the project root. Minimal example:");
            eprintln!(">>");
            eprintln!(">>   toolchains: {{}}");
            Err(ExitCode::from(2))
        }
        Err(e) => {
            eprintln!(">> ERROR: {}: {e}", path.display());
            Err(ExitCode::from(1))
        }
    }
}

fn write_dockerfile(path: &Path, content: &str) -> Result<(), ExitCode> {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(">> ERROR: cannot create {}: {e}", parent.display());
            return Err(ExitCode::from(1));
        }
    }
    if let Err(e) = fs::write(path, content) {
        eprintln!(">> ERROR: cannot write {}: {e}", path.display());
        return Err(ExitCode::from(1));
    }
    Ok(())
}

fn run_build(
    cwd: &Path,
    yaml: &YamlOwned,
    pithos_bytes: &[u8],
    dockerfile_path: &Path,
    dockerfile_content: &str,
) -> ExitCode {
    let project = match pithos::project::name_from_path(cwd) {
        Some(n) => n,
        None => {
            eprintln!(
                ">> ERROR: cannot derive project name from {}",
                cwd.display()
            );
            return ExitCode::from(1);
        }
    };

    let mut installers = BTreeMap::new();
    for name in pithos::dockerfile::toolchain_names(yaml) {
        let Some(bytes) = pithos::embed::installer_bytes(&name) else {
            eprintln!(
                ">> ERROR: no baked installer for toolchain {name:?} \
                 (config validator and embed bundle are out of sync)"
            );
            return ExitCode::from(1);
        };
        installers.insert(name, bytes.to_vec());
    }
    let hash = pithos::fingerprint::compute(dockerfile_content, pithos_bytes, &installers);

    // `TempDir` cleans on Drop; SIGINT runs Drop, SIGKILL leaks under `$TMPDIR` for the OS to reap.
    let context = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            eprintln!(">> ERROR: cannot create build-context tempdir: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = pithos::embed::extract_to(context.path()) {
        eprintln!(">> ERROR: cannot extract build context: {e}");
        return ExitCode::from(1);
    }

    match pithos::docker::build(context.path(), dockerfile_path, &project, &hash) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(">> ERROR: {e}");
            ExitCode::from(1)
        }
    }
}

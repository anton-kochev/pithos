use std::env;
use std::fs;
use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(">> ERROR: cannot read cwd: {e}");
            return ExitCode::from(1);
        }
    };
    let path = cwd.join(".pithos");
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!(">> ERROR: .pithos not found");
            eprintln!(">> Create a .pithos file at the project root. Minimal example:");
            eprintln!(">>");
            eprintln!(">>   toolchains: {{}}");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!(">> ERROR: {}: {e}", path.display());
            return ExitCode::from(1);
        }
    };
    match pithos::config::load(&bytes) {
        Ok(yaml) => {
            let dir = cwd.join(".pithos.d");
            if let Err(e) = fs::create_dir_all(&dir) {
                eprintln!(">> ERROR: cannot create {}: {e}", dir.display());
                return ExitCode::from(1);
            }
            let dockerfile = dir.join("Dockerfile");
            let content = pithos::dockerfile::emit(&yaml);
            if let Err(e) = fs::write(&dockerfile, content) {
                eprintln!(">> ERROR: cannot write {}: {e}", dockerfile.display());
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(">> ERROR: {e}");
            ExitCode::from(2)
        }
    }
}

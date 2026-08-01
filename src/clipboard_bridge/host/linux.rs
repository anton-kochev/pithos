use super::{looks_like_png, remaining_until, run_stdout};
use std::time::Instant;

pub(super) fn read_png(deadline: Instant) -> Option<Vec<u8>> {
    if let Some(types) = run_stdout("wl-paste", &["--list-types"], remaining_until(deadline)?) {
        let types = String::from_utf8_lossy(&types);
        if types.lines().any(|line| line.trim() == "image/png") {
            if let Some(bytes) = run_stdout(
                "wl-paste",
                &["--type", "image/png", "--no-newline"],
                remaining_until(deadline)?,
            ) {
                if looks_like_png(&bytes) {
                    return Some(bytes);
                }
            }
        }
    }

    if let Some(targets) = run_stdout(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        remaining_until(deadline)?,
    ) {
        let targets = String::from_utf8_lossy(&targets);
        if targets.lines().any(|line| line.trim() == "image/png") {
            if let Some(bytes) = run_stdout(
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"],
                remaining_until(deadline)?,
            ) {
                if looks_like_png(&bytes) {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

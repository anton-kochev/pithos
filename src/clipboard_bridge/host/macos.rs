use super::{
    looks_like_png, read_file_limited, remaining_until, run_stdout, run_success, secure_temp_path,
};
use std::fs;
use std::time::Instant;

pub(super) fn read_png(deadline: Instant) -> Option<Vec<u8>> {
    if let Some(bytes) = run_stdout("pngpaste", &["-"], remaining_until(deadline)?) {
        if looks_like_png(&bytes) {
            return Some(bytes);
        }
    }

    if let Some(bytes) = read_clipboard_class("PNGf", "png", deadline) {
        if looks_like_png(&bytes) {
            return Some(bytes);
        }
    }

    // macOS screenshots copied with Cmd+Ctrl+Shift+4 are often exposed as TIFF
    // rather than PNG. Convert with the built-in `sips` tool so Pi receives a
    // supported image/png payload.
    let tiff_path = write_clipboard_class_to_file("TIFF", "tiff", deadline)?;
    let Some(png_path) = secure_temp_path(".png") else {
        let _ = fs::remove_file(&tiff_path);
        return None;
    };
    let Some(timeout) = remaining_until(deadline) else {
        let _ = fs::remove_file(&tiff_path);
        let _ = fs::remove_file(&png_path);
        return None;
    };
    let converted = run_success(
        "sips",
        &[
            "-s",
            "format",
            "png",
            &tiff_path.to_string_lossy(),
            "--out",
            &png_path.to_string_lossy(),
        ],
        timeout,
    );
    let _ = fs::remove_file(&tiff_path);
    if !converted {
        let _ = fs::remove_file(&png_path);
        return None;
    }
    let bytes = read_file_limited(&png_path);
    let _ = fs::remove_file(&png_path);
    remaining_until(deadline)?;
    bytes.filter(|bytes| looks_like_png(bytes))
}

fn read_clipboard_class(class_code: &str, ext: &str, deadline: Instant) -> Option<Vec<u8>> {
    let path = write_clipboard_class_to_file(class_code, ext, deadline)?;
    let bytes = read_file_limited(&path);
    let _ = fs::remove_file(&path);
    remaining_until(deadline)?;
    bytes
}

fn write_clipboard_class_to_file(
    class_code: &str,
    ext: &str,
    deadline: Instant,
) -> Option<std::path::PathBuf> {
    let path = secure_temp_path(&format!(".{ext}"))?;
    let path_str = escape_applescript_string(&path.to_string_lossy());
    let Some(timeout) = remaining_until(deadline) else {
        let _ = fs::remove_file(&path);
        return None;
    };
    let copied = run_success(
        "osascript",
        &[
            "-e",
            &format!("set outPath to \"{path_str}\""),
            "-e",
            &format!("set imageData to the clipboard as «class {class_code}»"),
            "-e",
            "set outFile to open for access POSIX file outPath with write permission",
            "-e",
            "set eof outFile to 0",
            "-e",
            "write imageData to outFile",
            "-e",
            "close access outFile",
        ],
        timeout,
    );
    if !copied {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(path)
}

pub(in crate::clipboard_bridge) fn escape_applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

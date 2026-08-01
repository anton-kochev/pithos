use super::{looks_like_png, read_file_limited, remaining_until, run_success, secure_temp_path};
use std::fs;
use std::time::Instant;

pub(super) fn read_png(deadline: Instant) -> Option<Vec<u8>> {
    let path = secure_temp_path(".png")?;
    // System.Drawing refuses to save over an open/existing image file. The
    // name was created securely above; remove the placeholder before saving.
    let _ = fs::remove_file(&path);
    let path_str = path.to_string_lossy().replace('\'', "''");
    let script = [
        "Add-Type -AssemblyName System.Windows.Forms",
        "Add-Type -AssemblyName System.Drawing",
        &format!("$path = '{path_str}'"),
        "$image = [System.Windows.Forms.Clipboard]::GetImage()",
        "if ($image) { $image.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); exit 0 } else { exit 1 }",
    ]
    .join("; ");
    let copied = run_success(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-STA", "-Command", &script],
        remaining_until(deadline)?,
    );
    if !copied {
        let _ = fs::remove_file(&path);
        return None;
    }
    let bytes = read_file_limited(&path);
    let _ = fs::remove_file(&path);
    remaining_until(deadline)?;
    bytes.filter(|bytes| looks_like_png(bytes))
}

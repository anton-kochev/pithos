use std::path::Path;

/// Derive the Docker tag suffix from a path's basename per FR-403.
///
/// Sanitization pipeline:
/// 1. Take the basename (last path component); return `None` if the path
///    has no basename (e.g. `/`, `..`) or if the basename starts with `.`
///    (dotfile-style names collide under sanitization — `~/dotfiles` and
///    `~/.dotfiles` would both become `dotfiles`).
/// 2. For each character: keep if it's ASCII `[a-z0-9]` (lowercased);
///    otherwise emit a single `-`, collapsing consecutive non-conforming
///    characters into one dash.
/// 3. Trim leading and trailing `-` (Docker rejects tags starting with `-`).
/// 4. Truncate to 128 characters (Docker tag length limit), then trim any
///    trailing `-` exposed by truncation.
/// 5. Return `None` if the result is empty.
///
/// Non-ASCII characters degrade silently (`Café` → `caf`); users with such
/// paths should rename. Documented as accepted behavior, not a feature.
pub fn name_from_path(path: &Path) -> Option<String> {
    let basename = path.file_name()?.to_str()?;
    if basename.starts_with('.') {
        return None;
    }
    let mut out = String::with_capacity(basename.len());
    let mut last_was_dash = false;
    for ch in basename.chars() {
        let lc = ch.to_ascii_lowercase();
        if matches!(lc, 'a'..='z' | '0'..='9') {
            out.push(lc);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(128).collect();
    let final_trimmed = capped.trim_end_matches('-').to_string();
    (!final_trimmed.is_empty()).then_some(final_trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_path_passes_through_simple_lowercase_basename() {
        assert_eq!(
            name_from_path(Path::new("/home/user/myapp")),
            Some("myapp".to_string())
        );
    }

    #[test]
    fn name_from_path_lowercases_uppercase_letters() {
        assert_eq!(
            name_from_path(Path::new("/projects/My-App")),
            Some("my-app".to_string())
        );
    }

    #[test]
    fn name_from_path_collapses_non_conforming_chars_into_single_dash() {
        assert_eq!(
            name_from_path(Path::new("/x/My App!")),
            Some("my-app".to_string())
        );
    }

    #[test]
    fn name_from_path_collapses_underscores_and_runs() {
        assert_eq!(
            name_from_path(Path::new("/x/foo___bar")),
            Some("foo-bar".to_string())
        );
    }

    #[test]
    fn name_from_path_trims_leading_and_trailing_dashes_and_collapses_runs() {
        assert_eq!(
            name_from_path(Path::new("/x/--leading---trailing--")),
            Some("leading-trailing".to_string())
        );
    }

    #[test]
    fn name_from_path_returns_none_for_leading_dot_basename() {
        // `.dotfiles` and `dotfiles` would otherwise collide on the same
        // docker tag — reject leading-dot basenames outright.
        assert_eq!(name_from_path(Path::new("/x/.config")), None);
        assert_eq!(name_from_path(Path::new("/x/.dotfiles")), None);
    }

    #[test]
    fn name_from_path_returns_none_for_all_non_ascii_basename() {
        assert_eq!(name_from_path(Path::new("/x/🚀")), None);
    }

    #[test]
    fn name_from_path_returns_none_for_root_path() {
        assert_eq!(name_from_path(Path::new("/")), None);
    }

    #[test]
    fn name_from_path_returns_none_for_dotdot() {
        assert_eq!(name_from_path(Path::new("..")), None);
    }

    #[test]
    fn name_from_path_caps_output_at_128_chars() {
        let long = "a".repeat(200);
        let out = name_from_path(Path::new(&long)).unwrap();
        assert_eq!(out.len(), 128);
        assert!(out.chars().all(|c| c == 'a'));
    }
}

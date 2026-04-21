//! Output styling for pithos narration (bold `>>`) and docker build output
//! (dim + 2-space indent). Honors `NO_COLOR` and non-TTY stderr per §6.4.

use std::io::{self, IsTerminal, Read, Write};

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy)]
pub struct Style {
    enabled: bool,
}

/// Pure decision: style-on iff stderr is a TTY AND `NO_COLOR` is unset.
/// Split from `detect` so the four-cell truth table is unit-testable without
/// mutating process env.
fn decide(is_tty: bool, no_color_unset: bool) -> bool {
    is_tty && no_color_unset
}

impl Style {
    pub fn detect() -> Self {
        Self {
            enabled: decide(
                io::stderr().is_terminal(),
                std::env::var_os("NO_COLOR").is_none(),
            ),
        }
    }

    // Test-only constructors. Gate as `pub(crate)` so the public API doesn't
    // advertise knobs users shouldn't touch.
    #[cfg(test)]
    pub(crate) fn colored() -> Self {
        Self { enabled: true }
    }
    #[cfg(test)]
    pub(crate) fn plain() -> Self {
        Self { enabled: false }
    }

    pub fn bold(self, s: &str) -> String {
        if self.enabled {
            format!("{BOLD}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
    pub fn dim(self, s: &str) -> String {
        if self.enabled {
            format!("{DIM}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
}

/// Build a narration line: `<marker> <message>` where only the marker is
/// bolded. `marker` is one of `">>"`, `">> ERROR:"`, `">> WARN:"`. Returns
/// a `String` so it's trivial to unit-test without capturing stderr.
pub fn format_narration(style: Style, marker: &str, message: &str) -> String {
    format!("{} {message}", style.bold(marker))
}

/// Thin wrapper: formats and writes to stderr. Call sites replace
/// `eprintln!(">> ...")` with this.
pub fn narrate(style: Style, marker: &str, message: &str) {
    eprintln!("{}", format_narration(style, marker, message));
}

/// Transform one line of docker output: 2-space indent + dim. Blank lines
/// pass through unchanged (no `  ` on empty lines).
pub fn format_docker_line(line: &str, style: Style) -> String {
    if line.is_empty() {
        String::new()
    } else {
        style.dim(&format!("  {line}"))
    }
}

/// Read `reader` line by line, write styled docker output to `writer`.
/// Generic over `W: Write` so tests inject `Vec<u8>`; production passes
/// `std::io::stderr()`. Errors writing to the sink are dropped — stderr
/// failure has no recovery path.
pub fn stream_lines<R: Read, W: Write>(reader: R, mut writer: W, style: Style) {
    use std::io::BufRead;
    let br = io::BufReader::new(reader);
    for line in br.lines().map_while(Result::ok) {
        let _ = writeln!(writer, "{}", format_docker_line(&line, style));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Style decision truth table (4 cells)

    #[test]
    fn decide_enables_on_tty_and_no_color_unset() {
        assert!(decide(true, true));
    }

    #[test]
    fn decide_disables_when_not_tty() {
        assert!(!decide(false, true));
    }

    #[test]
    fn decide_disables_when_no_color_set() {
        assert!(!decide(true, false));
    }

    #[test]
    fn decide_disables_when_neither() {
        assert!(!decide(false, false));
    }

    // Formatters, color on/off

    #[test]
    fn bold_wraps_when_enabled() {
        let s = Style::colored().bold("x");
        assert!(s.contains("\x1b[1m"));
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn bold_passthrough_when_disabled() {
        assert_eq!(Style::plain().bold("x"), "x");
    }

    #[test]
    fn dim_wraps_when_enabled() {
        let s = Style::colored().dim("x");
        assert!(s.contains("\x1b[2m"));
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn dim_passthrough_when_disabled() {
        assert_eq!(Style::plain().dim("x"), "x");
    }

    // Narration formatter (bolds marker only)

    #[test]
    fn format_narration_bolds_only_the_marker_when_enabled() {
        let out = format_narration(Style::colored(), ">>", "msg");
        assert_eq!(out, "\x1b[1m>>\x1b[0m msg");
    }

    #[test]
    fn format_narration_is_plain_when_disabled() {
        assert_eq!(format_narration(Style::plain(), ">>", "msg"), ">> msg");
    }

    #[test]
    fn format_narration_supports_error_marker() {
        assert_eq!(
            format_narration(Style::plain(), ">> ERROR:", "boom"),
            ">> ERROR: boom"
        );
    }

    // Docker line formatter

    #[test]
    fn format_docker_line_indents_when_disabled() {
        assert_eq!(format_docker_line("Step 4/7", Style::plain()), "  Step 4/7");
    }

    #[test]
    fn format_docker_line_wraps_in_dim_when_enabled() {
        let out = format_docker_line("Step 4/7", Style::colored());
        assert!(out.starts_with("\x1b[2m"));
        assert!(out.contains("  Step 4/7"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn format_docker_line_passes_through_blank() {
        assert_eq!(format_docker_line("", Style::colored()), "");
        assert_eq!(format_docker_line("", Style::plain()), "");
    }

    #[test]
    fn format_docker_line_preserves_ansi_in_input() {
        let input = "\x1b[31mred\x1b[0m";
        let out = format_docker_line(input, Style::plain());
        assert!(out.contains(input), "input ANSI not preserved: {out:?}");
    }

    // Streaming sink (no docker daemon required)

    #[test]
    fn stream_lines_writes_each_line_to_sink_when_disabled() {
        let input: &[u8] = b"a\nb\n";
        let mut sink: Vec<u8> = Vec::new();
        stream_lines(input, &mut sink, Style::plain());
        assert_eq!(sink, b"  a\n  b\n");
    }

    #[test]
    fn stream_lines_applies_dim_when_enabled() {
        let input: &[u8] = b"a\nb\n";
        let mut sink: Vec<u8> = Vec::new();
        stream_lines(input, &mut sink, Style::colored());
        let text = String::from_utf8(sink).unwrap();
        // Two lines, each dim-wrapped.
        assert_eq!(text.matches("\x1b[2m").count(), 2);
        assert_eq!(text.matches("\x1b[0m").count(), 2);
        assert!(text.contains("  a"));
        assert!(text.contains("  b"));
    }

    #[test]
    fn stream_lines_drops_blank_lines_gracefully() {
        let input: &[u8] = b"a\n\nb\n";
        let mut sink: Vec<u8> = Vec::new();
        stream_lines(input, &mut sink, Style::plain());
        assert_eq!(sink, b"  a\n\n  b\n");
    }
}

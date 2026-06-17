//! Terminal colors, byte formatting, prompts.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const MAGENTA: &str = "\x1b[35m";
pub const RED: &str = "\x1b[31m";
pub const RESET: &str = "\x1b[0m";

/// Whether ANSI color is emitted to stdout, set once at startup from the TTY
/// check. When stdout is piped/redirected the report is stripped to plain text.
static COLOR: AtomicBool = AtomicBool::new(true);

/// Detect once whether stdout is a terminal; call at program start before any
/// output. When stdout is not a TTY, `emit`/`emitln` strip ANSI codes so a
/// redirected report is clean plain text. The stderr progress renderer is gated
/// separately (on stderr), so piping stdout still shows live bars on a TTY stderr.
pub fn init_color() {
    COLOR.store(std::io::stdout().is_terminal(), Ordering::Relaxed);
}

fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// Write `s` to stdout verbatim, or with ANSI SGR codes stripped when color is off.
pub fn emit(s: &str) {
    if color_enabled() {
        print!("{s}");
    } else {
        print!("{}", strip_ansi(s));
    }
}

/// `emit` plus a trailing newline.
pub fn emitln(s: &str) {
    if color_enabled() {
        println!("{s}");
    } else {
        println!("{}", strip_ansi(s));
    }
}

/// Remove ANSI escape sequences (`ESC [ … <final letter>`) — covers every SGR
/// color code this program emits.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Mirror of the bash `human()` awk: B/KB/MB/GB/TB, one decimal, caps at TB.
pub fn human(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < 4 {
        b /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", b, units[i])
}

/// Magenta group header. Mirrors bash `group()`.
pub fn group(title: &str) {
    emitln("");
    emitln(&format!("{BOLD}{MAGENTA}━━━ {title} ━━━{RESET}"));
}

/// Mirror of bash `confirm()`. `force` corresponds to bash `force_interactive`
/// being non-empty: when set, always prompt even under `--yes`.
/// Matches bash regex `^[Yy]$` — exactly the single character `y` or `Y`.
pub fn confirm(prompt: &str, yes: bool, force: bool) -> bool {
    if yes && !force {
        emitln(&format!("{prompt} [y/N] y {DIM}(auto -y){RESET}"));
        return true;
    }
    emit(&format!("{prompt} [y/N] "));
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let line = line.trim_end_matches(['\n', '\r']);
    line == "y" || line == "Y"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_formats_each_unit() {
        assert_eq!(human(0), "0.0 B");
        assert_eq!(human(512), "512.0 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
        assert_eq!(human(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human(1024u64.pow(4)), "1.0 TB");
        // Caps at TB like the bash version (i < 5 / index < 4).
        assert_eq!(human(1024u64.pow(5)), "1024.0 TB");
    }

    #[test]
    fn strip_ansi_removes_sgr_codes() {
        assert_eq!(strip_ansi("\x1b[1mhi\x1b[0m"), "hi");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[33m\x1b[1mX\x1b[0m"), "X");
    }
}

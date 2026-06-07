//! Plain-stdout output discipline — the non-TUI surface.
//!
//! Status/step/ok/warn/err helpers plus the interactive `prompt`/`confirm`
//! used on the non-interactive fallback path. Mirrors the migration-assistant
//! CLI's `ui` module so the two crates feel identical at the terminal.
//!
//! The pure formatting helpers (`fmt_*`, `resolve_pick`) are unit-tested; the
//! printing wrappers are thin and side-effecting.

use std::io::{self, Write};

const C_RESET: &str = "\x1b[0m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_RED: &str = "\x1b[31m";
const C_DIM: &str = "\x1b[2m";
const C_BOLD: &str = "\x1b[1m";

/// Format a banner line (bold). Pure.
pub fn fmt_banner(text: &str) -> String {
    format!("{C_BOLD}=== {text} ==={C_RESET}")
}

/// Format a numbered/step line. Pure.
pub fn fmt_step(text: &str) -> String {
    format!("{C_BOLD}▶ {text}{C_RESET}")
}

/// Format an ok line. Pure.
pub fn fmt_ok(text: &str) -> String {
    format!("{C_GREEN}✔{C_RESET} {text}")
}

/// Format a warn line. Pure.
pub fn fmt_warn(text: &str) -> String {
    format!("{C_YELLOW}!{C_RESET} {text}")
}

/// Format an error line. Pure.
pub fn fmt_err(text: &str) -> String {
    format!("{C_RED}error:{C_RESET} {text}")
}

/// Format a dim line. Pure.
pub fn fmt_dim(text: &str) -> String {
    format!("{C_DIM}{text}{C_RESET}")
}

pub fn banner(text: &str) {
    println!("{}", fmt_banner(text));
}
pub fn step(text: &str) {
    println!("{}", fmt_step(text));
}
pub fn ok(text: &str) {
    println!("{}", fmt_ok(text));
}
pub fn info(text: &str) {
    println!("{text}");
}
pub fn dim(text: &str) {
    println!("{}", fmt_dim(text));
}
pub fn warn(text: &str) {
    eprintln!("{}", fmt_warn(text));
}
pub fn err(text: &str) {
    eprintln!("{}", fmt_err(text));
}

/// Prompt for a line of input with a default. In non-interactive mode (or on
/// EOF) the default is returned without reading. Pure-ish: I/O only when
/// interactive.
pub fn prompt(label: &str, default: &str, non_interactive: bool) -> String {
    if non_interactive {
        return default.to_string();
    }
    print!("{label} [{default}]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => default.to_string(),
        Ok(_) => {
            let t = line.trim();
            if t.is_empty() {
                default.to_string()
            } else {
                t.to_string()
            }
        }
    }
}

/// Yes/no confirm with a default. Non-interactive returns the default.
pub fn confirm(label: &str, default_yes: bool, non_interactive: bool) -> bool {
    if non_interactive {
        return default_yes;
    }
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{label} [{hint}]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => default_yes,
        Ok(_) => match line.trim().to_ascii_lowercase().as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default_yes,
        },
    }
}

/// Resolve a picker selection (a 1-based index string or a label) against the
/// option ids/labels. Returns the chosen id, or `None` if unresolved. Pure.
pub fn resolve_pick<'a>(pick: &str, ids: &[&'a str], labels: &[&str]) -> Option<&'a str> {
    let t = pick.trim();
    // 1-based numeric index.
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 && n <= ids.len() {
            return Some(ids[n - 1]);
        }
    }
    // Exact id or label match (case-insensitive).
    let tl = t.to_ascii_lowercase();
    if let Some(i) = ids.iter().position(|id| id.to_ascii_lowercase() == tl) {
        return Some(ids[i]);
    }
    if let Some(i) = labels.iter().position(|l| l.to_ascii_lowercase() == tl) {
        return Some(ids[i]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatters_wrap_with_ansi_and_text() {
        assert!(fmt_ok("done").contains("done"));
        assert!(fmt_ok("done").contains(C_GREEN));
        assert!(fmt_err("boom").contains("boom"));
        assert!(fmt_step("go").contains('▶'));
        assert!(fmt_banner("Hi").contains("=== Hi ==="));
    }

    #[test]
    fn prompt_non_interactive_returns_default() {
        assert_eq!(prompt("x", "def", true), "def");
    }

    #[test]
    fn confirm_non_interactive_returns_default() {
        assert!(confirm("x", true, true));
        assert!(!confirm("x", false, true));
    }

    #[test]
    fn resolve_pick_by_index() {
        let ids = ["a", "b", "c"];
        let labels = ["A", "B", "C"];
        assert_eq!(resolve_pick("2", &ids, &labels), Some("b"));
        assert_eq!(resolve_pick("1", &ids, &labels), Some("a"));
        assert_eq!(resolve_pick("0", &ids, &labels), None);
        assert_eq!(resolve_pick("9", &ids, &labels), None);
    }

    #[test]
    fn resolve_pick_by_id_and_label() {
        let ids = ["local", "cloud"];
        let labels = ["Local", "Cloud"];
        assert_eq!(resolve_pick("cloud", &ids, &labels), Some("cloud"));
        assert_eq!(resolve_pick("LOCAL", &ids, &labels), Some("local"));
        assert_eq!(resolve_pick("Cloud", &ids, &labels), Some("cloud"));
        assert_eq!(resolve_pick("nope", &ids, &labels), None);
    }
}

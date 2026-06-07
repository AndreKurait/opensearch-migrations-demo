//! Small, dependency-free helpers shared across the harness.
//!
//! Kept hand-rolled (no `regex`/`indoc`) to match the minimal-dependency
//! philosophy of the migration-assistant CLI.

/// Indent every non-empty line of `text` by `spaces` spaces. Used when nesting
/// one emitted manifest block inside another (e.g. a config file embedded in a
/// ConfigMap's `data:`). Empty lines are left empty (no trailing whitespace).
pub fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{pad}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `name` is a safe Kubernetes/DNS-1123 label segment: lowercase
/// alphanumerics and '-', not starting/ending with '-', non-empty, ≤63 chars.
/// The harness only generates names from a fixed stem, but resource emitters
/// assert their inputs with this so a future rename can't emit an invalid name.
pub fn is_dns1123_label(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Escape a string for safe single-line embedding in a YAML double-quoted
/// scalar: backslash and double-quote are escaped. (Our emitted values are
/// simple — versions, names, URLs — so this minimal escaping suffices.)
pub fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_pads_non_empty_lines_only() {
        let out = indent("a\n\nb", 2);
        assert_eq!(out, "  a\n\n  b");
    }

    #[test]
    fn indent_handles_single_line() {
        assert_eq!(indent("x", 4), "    x");
    }

    #[test]
    fn dns_label_accepts_valid_names() {
        assert!(is_dns1123_label("ma-demo-source"));
        assert!(is_dns1123_label("opensearch"));
        assert!(is_dns1123_label("a1"));
    }

    #[test]
    fn dns_label_rejects_invalid_names() {
        assert!(!is_dns1123_label(""));
        assert!(!is_dns1123_label("-leading"));
        assert!(!is_dns1123_label("trailing-"));
        assert!(!is_dns1123_label("Upper"));
        assert!(!is_dns1123_label("under_score"));
        assert!(!is_dns1123_label(&"x".repeat(64)));
    }

    #[test]
    fn yaml_quote_escapes_specials() {
        assert_eq!(yaml_quote("simple"), "\"simple\"");
        assert_eq!(yaml_quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(yaml_quote(r"a\b"), r#""a\\b""#);
    }
}

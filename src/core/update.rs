//! Startup update check — is a newer ma-demo release published on GitHub?
//!
//! Pure semver comparison ([`is_newer`], [`parse_latest_tag`]) plus a
//! fail-silent probe ([`check`]) that curls the demo repo's latest-release API
//! through the [`CommandRunner`] seam. Everything degrades to "no prompt" on any
//! error/timeout so the check never blocks or breaks startup, and it's inert
//! under [`MockRunner`] (so tests don't hit the network). Opt out with
//! `MA_DEMO_NO_UPDATE_CHECK=1`.

use crate::runner::CommandRunner;

/// The GitHub repo whose releases we check (this harness's own repo).
pub const RELEASE_REPO: &str = "AndreKurait/opensearch-migrations-demo";

/// Parse a semver-ish version into `(major, minor, patch)`, ignoring a leading
/// `v` and any `-suffix`/`+build`. Unparseable parts are 0.
pub fn parse_semver(v: &str) -> (u64, u64, u64) {
    let v = v.trim().trim_start_matches('v');
    // Drop pre-release / build metadata.
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut it = core.split('.');
    let p = |x: Option<&str>| x.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    (p(it.next()), p(it.next()), p(it.next()))
}

/// Whether `latest` is a newer release than `current` (semver-ordered). A dev
/// build (e.g. `0.1.0-dev`) compares by its numeric core, so a published
/// `0.1.0` is NOT "newer" than `0.1.0-dev` — avoids nagging local dev builds.
pub fn is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

/// Extract the `tag_name` from a GitHub `releases/latest` JSON body.
pub fn parse_latest_tag(json: &str) -> Option<String> {
    let key = "\"tag_name\"";
    let i = json.find(key)? + key.len();
    let rest = &json[i..];
    let colon = rest.find(':')? + 1;
    let after = &rest[colon..];
    let q1 = after.find('"')? + 1;
    let q2 = after[q1..].find('"')? + q1;
    Some(after[q1..q2].to_string())
}

/// The result of an update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// A newer release is available — carries its tag.
    Available { latest: String },
    /// Already up to date (or a dev build ahead of the latest release).
    UpToDate,
    /// The check was skipped or couldn't determine an answer (offline, opted
    /// out, no curl, bad response) — never surfaced as an error.
    Unknown,
}

/// Run the fail-silent update check for `current` through `runner`. Returns
/// [`Update::Available`] only when a strictly-newer release is published.
/// Respects `MA_DEMO_NO_UPDATE_CHECK`. The curl is bounded by `--max-time` so a
/// slow network can't hang startup.
pub fn check<R: CommandRunner>(runner: &R, current: &str) -> Update {
    if std::env::var("MA_DEMO_NO_UPDATE_CHECK").is_ok() {
        return Update::Unknown;
    }
    if !runner.has_command("curl") {
        return Update::Unknown;
    }
    let url = format!("https://api.github.com/repos/{RELEASE_REPO}/releases/latest");
    let out = runner.run(
        "curl",
        &[
            "-fsSL",
            "--max-time",
            "3",
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ],
    );
    if !out.success() {
        return Update::Unknown;
    }
    match parse_latest_tag(&out.stdout) {
        Some(tag) if is_newer(&tag, current) => Update::Available { latest: tag },
        Some(_) => Update::UpToDate,
        None => Update::Unknown,
    }
}

/// The one-line upgrade hint shown when an update is available.
pub fn upgrade_hint(latest: &str, current: &str) -> String {
    format!(
        "A newer ma-demo is available: {latest} (you have {current}). Upgrade:\n  \
         curl -fsSL https://github.com/{RELEASE_REPO}/releases/latest/download/install.sh | bash"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::MockRunner;

    #[test]
    fn parse_semver_handles_v_and_suffixes() {
        assert_eq!(parse_semver("v0.1.1"), (0, 1, 1));
        assert_eq!(parse_semver("0.1.0-dev"), (0, 1, 0));
        assert_eq!(parse_semver("1.2.3+build"), (1, 2, 3));
        assert_eq!(parse_semver("garbage"), (0, 0, 0));
    }

    #[test]
    fn is_newer_orders_semver() {
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        // A published 0.1.0 is not "newer" than a 0.1.0-dev build (same core).
        assert!(!is_newer("0.1.0", "0.1.0-dev"));
    }

    #[test]
    fn parse_latest_tag_extracts_tag() {
        let json = r#"{"url":"...","tag_name":"v0.1.1","name":"ma-demo v0.1.1"}"#;
        assert_eq!(parse_latest_tag(json).as_deref(), Some("v0.1.1"));
        assert_eq!(parse_latest_tag("{}"), None);
    }

    #[test]
    fn check_reports_available_when_release_is_newer() {
        let r = MockRunner::new().with_command("curl").stub(
            "curl",
            &["releases/latest"],
            0,
            r#"{"tag_name":"v0.2.0"}"#,
        );
        assert_eq!(
            check(&r, "0.1.1"),
            Update::Available {
                latest: "v0.2.0".into()
            }
        );
    }

    #[test]
    fn check_up_to_date_when_same_or_older() {
        let r = MockRunner::new().with_command("curl").stub(
            "curl",
            &["releases/latest"],
            0,
            r#"{"tag_name":"v0.1.1"}"#,
        );
        assert_eq!(check(&r, "0.1.1"), Update::UpToDate);
    }

    #[test]
    fn check_unknown_without_curl_or_on_error() {
        // No curl on PATH.
        assert_eq!(check(&MockRunner::new(), "0.1.1"), Update::Unknown);
        // curl present but errors (offline).
        let r = MockRunner::new().with_command("curl").stub_stderr(
            "curl",
            &["releases/latest"],
            6,
            "could not resolve host",
        );
        assert_eq!(check(&r, "0.1.1"), Update::Unknown);
    }

    #[test]
    fn check_respects_opt_out_env() {
        std::env::set_var("MA_DEMO_NO_UPDATE_CHECK", "1");
        let r = MockRunner::new().with_command("curl").stub(
            "curl",
            &["releases/latest"],
            0,
            r#"{"tag_name":"v9.9.9"}"#,
        );
        assert_eq!(check(&r, "0.1.1"), Update::Unknown);
        std::env::remove_var("MA_DEMO_NO_UPDATE_CHECK");
    }

    #[test]
    fn upgrade_hint_mentions_versions_and_installer() {
        let h = upgrade_hint("v0.2.0", "0.1.1");
        assert!(h.contains("v0.2.0"));
        assert!(h.contains("0.1.1"));
        assert!(h.contains("install.sh | bash"));
    }
}

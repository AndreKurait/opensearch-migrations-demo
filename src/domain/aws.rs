//! AWS credential + region helpers for the cloud / AOSS paths.
//!
//! Pure parsing (profile discovery from `~/.aws/config` + `credentials`, region
//! list) plus thin probes through the [`CommandRunner`] seam (`aws sts
//! get-caller-identity`). The wizard offers the discovered profiles + a region
//! list; the orchestrator exports the chosen `AWS_PROFILE`/`AWS_REGION` before
//! any `aws`/`terraform` call so both the harness probes and Terraform's
//! provider use them.

use crate::runner::CommandRunner;

/// A curated list of common AWS regions offered in the wizard (the operator can
/// always override with `--aws-region`). Kept short + ordered by typical use.
pub const COMMON_REGIONS: [&str; 8] = [
    "us-east-1",
    "us-east-2",
    "us-west-2",
    "eu-west-1",
    "eu-central-1",
    "ap-south-1",
    "ap-southeast-1",
    "ap-northeast-1",
];

/// Discover configured AWS profile names from `~/.aws/config` + `credentials`.
/// Returns `["default"]` when nothing is found, deduped + with `default` first.
pub fn list_profiles() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut found = Vec::new();
    for (file, prefixed) in [("config", true), ("credentials", false)] {
        let path = format!("{home}/.aws/{file}");
        if let Ok(text) = std::fs::read_to_string(&path) {
            for name in parse_profiles(&text, prefixed) {
                if !found.contains(&name) {
                    found.push(name);
                }
            }
        }
    }
    // Ensure `default` is present and first.
    if !found.iter().any(|p| p == "default") {
        found.insert(0, "default".to_string());
    } else {
        found.retain(|p| p != "default");
        found.insert(0, "default".to_string());
    }
    found
}

/// Parse profile names from an AWS config/credentials file body. In
/// `~/.aws/config`, sections are `[profile NAME]` (except `[default]`); in
/// `~/.aws/credentials` they're `[NAME]`. `prefixed` selects the config form.
fn parse_profiles(text: &str, prefixed: bool) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('[') || !line.ends_with(']') {
            continue;
        }
        let inner = &line[1..line.len() - 1];
        let name = if prefixed {
            if inner == "default" {
                "default"
            } else if let Some(rest) = inner.strip_prefix("profile ") {
                rest.trim()
            } else {
                // A bare [name] in config is unusual; accept it.
                inner.trim()
            }
        } else {
            inner.trim()
        };
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// The caller identity for a profile/region (`aws sts get-caller-identity`),
/// or `None` if the credentials are missing/expired. Used to show the operator
/// which account they're about to deploy into.
pub fn caller_identity<R: CommandRunner>(
    runner: &R,
    profile: &str,
    region: &str,
) -> Option<String> {
    let out = runner.run(
        "aws",
        &[
            "sts",
            "get-caller-identity",
            "--profile",
            profile,
            "--region",
            region,
            "--query",
            "Arn",
            "--output",
            "text",
        ],
    );
    let arn = out.trimmed();
    if out.success() && !arn.is_empty() && arn != "None" {
        Some(arn.to_string())
    } else {
        None
    }
}

/// The account id portion of an STS ARN (`arn:aws:...::<acct>:...`), if present.
pub fn account_of(arn: &str) -> Option<String> {
    arn.split(':')
        .nth(4)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_profiles() {
        let cfg = "[default]\nregion = us-east-1\n\n[profile dev]\nregion = us-west-2\n\n[profile prod-admin]\n";
        let p = parse_profiles(cfg, true);
        assert_eq!(p, vec!["default", "dev", "prod-admin"]);
    }

    #[test]
    fn parses_credentials_profiles() {
        let creds = "[default]\naws_access_key_id = x\n\n[bedrock]\naws_access_key_id = y\n";
        let p = parse_profiles(creds, false);
        assert_eq!(p, vec!["default", "bedrock"]);
    }

    #[test]
    fn account_of_extracts_id() {
        assert_eq!(
            account_of("arn:aws:sts::874041194807:assumed-role/Role/sess").as_deref(),
            Some("874041194807")
        );
        assert_eq!(account_of("not-an-arn"), None);
    }

    #[test]
    fn common_regions_includes_us_east_1() {
        assert!(COMMON_REGIONS.contains(&"us-east-1"));
    }
}

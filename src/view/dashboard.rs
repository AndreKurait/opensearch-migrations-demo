//! The live status dashboard — a real-time, auto-refreshing view of everything
//! the harness provisioned.
//!
//! Split the usual way: a pure [`Snapshot`] (plain data describing the current
//! state of each resource) + a [`probe`] that gathers a fresh `Snapshot` through
//! the [`CommandRunner`] seam, and a pure ratatui render that's a projection of
//! the snapshot. The interactive loop ([`crate::command::app`] drives it) ticks
//! every couple of seconds, re-probes, and redraws — so the panels update in
//! real time and the dashboard stays up until the operator quits.
//!
//! Because probing goes through the runner, the whole gather → render path is
//! asserted against a [`MockRunner`] + `TestBackend` with no Docker.

use crate::manifests;
use crate::model::Answers;
use crate::plan;
use crate::runner::CommandRunner;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, Padding, Paragraph, Widget, Wrap},
    Frame,
};

/// The health of one resource, with a glyph + color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Up / ready / reachable.
    Up,
    /// Provisioning / not yet ready.
    Pending,
    /// Down / unreachable / failed.
    Down,
    /// Not part of this plan (skipped).
    NotApplicable,
}

impl Health {
    pub fn glyph(self) -> &'static str {
        match self {
            Health::Up => "●",
            Health::Pending => "◐",
            Health::Down => "○",
            Health::NotApplicable => "·",
        }
    }
}

/// One row in a panel: a label, a health, and an optional detail string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub health: Health,
    pub detail: String,
}

impl Row {
    fn new(label: impl Into<String>, health: Health, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            health,
            detail: detail.into(),
        }
    }
}

/// A full snapshot of the provisioned environment at one instant. Pure data —
/// the render is a projection of this, and [`probe`] produces it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// KIND cluster rows (name → exists).
    pub clusters: Vec<Row>,
    /// Source cluster workload rows (pods).
    pub source_pods: Vec<Row>,
    /// Per-index document counts on the source.
    pub indices: Vec<Row>,
    /// Target rows (local OpenSearch pod or AOSS collection).
    pub target: Vec<Row>,
    /// Migration Assistant rows (console statefulset / collection group).
    pub ma: Vec<Row>,
    /// Probe tick — incremented each (slow, ~2s) resource re-probe.
    pub tick: u64,
    /// Spinner frame — bumped on each (fast, ~120ms) redraw, independent of the
    /// probe tick, so the loading indicator animates smoothly while resources
    /// are only re-queried every couple of seconds.
    pub frame: u64,
}

impl Snapshot {
    /// A one-line overall summary (counts of up / pending resources).
    pub fn summary(&self) -> String {
        let all: Vec<&Row> = self
            .clusters
            .iter()
            .chain(&self.source_pods)
            .chain(&self.target)
            .chain(&self.ma)
            .collect();
        let up = all.iter().filter(|r| r.health == Health::Up).count();
        let total = all
            .iter()
            .filter(|r| r.health != Health::NotApplicable)
            .count();
        let docs: u64 = self
            .indices
            .iter()
            .filter_map(|r| r.detail.split_whitespace().next())
            .filter_map(|n| n.parse::<u64>().ok())
            .sum();
        format!("{up}/{total} resources up · {docs} docs seeded")
    }
}

/// Gather a fresh [`Snapshot`] for `answers` through the runner. Dispatches on
/// the deploy target: a Cloud plan probes real AWS resources (no Docker), a
/// Local plan probes KIND. Each probe is a cheap read; failures degrade to
/// `Down`/`Pending` rather than erroring, so the dashboard keeps ticking even
/// mid-provision.
pub fn probe<R: CommandRunner>(runner: &R, answers: &Answers, tick: u64) -> Snapshot {
    if answers.target == Some(crate::model::Target::Cloud) {
        probe_cloud(runner, answers, tick)
    } else {
        probe_local(runner, answers, tick)
    }
}

/// Probe the LOCAL (KIND) topology: clusters, source/target pods, indices, MA.
fn probe_local<R: CommandRunner>(runner: &R, answers: &Answers, tick: u64) -> Snapshot {
    let existing = list_clusters(runner);
    let mut snap = Snapshot {
        tick,
        ..Default::default()
    };

    // ---- clusters ----
    let src = answers.source_cluster();
    snap.clusters
        .push(cluster_row("source cluster", &src, &existing));
    if answers.provisions_local_target() {
        let tgt = answers.target_cluster();
        snap.clusters
            .push(cluster_row("target cluster", &tgt, &existing));
    }
    if answers.deploys_ma_locally() {
        let ma = answers.ma_cluster();
        snap.clusters
            .push(cluster_row("MA cluster", &ma, &existing));
    }

    // ---- source workloads + indices (only if the source cluster exists) ----
    let src_ctx = kube_context(&src);
    if existing.iter().any(|c| c == &src) {
        snap.source_pods = pod_rows(runner, &src_ctx, manifests::NAMESPACE);
        if answers.source_is_http() {
            let host = plan::source_service_name(answers.source_engine.unwrap());
            for idx in manifests::SEED_INDICES {
                let count = index_count(runner, &src_ctx, host, idx);
                let health = match count {
                    Some(n) if n > 0 => Health::Up,
                    Some(_) => Health::Pending,
                    None => Health::Down,
                };
                let detail = count
                    .map(|n| format!("{n} docs"))
                    .unwrap_or_else(|| "—".into());
                snap.indices.push(Row::new(idx, health, detail));
            }
        }
    } else {
        snap.source_pods
            .push(Row::new("source", Health::Pending, "cluster not up yet"));
    }

    // ---- target ----
    if answers.provisions_local_target() {
        let tgt_ctx = kube_context(&answers.target_cluster());
        if existing.iter().any(|c| c == &answers.target_cluster()) {
            let pods = pod_rows(runner, &tgt_ctx, manifests::NAMESPACE);
            snap.target = if pods.is_empty() {
                vec![Row::new("target-opensearch", Health::Pending, "starting")]
            } else {
                pods
            };
        } else {
            snap.target.push(Row::new(
                "target-opensearch",
                Health::Pending,
                "cluster not up yet",
            ));
        }
    } else if answers.provisions_aoss_target() {
        let ep = aoss_status(runner, answers);
        snap.target.push(ep);
    } else {
        snap.target.push(Row::new(
            "target",
            Health::NotApplicable,
            "left to Migration Assistant",
        ));
    }

    // ---- Migration Assistant ----
    if answers.deploys_ma_locally() {
        let ma_ctx = kube_context(&answers.ma_cluster());
        if existing.iter().any(|c| c == &answers.ma_cluster()) {
            let console =
                statefulset_ready(runner, &ma_ctx, plan::MA_NAMESPACE, "migration-console");
            snap.ma
                .push(Row::new("migration-console", console.0, console.1));
        } else {
            snap.ma
                .push(Row::new("migration-console", Health::Pending, "deploying"));
        }
    } else {
        snap.ma.push(Row::new(
            "MA CLI",
            Health::NotApplicable,
            "installed to workspace bin/",
        ));
    }

    snap
}

/// Probe the CLOUD (AWS / Terraform) topology — there are NO Docker containers
/// here, so we query the real AWS resources the terraform creates: the EC2
/// source instance (by Name tag), the S3 snapshot bucket, and the target (an
/// AOSS NextGen collection or an OpenSearch Service domain). All reads go
/// through the `aws` CLI and degrade gracefully (Pending when not yet up /
/// credentials missing). The "clusters" panel is relabeled to the AWS account
/// context so a cloud run never shows phantom KIND clusters.
fn probe_cloud<R: CommandRunner>(runner: &R, answers: &Answers, tick: u64) -> Snapshot {
    let mut snap = Snapshot {
        tick,
        ..Default::default()
    };
    // The saved plan is the single source of truth for profile + region, so
    // `ma-demo status` probes the same account/region the plan deployed into
    // regardless of the ambient AWS_* env.
    let region = answers.effective_aws_region().to_string();
    let profile = answers.effective_aws_profile().to_string();

    // ---- account context (replaces the KIND-clusters panel) ----
    if !runner.has_command("aws") {
        snap.clusters
            .push(Row::new("AWS", Health::Down, "aws CLI not on PATH"));
    } else {
        match aws_caller_arn(runner, &profile, &region) {
            Some(arn) => {
                let acct = arn.split(':').nth(4).unwrap_or("?").to_string();
                snap.clusters.push(Row::new(
                    "AWS account",
                    Health::Up,
                    format!("{acct} · {region} · profile {profile}"),
                ));
            }
            None => snap.clusters.push(Row::new(
                "AWS account",
                Health::Down,
                format!("creds missing/expired (profile {profile})"),
            )),
        }
    }

    // ---- source: the EC2 instance terraform creates (tag Name=ma-demo-source) ----
    let (sh, sd) = ec2_status(runner, "ma-demo-source", &profile, &region);
    snap.source_pods.push(Row::new("source EC2", sh, sd));

    // ---- snapshot bucket (S3) ----
    if answers.snapshot_storage != Some(crate::model::SnapshotStorage::None) {
        let (bh, bd) = s3_bucket_status(runner, "ma-demo-snapshots", &profile, &region);
        snap.source_pods.push(Row::new("S3 snapshots", bh, bd));
    }

    // ---- target ----
    if answers.provisions_aoss_target() {
        snap.target.push(aoss_status(runner, answers));
    } else if answers.target_mode == Some(crate::model::TargetMode::Provision) {
        let (th, td) = opensearch_domain_status(runner, "ma-demo-target", &profile, &region);
        snap.target.push(Row::new("OpenSearch Service", th, td));
    } else {
        snap.target.push(Row::new(
            "target",
            Health::NotApplicable,
            "left to Migration Assistant",
        ));
    }

    // ---- MA: cloud always installs the EKS-targeting CLI ----
    snap.ma.push(Row::new(
        "MA CLI",
        Health::NotApplicable,
        "installed to workspace bin/ (deploys to EKS)",
    ));

    snap
}

// ---- probe helpers (each goes through the runner) ----

fn kube_context(cluster: &str) -> String {
    format!("kind-{cluster}")
}

/// The set of existing KIND cluster names (`kind get clusters`).
fn list_clusters<R: CommandRunner>(runner: &R) -> Vec<String> {
    let out = runner.run("kind", &["get", "clusters"]);
    if !out.success() {
        return Vec::new();
    }
    out.stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.contains("No kind clusters"))
        .collect()
}

fn cluster_row(label: &str, name: &str, existing: &[String]) -> Row {
    let health = if existing.iter().any(|c| c == name) {
        Health::Up
    } else {
        Health::Pending
    };
    Row::new(label, health, name.to_string())
}

/// Pod rows from `kubectl get pods -o ...`: one row per pod with Ready health.
/// Parses the wide `kubectl get pods --no-headers` form (`NAME READY STATUS …`).
fn pod_rows<R: CommandRunner>(runner: &R, ctx: &str, ns: &str) -> Vec<Row> {
    let out = runner.run(
        "kubectl",
        &["--context", ctx, "-n", ns, "get", "pods", "--no-headers"],
    );
    if !out.success() {
        return Vec::new();
    }
    out.stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let name = cols.first().copied().unwrap_or("?");
            let ready = cols.get(1).copied().unwrap_or("0/0");
            let status = cols.get(2).copied().unwrap_or("?");
            let health = pod_health(ready, status);
            Row::new(short_pod_name(name), health, format!("{status} {ready}"))
        })
        .collect()
}

/// Health from a pod's READY (`a/b`) + STATUS columns.
fn pod_health(ready: &str, status: &str) -> Health {
    if status == "Running" || status == "Completed" || status == "Succeeded" {
        // Ready iff a == b in "a/b".
        if let Some((a, b)) = ready.split_once('/') {
            if a == b && a != "0" {
                return Health::Up;
            }
        }
        if status == "Completed" || status == "Succeeded" {
            return Health::Up;
        }
        return Health::Pending;
    }
    if status.contains("Error") || status.contains("CrashLoop") || status.contains("Failed") {
        return Health::Down;
    }
    Health::Pending
}

/// Trim the random replica/job suffix off a pod name for display, keeping
/// statefulset ordinals (`-0`). Strips trailing kubernetes hash segments — the
/// `-<10char>-<5char>` of a Deployment ReplicaSet pod and the `-<5char>` of a
/// Job pod — but never a pure-numeric ordinal.
fn short_pod_name(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    // statefulset: <name>-<ordinal> — keep the ordinal.
    if parts
        .last()
        .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
    {
        return name.to_string();
    }
    // Otherwise drop trailing kubernetes-generated suffixes, leaving the base
    // name. A pod suffix is exactly 5 lowercase-alnum chars (may be all
    // letters, e.g. `fdwbh`); a ReplicaSet hash is 8–10 lowercase-alnum chars
    // and contains a digit (e.g. `7b4664d776`).
    let mut end = parts.len();
    while end > 1 {
        let seg = parts[end - 1];
        let alnum = !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let pod_suffix = seg.len() == 5;
        let rs_hash = (8..=10).contains(&seg.len()) && seg.chars().any(|c| c.is_ascii_digit());
        if alnum && (pod_suffix || rs_hash) {
            end -= 1;
        } else {
            break;
        }
    }
    parts[..end].join("-")
}

/// Per-index doc count via an in-cluster `kubectl exec curl _count`. Returns
/// `None` when unreachable.
fn index_count<R: CommandRunner>(runner: &R, ctx: &str, host: &str, idx: &str) -> Option<u64> {
    let url = format!("http://{host}:9200/{idx}/_count");
    let out = runner.run(
        "kubectl",
        &[
            "--context",
            ctx,
            "-n",
            manifests::NAMESPACE,
            "exec",
            &pod_for(host),
            "--",
            "curl",
            "-s",
            &url,
        ],
    );
    if !out.success() {
        return None;
    }
    parse_count(&out.stdout)
}

/// The pod name to `exec` into for a given source service host.
fn pod_for(host: &str) -> String {
    // Source services are backed by a statefulset pod `<service>-0` (ES/OS) or a
    // deployment for solr; the `-0` form covers the http engines we count.
    format!("{host}-0")
}

/// Parse `{"count":N,...}` → N.
fn parse_count(json: &str) -> Option<u64> {
    let key = "\"count\":";
    let i = json.find(key)? + key.len();
    let rest = &json[i..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ---- cloud (AWS) probe helpers ----

/// The caller ARN for a profile/region (`aws sts get-caller-identity`), or
/// `None` when credentials are missing/expired.
fn aws_caller_arn<R: CommandRunner>(runner: &R, profile: &str, region: &str) -> Option<String> {
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
    let arn = out.trimmed_stdout().trim();
    (out.success() && !arn.is_empty() && arn != "None").then(|| arn.to_string())
}

/// Status of the EC2 source instance (tagged `Name=<name>`): its instance-state
/// → health, plus a short detail. Pending when no matching instance exists yet.
fn ec2_status<R: CommandRunner>(
    runner: &R,
    name: &str,
    profile: &str,
    region: &str,
) -> (Health, String) {
    let out = runner.run(
        "aws",
        &[
            "ec2",
            "describe-instances",
            "--profile",
            profile,
            "--region",
            region,
            "--filters",
            &format!("Name=tag:Name,Values={name}"),
            "Name=instance-state-name,Values=pending,running,stopping,stopped",
            "--query",
            "Reservations[].Instances[].State.Name",
            "--output",
            "text",
        ],
    );
    if !out.success() {
        return (Health::Pending, "querying".into());
    }
    match out.trimmed_stdout().split_whitespace().next() {
        Some("running") => (Health::Up, "running".into()),
        Some("pending") => (Health::Pending, "launching".into()),
        Some(other) => (Health::Pending, other.to_string()),
        None => (Health::Pending, "not created yet".into()),
    }
}

/// Status of the S3 snapshot bucket (prefix match on `<prefix>`). Up when a
/// bucket with that prefix exists.
fn s3_bucket_status<R: CommandRunner>(
    runner: &R,
    prefix: &str,
    profile: &str,
    _region: &str,
) -> (Health, String) {
    let out = runner.run(
        "aws",
        &[
            "s3api",
            "list-buckets",
            "--profile",
            profile,
            "--query",
            &format!("Buckets[?starts_with(Name, `{prefix}`)].Name | [0]"),
            "--output",
            "text",
        ],
    );
    if !out.success() {
        return (Health::Pending, "querying".into());
    }
    let name = out.trimmed_stdout().trim();
    if name.is_empty() || name == "None" {
        (Health::Pending, "not created yet".into())
    } else {
        (Health::Up, name.to_string())
    }
}

/// Status of an Amazon OpenSearch Service domain (by name): Active processing
/// → health + endpoint.
fn opensearch_domain_status<R: CommandRunner>(
    runner: &R,
    name: &str,
    profile: &str,
    region: &str,
) -> (Health, String) {
    let out = runner.run(
        "aws",
        &[
            "opensearch",
            "describe-domain",
            "--profile",
            profile,
            "--region",
            region,
            "--domain-name",
            name,
            "--query",
            "DomainStatus.{processing:Processing,endpoint:Endpoint,created:Created}",
            "--output",
            "text",
        ],
    );
    if !out.success() {
        return (Health::Pending, "not created yet".into());
    }
    let s = out.trimmed_stdout();
    // `Created Endpoint Processing` columns; Processing=False means ready.
    if s.contains("False") {
        let ep = s
            .split_whitespace()
            .find(|t| t.contains('.'))
            .unwrap_or("active");
        (Health::Up, ep.to_string())
    } else {
        (Health::Pending, "processing".into())
    }
}

/// AOSS collection status row (ACTIVE + endpoint, via batch-get-collection).
fn aoss_status<R: CommandRunner>(runner: &R, answers: &Answers) -> Row {
    // Region from the saved plan (single source of truth), not the ambient env.
    let region = answers.effective_aws_region();
    let args = plan::aoss_batch_get_args(&answers.aoss_collection_name(), region);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = runner.run("aws", &argv);
    if !out.success() {
        return Row::new("AOSS collection", Health::Pending, "querying");
    }
    if let Some(ep) = crate::app::parse_aoss_endpoint(&out.stdout) {
        Row::new("AOSS NextGen", Health::Up, ep)
    } else {
        Row::new("AOSS NextGen", Health::Pending, "CREATING")
    }
}

/// Statefulset readiness via `kubectl get statefulset -o jsonpath` readyReplicas.
fn statefulset_ready<R: CommandRunner>(
    runner: &R,
    ctx: &str,
    ns: &str,
    name: &str,
) -> (Health, String) {
    let out = runner.run(
        "kubectl",
        &[
            "--context",
            ctx,
            "-n",
            ns,
            "get",
            "statefulset",
            name,
            "-o",
            "jsonpath={.status.readyReplicas}/{.status.replicas}",
        ],
    );
    if !out.success() {
        return (Health::Pending, "deploying".into());
    }
    let s = out.trimmed_stdout().trim().to_string();
    if let Some((a, b)) = s.split_once('/') {
        if a == b && a != "0" && !a.is_empty() {
            return (Health::Up, format!("ready {s}"));
        }
        return (Health::Pending, format!("{s} ready"));
    }
    (Health::Pending, "deploying".into())
}

// ---------------------------------------------------------------------------
// Render — a pure projection of the Snapshot.
// ---------------------------------------------------------------------------

/// Render the dashboard into `frame` for `snap`. `answers` provides the plan
/// summary header.
pub fn render(frame: &mut Frame, snap: &Snapshot, answers: &Answers) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header, snap, answers);

    let cloud = answers.target == Some(crate::model::Target::Cloud);
    // Target-aware panel labels: a cloud run has no KIND clusters or in-cluster
    // index counts — it shows the AWS account + resources instead.
    let (clusters_title, source_title) = if cloud {
        (" AWS ", " AWS resources ")
    } else {
        (" KIND clusters ", " Source workloads ")
    };

    // Two columns: left = clusters/AWS + source; right = target + MA. The
    // seeded-indices panel only applies to the local (in-cluster) path.
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
    if cloud {
        let [clusters_a, source_a] = Layout::vertical([
            Constraint::Length(snap.clusters.len() as u16 + 2),
            Constraint::Min(4),
        ])
        .areas(left);
        panel(frame, clusters_a, clusters_title, &snap.clusters);
        panel(frame, source_a, source_title, &snap.source_pods);
    } else {
        let [clusters_a, source_a, indices_a] = Layout::vertical([
            Constraint::Length(snap.clusters.len() as u16 + 2),
            Constraint::Min(4),
            Constraint::Length(manifests::SEED_INDICES.len() as u16 + 2),
        ])
        .areas(left);
        panel(frame, clusters_a, clusters_title, &snap.clusters);
        panel(frame, source_a, source_title, &snap.source_pods);
        panel(frame, indices_a, " Seeded indices ", &snap.indices);
    }
    let [target_a, ma_a] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(right);
    panel(frame, target_a, " Target ", &snap.target);
    panel(frame, ma_a, " Migration Assistant ", &snap.ma);

    let hint = "live · refreshing every 2s · press q or Esc to exit";
    Paragraph::new(hint.dim()).render(footer, frame.buffer_mut());
}

fn render_header(frame: &mut Frame, area: Rect, snap: &Snapshot, answers: &Answers) {
    let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    // Driven by the fast frame counter (not the slow probe tick) so it spins
    // smoothly rather than stepping once per 2s re-probe.
    let s = spinner[(snap.frame as usize) % spinner.len()];
    let title = Line::from(vec![
        Span::from(format!(" {s} ")).bold().on_blue(),
        Span::from(" Migration Assistant — Demo Environment ").bold(),
    ]);
    let sub = Line::from(vec![
        Span::raw("  "),
        Span::raw(answers.summary()).dim(),
        Span::raw("   "),
        Span::from(snap.summary()).green(),
    ]);
    let block = Block::bordered();
    let inner = block.inner(area);
    block.render(area, frame.buffer_mut());
    Paragraph::new(vec![title, sub]).render(inner, frame.buffer_mut());
}

/// Render one bordered panel of rows.
fn panel(frame: &mut Frame, area: Rect, title: &str, rows: &[Row]) {
    let lines: Vec<Line> = if rows.is_empty() {
        vec![Line::from("  (none)".dim())]
    } else {
        rows.iter()
            .map(|r| {
                let glyph = Span::from(format!(" {} ", r.health.glyph()));
                let glyph = match r.health {
                    Health::Up => glyph.green(),
                    Health::Pending => glyph.yellow(),
                    Health::Down => glyph.red(),
                    Health::NotApplicable => glyph.dim(),
                };
                Line::from(vec![
                    glyph,
                    Span::from(format!("{:<22}", truncate(&r.label, 22))),
                    Span::from(r.detail.clone()).dim(),
                ])
            })
            .collect()
    };
    let block = Block::bordered()
        .title(title.bold())
        .padding(Padding::horizontal(1));
    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: true })
        .render(area, frame.buffer_mut());
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

// ---------------------------------------------------------------------------
// Interactive loop — re-probe on a tick, redraw, stay up until the user quits.
// ---------------------------------------------------------------------------

/// Whether a key should quit the dashboard (q / Esc). Pure, so the binding is
/// tested without a terminal. Ctrl-C also quits, but — because raw mode does not
/// deliver it as SIGINT — it's matched on the full key event in the loop via
/// [`crate::view::tui::is_ctrl_c`], not here (this takes only the keycode).
pub fn key_quits(code: ratatui::crossterm::event::KeyCode) -> bool {
    use ratatui::crossterm::event::KeyCode::*;
    matches!(code, Char('q') | Char('Q') | Esc)
}

/// Run the live dashboard until the user quits. Re-probes through `runner` every
/// `tick` and redraws; polls input frequently so q/Esc are responsive. Sets up
/// the terminal and always restores it. This is the only IO here — the snapshot
/// it renders is gathered by [`probe`] and the view is a pure projection.
pub fn run_live<R: CommandRunner>(
    runner: &R,
    answers: &Answers,
    tick: std::time::Duration,
) -> std::io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let result = run_loop(&mut terminal, runner, answers, tick);
    ratatui::restore();
    result
}

fn run_loop<R: CommandRunner>(
    terminal: &mut ratatui::DefaultTerminal,
    runner: &R,
    answers: &Answers,
    tick: std::time::Duration,
) -> std::io::Result<()> {
    use ratatui::crossterm::event::{self, Event};
    let mut counter: u64 = 0;
    let mut frame: u64 = 0;
    let mut snap = probe(runner, answers, counter);
    let mut last = std::time::Instant::now();
    // Fast redraw cadence drives the spinner; resources re-probe every `tick`.
    let spin = std::time::Duration::from_millis(120);
    loop {
        snap.frame = frame;
        terminal.draw(|f| render(f, &snap, answers))?;
        // Poll input on the spinner interval so the animation stays smooth AND
        // quitting is snappy, independent of the (slower) resource re-probe.
        if event::poll(spin)? {
            if let Event::Key(key) = event::read()? {
                if key.is_press() && (key_quits(key.code) || crate::view::tui::is_ctrl_c(&key)) {
                    return Ok(());
                }
            }
        }
        frame = frame.wrapping_add(1);
        if last.elapsed() >= tick {
            counter = counter.wrapping_add(1);
            snap = probe(runner, answers, counter);
            last = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Answers, SourceEngine, Target, TargetMode};
    use crate::runner::MockRunner;
    use ratatui::{backend::TestBackend, Terminal};

    fn local_answers() -> Answers {
        let mut a = Answers::new();
        a.target = Some(Target::Local);
        a.source_engine = Some(SourceEngine::OpenSearch);
        a.source_version = Some("2.15.0".into());
        a.target_mode = Some(TargetMode::Provision);
        a.target_kind = Some(crate::model::TargetKind::KindOpenSearch);
        a.target_version = Some("3.3.0".into());
        a
    }

    fn cloud_answers() -> Answers {
        let mut a = Answers::new();
        a.target = Some(Target::Cloud);
        a.source_engine = Some(SourceEngine::Solr);
        a.source_version = Some("8.11.3".into());
        a.snapshot_storage = Some(crate::model::SnapshotStorage::AwsS3);
        a.target_mode = Some(TargetMode::Provision);
        a.target_kind = Some(crate::model::TargetKind::AossServerlessNextGen);
        a.aws_profile = Some("default".into());
        a.aws_region = Some("us-east-1".into());
        a
    }

    #[test]
    fn cloud_probe_shows_aws_resources_not_kind() {
        let r = MockRunner::new()
            .with_command("aws")
            .stub(
                "aws",
                &["sts", "get-caller-identity"],
                0,
                "arn:aws:sts::874041194807:assumed-role/Role/sess",
            )
            .stub("aws", &["ec2", "describe-instances"], 0, "running")
            .stub("aws", &["s3api", "list-buckets"], 0, "ma-demo-snapshots-abc123")
            .stub(
                "aws",
                &["batch-get-collection"],
                0,
                r#"{"collectionDetails":[{"status":"ACTIVE","collectionEndpoint":"https://x.aoss.us-east-1.on.aws","id":"x"}]}"#,
            );
        let snap = probe(&r, &cloud_answers(), 0);
        // NO `kind get clusters` on the cloud path.
        assert!(!r.any_call_contains("kind get clusters"));
        // AWS account context shown (account id parsed from the ARN).
        assert!(snap.clusters.iter().any(|c| c.label == "AWS account"
            && c.health == Health::Up
            && c.detail.contains("874041194807")));
        // Real AWS resources, not phantom KIND pods.
        assert!(snap
            .source_pods
            .iter()
            .any(|r| r.label == "source EC2" && r.health == Health::Up));
        assert!(snap
            .source_pods
            .iter()
            .any(|r| r.label == "S3 snapshots" && r.health == Health::Up));
        assert!(snap
            .target
            .iter()
            .any(|t| t.label == "AOSS NextGen" && t.health == Health::Up));
        // The misleading "cluster not up yet" placeholder must NOT appear.
        assert!(!snap
            .source_pods
            .iter()
            .any(|r| r.detail.contains("cluster not up yet")));
    }

    #[test]
    fn cloud_probe_degrades_when_creds_missing() {
        // aws present but sts fails (expired creds) → account row Down, no panic.
        let r =
            MockRunner::new()
                .with_command("aws")
                .stub_stderr("aws", &["sts"], 255, "ExpiredToken");
        let snap = probe(&r, &cloud_answers(), 0);
        assert!(snap
            .clusters
            .iter()
            .any(|c| c.label == "AWS account" && c.health == Health::Down));
    }

    #[test]
    fn cloud_render_labels_panels_for_aws() {
        let r = MockRunner::new().with_command("aws");
        let snap = probe(&r, &cloud_answers(), 0);
        let mut t = Terminal::new(TestBackend::new(110, 24)).unwrap();
        t.draw(|f| render(f, &snap, &cloud_answers())).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(
            text.contains("AWS resources"),
            "cloud run should label the panel 'AWS resources'"
        );
        assert!(
            !text.contains("KIND clusters"),
            "cloud run must not show a KIND panel"
        );
        assert!(
            !text.contains("Seeded indices"),
            "no in-cluster index panel on cloud"
        );
    }

    #[test]
    fn spinner_frame_is_independent_of_probe_tick() {
        // The header spinner is driven by `frame`, not `tick`, so it animates on
        // fast redraws while the probe tick only steps every ~2s.
        let r = MockRunner::new();
        let mut snap = probe(&r, &local_answers(), 0);
        // The header is inside a bordered block, so the spinner sits on the
        // inner row (y=1), not the border row (y=0).
        let glyph_at = |s: &Snapshot| {
            let mut t = Terminal::new(TestBackend::new(100, 24)).unwrap();
            t.draw(|f| render(f, s, &local_answers())).unwrap();
            let buf = t.backend().buffer().clone();
            (0..buf.area().width)
                .map(|x| buf[(x, 1)].symbol().to_string())
                .collect::<String>()
        };
        snap.frame = 0;
        let g0 = glyph_at(&snap);
        snap.frame = 3; // advance spinner WITHOUT re-probing (tick unchanged)
        let g3 = glyph_at(&snap);
        assert_ne!(g0, g3, "spinner glyph must change as frame advances");
    }

    #[test]
    fn parse_count_extracts_number() {
        assert_eq!(parse_count(r#"{"count":500,"_shards":{}}"#), Some(500));
        assert_eq!(parse_count("nope"), None);
    }

    #[test]
    fn key_quits_on_q_and_esc_only() {
        use ratatui::crossterm::event::KeyCode;
        assert!(key_quits(KeyCode::Char('q')));
        assert!(key_quits(KeyCode::Char('Q')));
        assert!(key_quits(KeyCode::Esc));
        assert!(!key_quits(KeyCode::Char('x')));
        assert!(!key_quits(KeyCode::Down));
    }

    #[test]
    fn pod_health_classifies() {
        assert_eq!(pod_health("1/1", "Running"), Health::Up);
        assert_eq!(pod_health("0/1", "Running"), Health::Pending);
        assert_eq!(pod_health("0/1", "ContainerCreating"), Health::Pending);
        assert_eq!(pod_health("0/1", "CrashLoopBackOff"), Health::Down);
        assert_eq!(pod_health("0/1", "Completed"), Health::Up);
    }

    #[test]
    fn short_pod_name_keeps_ordinals_trims_hashes() {
        // statefulset ordinal kept
        assert_eq!(short_pod_name("source-opensearch-0"), "source-opensearch-0");
        assert_eq!(short_pod_name("target-opensearch-0"), "target-opensearch-0");
        // deployment ReplicaSet pod → base name (incl. an all-letter pod suffix)
        assert_eq!(short_pod_name("locust-6c86b77b45-td7fx"), "locust");
        assert_eq!(short_pod_name("locust-7b4664d776-fdwbh"), "locust");
        assert_eq!(
            short_pod_name("sample-search-app-66954fd9cb-9htv6"),
            "sample-search-app"
        );
        // Job pod (single hash suffix) → base name
        assert_eq!(short_pod_name("data-seed-wd795"), "data-seed");
        // base names whose trailing word is <5 chars are preserved
        assert_eq!(short_pod_name("data-seed"), "data-seed");
    }

    #[test]
    fn probe_pending_when_no_clusters() {
        // kind get clusters returns nothing → everything pending.
        let r = MockRunner::new();
        let snap = probe(&r, &local_answers(), 0);
        assert!(snap.clusters.iter().all(|c| c.health == Health::Pending));
        // No source cluster → source pods row is the "not up yet" placeholder.
        assert!(snap
            .source_pods
            .iter()
            .any(|r| r.detail.contains("not up yet")));
    }

    #[test]
    fn probe_reads_pods_and_counts_when_source_up() {
        let r = MockRunner::new()
            .stub("kind", &["get", "clusters"], 0, "ma-demo-source\nma-demo-target")
            .stub(
                "kubectl",
                &["kind-ma-demo-source", "get", "pods"],
                0,
                "source-opensearch-0   1/1   Running   0   2m\nlocust-6c86b77b45-td7fx   1/1   Running   0   2m",
            )
            .stub(
                "kubectl",
                &["kind-ma-demo-target", "get", "pods"],
                0,
                "target-opensearch-0   1/1   Running   0   1m",
            )
            // index counts
            .stub("kubectl", &["demo-logs/_count"], 0, r#"{"count":500}"#)
            .stub("kubectl", &["demo-users/_count"], 0, r#"{"count":200}"#)
            .stub("kubectl", &["demo-products/_count"], 0, r#"{"count":150}"#);
        let snap = probe(&r, &local_answers(), 1);
        // Both clusters up.
        assert!(snap.clusters.iter().all(|c| c.health == Health::Up));
        // Source pods parsed + healthy.
        assert!(snap
            .source_pods
            .iter()
            .any(|p| p.label == "source-opensearch-0" && p.health == Health::Up));
        assert!(snap.source_pods.iter().any(|p| p.label == "locust"));
        // Indices counted.
        let logs = snap
            .indices
            .iter()
            .find(|r| r.label == "demo-logs")
            .unwrap();
        assert_eq!(logs.health, Health::Up);
        assert!(logs.detail.contains("500"));
        // Target pod healthy.
        assert!(snap
            .target
            .iter()
            .any(|p| p.label == "target-opensearch-0" && p.health == Health::Up));
        // Summary mentions docs.
        assert!(snap.summary().contains("850 docs"));
    }

    #[test]
    fn probe_aoss_target_uses_batch_get() {
        let mut a = local_answers();
        a.target_kind = Some(crate::model::TargetKind::AossServerlessNextGen);
        a.target_version = None;
        let r = MockRunner::new()
            .stub("kind", &["get", "clusters"], 0, "ma-demo-source")
            .stub(
                "aws",
                &["batch-get-collection"],
                0,
                r#"{"collectionDetails":[{"status":"ACTIVE","collectionEndpoint":"https://x.aoss.us-east-1.on.aws","id":"x"}]}"#,
            );
        let snap = probe(&r, &a, 0);
        assert!(snap
            .target
            .iter()
            .any(|t| t.label == "AOSS NextGen" && t.health == Health::Up));
        // No second KIND cluster row for AOSS.
        assert!(!snap.clusters.iter().any(|c| c.detail == "ma-demo-target"));
    }

    #[test]
    fn renders_a_full_frame_without_panicking() {
        let r = MockRunner::new()
            .stub("kind", &["get", "clusters"], 0, "ma-demo-source")
            .stub(
                "kubectl",
                &["get", "pods"],
                0,
                "source-opensearch-0   1/1   Running   0   2m",
            )
            .stub("kubectl", &["_count"], 0, r#"{"count":100}"#);
        let snap = probe(&r, &local_answers(), 3);
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| render(f, &snap, &local_answers())).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(text.contains("KIND clusters"));
        assert!(text.contains("Source workloads"));
        assert!(text.contains("Seeded indices"));
        assert!(text.contains("Migration Assistant"));
    }
}

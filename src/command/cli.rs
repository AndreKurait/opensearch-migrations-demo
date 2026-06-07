//! Command-line surface + dispatcher.
//!
//! The default invocation runs the wizard (interactive Ratatui, or
//! non-interactive defaults with `-y`), provisions the environment, then
//! installs + launches the Migration Assistant CLI. Other subcommands inspect
//! or reset. The exit-code contract mirrors the migration-assistant CLI:
//! unknown command → 64, `die` → 1, success → 0.

use crate::app::{App, Progress};
use crate::error::{Error, Result};
use crate::model::Answers;
use crate::plan::ProvisionPlan;
use crate::runner::CommandRunner;
use crate::state::State;
use crate::tui::Outcome;
use crate::wizard::{self, QuestionId};
use crate::{terraform, ui};
use std::path::PathBuf;

/// The harness version, stamped from build.rs (CLI_VERSION) or a dev default.
pub const VERSION: &str = match option_env!("DEMO_VERSION") {
    Some(v) => v,
    None => "0.1.0-dev",
};

/// How the wizard collects answers. The real binary uses the Ratatui TUI; tests
/// drive a scripted collector so the whole dispatch is assertable.
pub trait Wizardish {
    /// Present `question`, returning the chosen [`Outcome`]. `preselect` seeds
    /// multi-choice (resume).
    fn ask(&self, question: &wizard::Question, preselect: &[String]) -> Result<Outcome>;
}

/// The interactive Ratatui wizard.
pub struct TuiWizard;
impl Wizardish for TuiWizard {
    fn ask(&self, question: &wizard::Question, preselect: &[String]) -> Result<Outcome> {
        crate::tui::run(question.clone(), preselect).map_err(Error::from)
    }
}

/// A progress sink that prints via `ui`.
pub struct UiProgress;
impl Progress for UiProgress {
    fn step(&self, msg: &str) {
        ui::step(msg);
    }
    fn info(&self, msg: &str) {
        ui::dim(&format!("  {msg}"));
    }
}

/// Parsed top-level flags shared by the run path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub non_interactive: bool,
    pub dry_run: bool,
    pub workspace: Option<String>,
}

/// Parse flags out of the argument list (order-independent).
pub fn parse_flags(args: &[String]) -> Flags {
    let mut f = Flags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-y" | "--non-interactive" | "--yes" => f.non_interactive = true,
            "--dry-run" | "--plan" => f.dry_run = true,
            "--workspace" => {
                f.workspace = args.get(i + 1).cloned();
                i += 1;
            }
            s => {
                if let Some(v) = s.strip_prefix("--workspace=") {
                    f.workspace = Some(v.to_string());
                }
            }
        }
        i += 1;
    }
    f
}

/// The default workspace directory (one removable folder under CWD).
pub fn default_workspace() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join("migration-demo-workspace")
}

/// Parse argv (program name dropped) and dispatch. Returns the process exit
/// code. `runner` is the external-command backend (real in `main`, mock in
/// tests); `wiz` collects wizard answers.
pub fn dispatch<R: CommandRunner, W: Wizardish>(args: &[String], runner: &R, wiz: &W) -> i32 {
    match run(args, runner, wiz) {
        Ok(()) => 0,
        Err(e) => {
            ui::err(&e.message);
            e.code
        }
    }
}

fn run<R: CommandRunner, W: Wizardish>(args: &[String], runner: &R, wiz: &W) -> Result<()> {
    let (sub, rest) = split_subcommand(args);
    match sub.as_deref() {
        None => cmd_run(runner, wiz, &rest, /*force_dry=*/ false),
        Some("run") => cmd_run(runner, wiz, &rest, false),
        // `plan` collects answers + prints the plan but provisions nothing.
        Some("plan") => cmd_run(runner, wiz, &rest, true),
        Some("clear") => cmd_clear(&rest),
        Some("destroy") => cmd_destroy(runner, &rest),
        Some("version") | Some("--version") | Some("-V") => {
            println!("{VERSION}");
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            print!("{}", help_text());
            Ok(())
        }
        // A flag-led invocation behaves like the default run.
        Some(s) if s.starts_with('-') => {
            let mut full = vec![s.to_string()];
            full.extend_from_slice(&rest);
            cmd_run(runner, wiz, &full, false)
        }
        Some(_unknown) => {
            print!("{}", help_text());
            Err(Error::with_code("unknown command", 64))
        }
    }
}

/// Split argv into `(subcommand, rest)`. The subcommand is the first non-flag
/// token; a leading flag means the default run.
fn split_subcommand(args: &[String]) -> (Option<String>, Vec<String>) {
    if args.is_empty() {
        return (None, Vec::new());
    }
    (Some(args[0].clone()), args[1..].to_vec())
}

/// The main run path: collect answers, provision, then install + launch the MA
/// CLI. With `--dry-run` (or the `plan` subcommand, via `force_dry`) it stops
/// after printing the plan, provisioning nothing.
fn cmd_run<R: CommandRunner, W: Wizardish>(
    runner: &R,
    wiz: &W,
    args: &[String],
    force_dry: bool,
) -> Result<()> {
    let mut flags = parse_flags(args);
    flags.dry_run = flags.dry_run || force_dry;
    let workspace = flags
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);

    ui::banner("Migration Assistant — Demo Environment Setup");
    ui::dim(&format!(
        "  version={VERSION}  workspace={}",
        workspace.display()
    ));

    // Load any saved plan (resume), then collect remaining answers.
    let mut state = State::new(&workspace);
    state.load()?;
    let mut answers = state.plan.answers.clone();

    collect_answers(wiz, &mut answers, flags.non_interactive)?;
    state.plan.answers = answers.clone();
    state.save()?;

    ui::step("Plan");
    print_plan(&answers);

    if flags.dry_run {
        ui::ok("dry run — no resources created");
        return Ok(());
    }

    // Provision through the orchestrator.
    let progress = UiProgress;
    let mut app = App::new(runner, &workspace, &progress);
    app.state.plan.answers = answers.clone();
    app.preflight(&answers)?;
    let plan = app.provision(&answers)?;
    print_endpoints(&plan, &answers);

    // Install the Migration Assistant CLI and hand off.
    let argv = app.install_ma()?;
    ui::ok("Environment ready. Launching the Migration Assistant…");
    ui::dim(&format!("  $ {}", argv.join(" ")));
    launch_ma(runner, &argv, flags.non_interactive)
}

/// Drive the wizard until [`QuestionId::Review`], applying each answer. On
/// non-interactive, fills defaults and returns immediately. Cancelling the TUI
/// aborts the run with a clean message.
fn collect_answers<W: Wizardish>(
    wiz: &W,
    answers: &mut Answers,
    non_interactive: bool,
) -> Result<()> {
    if non_interactive {
        wizard::fill_defaults(answers);
        return Ok(());
    }
    loop {
        let id = wizard::next_question_id(answers);
        if id == QuestionId::Review {
            return Ok(());
        }
        let q = wizard::build(id, answers);
        let preselect = preselect_for(id, answers);
        match wiz.ask(&q, &preselect)? {
            Outcome::Answered(ans) => {
                wizard::apply(answers, id, ans);
            }
            Outcome::Cancelled => {
                return Err(Error::die("setup cancelled by user"));
            }
        }
    }
}

/// The currently-selected ids to pre-check for a multi-choice question on
/// resume (so re-entering the wizard keeps prior selections).
fn preselect_for(id: QuestionId, a: &Answers) -> Vec<String> {
    match id {
        QuestionId::SourcePlugins => a.source_plugins.clone(),
        QuestionId::Clients => a.clients.iter().map(|c| c.id().to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Exec (or, non-interactively, just report) the Migration Assistant launch.
/// In an interactive terminal the harness replaces itself with the MA CLI; in
/// non-interactive/CI we don't exec (no TTY) and just confirm readiness.
fn launch_ma<R: CommandRunner>(runner: &R, argv: &[String], non_interactive: bool) -> Result<()> {
    if non_interactive {
        ui::dim("  non-interactive: not launching the TUI; run the command above to start.");
        return Ok(());
    }
    // Run the MA CLI through the seam. RealRunner spawns it inheriting the
    // terminal; the harness returns its exit status as success/failure.
    let prog = &argv[0];
    let rest: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let out = runner.run(prog, &rest);
    if out.success() {
        Ok(())
    } else {
        Err(Error::with_code(
            "Migration Assistant exited non-zero",
            out.status.max(1),
        ))
    }
}

/// `clear` — wipe the local workspace (no Docker/cloud changes).
fn cmd_clear(args: &[String]) -> Result<()> {
    let flags = parse_flags(args);
    let workspace = flags
        .workspace
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);
    ui::banner(&format!("Clear workspace: {}", workspace.display()));
    ui::dim("  this does NOT delete KIND clusters or cloud resources");
    if !workspace.exists() {
        ui::info("nothing to clear");
        return Ok(());
    }
    std::fs::remove_dir_all(&workspace)?;
    ui::ok("workspace cleared");
    Ok(())
}

/// `destroy` — delete the harness's local KIND clusters (source + target) and
/// then wipe the workspace. The local counterpart to MA's own `cleanup`; for
/// the cloud path the operator runs `terraform destroy` on the emitted module.
fn cmd_destroy<R: CommandRunner>(runner: &R, args: &[String]) -> Result<()> {
    let flags = parse_flags(args);
    let workspace = flags
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);
    let answers = Answers::new();
    ui::banner("Destroy local demo environment");

    if !runner.has_command("kind") {
        ui::warn("kind not on PATH; skipping cluster deletion");
    } else {
        for cluster in [answers.source_cluster(), answers.target_cluster()] {
            ui::step(&format!("kind delete cluster {cluster}"));
            // Idempotent: kind exits 0 (with a notice) when the cluster is absent.
            let out = runner.run("kind", &["delete", "cluster", "--name", &cluster]);
            if out.success() {
                ui::ok(&format!("{cluster} deleted"));
            } else {
                ui::warn(&format!(
                    "could not delete {cluster}: {}",
                    out.stderr.trim()
                ));
            }
        }
    }

    if workspace.exists() {
        std::fs::remove_dir_all(&workspace)?;
        ui::ok("workspace cleared");
    }
    ui::dim("  (cloud) for an AWS deploy, run `terraform destroy` in terraform/");
    Ok(())
}

/// Print the human plan summary (the review screen's text form).
pub fn print_plan(a: &Answers) {
    ui::info(&format!("  {}", a.summary()));
    if let Some(t) = a.target {
        ui::dim(&format!("  target          {}", t.label()));
    }
    if let Some(e) = a.source_engine {
        let v = a.source_version.as_deref().unwrap_or("—");
        ui::dim(&format!("  source          {} {v}", e.label()));
    }
    if !a.source_plugins.is_empty() {
        ui::dim(&format!(
            "  plugins         {}",
            a.source_plugins.join(", ")
        ));
    }
    if let Some(s) = a.snapshot_storage {
        ui::dim(&format!("  snapshots       {}", s.label()));
    }
    if let Some(m) = a.target_mode {
        let v = a.target_version.as_deref().unwrap_or("");
        ui::dim(&format!("  target cluster  {} {v}", m.label()));
    }
    if !a.clients.is_empty() {
        let names: Vec<&str> = a.clients.iter().map(|c| c.label()).collect();
        ui::dim(&format!("  clients         {}", names.join(", ")));
    }
    ui::dim(&format!(
        "  seed data       {}",
        if a.seed_data == Some(true) {
            "yes"
        } else {
            "no"
        }
    ));
}

/// Print the resolved endpoints after provisioning (so the operator + MA know
/// where things live).
fn print_endpoints(plan: &ProvisionPlan, answers: &Answers) {
    ui::step("Provisioned endpoints");
    ui::dim(&format!(
        "  source   http://{}:9200  (in-cluster) / http://localhost:19200 (host)",
        plan.source_host
    ));
    if let Some(t) = &plan.target_host {
        ui::dim(&format!(
            "  target   http://{t}:9200  (in-cluster) / http://localhost:29200 (host)"
        ));
    }
    if answers.target == Some(crate::model::Target::Cloud) {
        ui::dim("  (cloud) see terraform/ outputs after `terraform apply`");
        let _ = terraform::files(answers); // touch to keep the path exercised
    }
}

/// The help text (stdout).
pub fn help_text() -> String {
    format!(
        "ma-demo — OpenSearch Migration Assistant demo environment setup\n\n\
Usage:\n\
\x20 ma-demo [flags]            Run the wizard, provision, launch MA (default)\n\
\x20 ma-demo run [flags]        Same as default\n\
\x20 ma-demo plan [flags]       Collect answers + print the plan; provision nothing\n\
\x20 ma-demo clear [flags]      Wipe the local workspace (no Docker/cloud changes)\n\
\x20 ma-demo destroy [flags]    Delete the local KIND clusters + wipe the workspace\n\
\x20 ma-demo version            Print version\n\
\x20 ma-demo help               This help\n\n\
Flags:\n\
\x20 -y, --non-interactive      Accept defaults; for CI / unattended runs.\n\
\x20 --dry-run, --plan          Stop after printing the plan.\n\
\x20 --workspace DIR            Workspace dir (default ./migration-demo-workspace).\n\n\
This harness stands up a SOURCE search cluster (Elasticsearch / OpenSearch /\n\
Solr), optional snapshot storage + target OpenSearch, and client apps, locally\n\
via KIND or as Terraform for AWS — then installs and launches the Migration\n\
Assistant CLI (from {repo} {ver}).\n\n\
Version: {VERSION}\n",
        repo = crate::plan::MA_REPO,
        ver = crate::plan::MA_VERSION,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::MockRunner;
    use crate::wizard::Answer;
    use std::cell::RefCell;

    /// A scripted wizard: returns canned outcomes per question id.
    struct ScriptWizard {
        answers: std::collections::HashMap<&'static str, Outcome>,
        asked: RefCell<Vec<QuestionId>>,
    }
    impl ScriptWizard {
        fn new() -> Self {
            Self {
                answers: Default::default(),
                asked: RefCell::new(Vec::new()),
            }
        }
        fn with(mut self, id: QuestionId, out: Outcome) -> Self {
            self.answers.insert(key(id), out);
            self
        }
    }
    fn key(id: QuestionId) -> &'static str {
        match id {
            QuestionId::Target => "target",
            QuestionId::SourceEngine => "engine",
            QuestionId::SourceVersion => "version",
            QuestionId::SourcePlugins => "plugins",
            QuestionId::SnapshotStorage => "snapshot",
            QuestionId::TargetMode => "tmode",
            QuestionId::TargetVersion => "tversion",
            QuestionId::Clients => "clients",
            QuestionId::SeedData => "seed",
            QuestionId::Review => "review",
        }
    }
    impl Wizardish for ScriptWizard {
        fn ask(&self, question: &wizard::Question, _preselect: &[String]) -> Result<Outcome> {
            self.asked.borrow_mut().push(question.id);
            self.answers
                .get(key(question.id))
                .cloned()
                .ok_or_else(|| Error::die(format!("no scripted answer for {:?}", question.id)))
        }
    }

    fn ready_runner() -> MockRunner {
        MockRunner::new()
            .with_command("docker")
            .with_command("kind")
            .with_command("kubectl")
            .with_command("curl")
            .with_command("bash")
            .stub("docker", &["info"], 0, "ok")
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_command_exits_64() {
        let r = MockRunner::new();
        let w = ScriptWizard::new();
        assert_eq!(dispatch(&args(&["notacommand"]), &r, &w), 64);
    }

    #[test]
    fn version_and_help_exit_zero() {
        let r = MockRunner::new();
        let w = ScriptWizard::new();
        assert_eq!(dispatch(&args(&["version"]), &r, &w), 0);
        assert_eq!(dispatch(&args(&["help"]), &r, &w), 0);
    }

    #[test]
    fn help_text_mentions_fork_and_subcommands() {
        let h = help_text();
        assert!(h.contains("AndreKurait/opensearch-migrations"));
        assert!(h.contains("ma-demo plan"));
        assert!(h.contains("--non-interactive"));
    }

    #[test]
    fn parse_flags_reads_all_forms() {
        let f = parse_flags(&args(&["-y", "--workspace", "/tmp/ws", "--dry-run"]));
        assert!(f.non_interactive);
        assert!(f.dry_run);
        assert_eq!(f.workspace.as_deref(), Some("/tmp/ws"));

        let f2 = parse_flags(&args(&["--workspace=/x", "--plan"]));
        assert_eq!(f2.workspace.as_deref(), Some("/x"));
        assert!(f2.dry_run);
    }

    #[test]
    fn non_interactive_dry_run_fills_defaults_and_provisions_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let r = ready_runner();
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["plan", "-y", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // Plan was saved.
        assert!(ws.join("plan.json").exists());
        // No kind clusters created (dry run / plan).
        assert!(!r.any_call_contains("kind create"));
    }

    #[test]
    fn interactive_wizard_walks_questions_then_provisions() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let r = ready_runner();
        let w = ScriptWizard::new()
            .with(
                QuestionId::Target,
                Outcome::Answered(Answer::Choice("local".into())),
            )
            .with(
                QuestionId::SourceEngine,
                Outcome::Answered(Answer::Choice("elasticsearch".into())),
            )
            .with(
                QuestionId::SourceVersion,
                Outcome::Answered(Answer::Choice("7.10.2".into())),
            )
            .with(
                QuestionId::SourcePlugins,
                Outcome::Answered(Answer::Choices(vec!["repository-s3".into()])),
            )
            .with(
                QuestionId::SnapshotStorage,
                Outcome::Answered(Answer::Choice("localstack".into())),
            )
            .with(
                QuestionId::TargetMode,
                Outcome::Answered(Answer::Choice("leave-to-ma".into())),
            )
            .with(
                QuestionId::Clients,
                Outcome::Answered(Answer::Choices(vec!["locust".into()])),
            )
            .with(QuestionId::SeedData, Outcome::Answered(Answer::Bool(true)));
        // `plan` so we stop before provisioning Docker, but after walking the wizard.
        let code = dispatch(
            &args(&["plan", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // The wizard skipped TargetVersion (leave-to-ma).
        let asked = w.asked.borrow();
        assert!(asked.contains(&QuestionId::Target));
        assert!(asked.contains(&QuestionId::SeedData));
        assert!(!asked.contains(&QuestionId::TargetVersion));
    }

    #[test]
    fn cancelling_wizard_aborts_with_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let r = ready_runner();
        let w = ScriptWizard::new().with(QuestionId::Target, Outcome::Cancelled);
        let code = dispatch(&args(&["--workspace", ws.to_str().unwrap()]), &r, &w);
        assert_eq!(code, 1);
        assert!(!r.any_call_contains("kind create"));
    }

    #[test]
    fn clear_removes_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("plan.json"), "{}").unwrap();
        let r = MockRunner::new();
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["clear", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        assert!(!ws.exists());
    }

    #[test]
    fn destroy_deletes_both_clusters_and_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("plan.json"), "{}").unwrap();
        let r = MockRunner::new().with_command("kind");
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["destroy", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // Both clusters deleted, workspace gone.
        assert!(r.any_call_contains("kind delete cluster --name ma-demo-source"));
        assert!(r.any_call_contains("kind delete cluster --name ma-demo-target"));
        assert!(!ws.exists());
    }

    #[test]
    fn destroy_without_kind_skips_cluster_deletion_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let r = MockRunner::new(); // no kind on PATH
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["destroy", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        assert!(!r.any_call_contains("kind delete"));
        assert!(!ws.exists());
    }

    #[test]
    fn full_non_interactive_run_provisions_and_launches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let r = ready_runner();
        let w = ScriptWizard::new();
        // Default run (launch=true) but non-interactive → launch_ma reports only.
        let code = dispatch(&args(&["-y", "--workspace", ws.to_str().unwrap()]), &r, &w);
        assert_eq!(code, 0);
        // Provisioned: kind clusters + MA install invoked.
        assert!(r.any_call_contains("kind create cluster --name ma-demo-source"));
        assert!(r.any_call_contains("AndreKurait/opensearch-migrations"));
        // Did NOT exec the MA binary (non-interactive guard).
        assert!(!r.any_call_contains("/migration-assistant"));
    }
}

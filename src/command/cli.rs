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
use crate::tui::{Outcome, ReviewContext, ReviewOutcome};
use crate::wizard::{self, QuestionId};
use crate::{dashboard, terraform, ui};
use std::path::PathBuf;

/// The harness version: the `DEMO_VERSION` env stamped by build.rs at release
/// build time (set from the git tag), or a dev default for a plain `cargo build`.
pub const VERSION: &str = match option_env!("DEMO_VERSION") {
    Some(v) => v,
    None => "0.1.0-dev",
};

/// How the wizard collects answers. The real binary uses the Ratatui TUI; tests
/// drive a scripted collector so the whole dispatch is assertable.
pub trait Wizardish {
    /// Present `question`, returning the chosen [`Outcome`]. `preselect` seeds
    /// multi-choice (resume); `step` is the optional `(n, total)` progress shown
    /// in the footer (`None` for an isolated edit re-ask).
    fn ask(
        &self,
        question: &wizard::Question,
        preselect: &[String],
        step: Option<(usize, usize)>,
    ) -> Result<Outcome>;

    /// Present the editable review of the plan — with the contextual header
    /// (version, workspace, AWS identity, upgrade hint) — and return the
    /// operator's choice (confirm / edit a field / cancel).
    fn review(&self, rows: &[wizard::ReviewRow], context: &ReviewContext) -> Result<ReviewOutcome>;
}

/// The interactive Ratatui wizard.
pub struct TuiWizard;
impl Wizardish for TuiWizard {
    fn ask(
        &self,
        question: &wizard::Question,
        preselect: &[String],
        step: Option<(usize, usize)>,
    ) -> Result<Outcome> {
        crate::tui::run(question.clone(), preselect, step).map_err(Error::from)
    }
    fn review(&self, rows: &[wizard::ReviewRow], context: &ReviewContext) -> Result<ReviewOutcome> {
        crate::tui::run_review(rows.to_vec(), context.clone()).map_err(Error::from)
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
    /// Overrides the MA handoff: "install-cli" or "deploy-local-helm".
    pub ma_handoff: Option<String>,
    /// Skip auto-launching the live dashboard at the end of a run (provision +
    /// print only). For scripted/CI runs and tests.
    pub no_dashboard: bool,
    /// On the cloud path, emit the Terraform but do NOT run `terraform apply`
    /// (the operator applies it themselves).
    pub no_apply: bool,
    /// Explicitly opt in to applying real cloud infra on a NON-interactive run
    /// (`--apply` / `--auto-approve`). Interactive runs apply on review-confirm;
    /// `-y` cloud runs require this so CI never stands up billable infra silently.
    pub apply: bool,
    /// Override the AWS profile for cloud/AOSS operations.
    pub aws_profile: Option<String>,
    /// Override the AWS region for cloud/AOSS operations.
    pub aws_region: Option<String>,
}

/// Parse flags out of the argument list (order-independent). A value-taking flag
/// (`--workspace`, `--ma-handoff`, `--aws-profile`, `--aws-region`) whose next
/// token is missing or itself looks like a flag is a usage error (exit 64) —
/// otherwise `--workspace --dry-run` would silently consume `--dry-run` as the
/// workspace path.
pub fn parse_flags(args: &[String]) -> Result<Flags> {
    let mut f = Flags::default();
    let mut i = 0;
    // Read the value for a value-taking flag, erroring if it's missing or is a flag.
    let take_value = |args: &[String], i: usize, name: &str| -> Result<String> {
        match args.get(i + 1) {
            Some(v) if !v.starts_with('-') => Ok(v.clone()),
            _ => Err(Error::with_code(
                format!("{name} requires a value (e.g. `{name} <value>`)"),
                64,
            )),
        }
    };
    while i < args.len() {
        match args[i].as_str() {
            "-y" | "--non-interactive" | "--yes" => f.non_interactive = true,
            "--dry-run" | "--plan" => f.dry_run = true,
            "--no-dashboard" => f.no_dashboard = true,
            "--no-apply" => f.no_apply = true,
            "--apply" | "--auto-approve" => f.apply = true,
            "--workspace" => {
                f.workspace = Some(take_value(args, i, "--workspace")?);
                i += 1;
            }
            "--ma-handoff" => {
                f.ma_handoff = Some(take_value(args, i, "--ma-handoff")?);
                i += 1;
            }
            "--aws-profile" => {
                f.aws_profile = Some(take_value(args, i, "--aws-profile")?);
                i += 1;
            }
            "--aws-region" => {
                f.aws_region = Some(take_value(args, i, "--aws-region")?);
                i += 1;
            }
            s => {
                if let Some(v) = s.strip_prefix("--workspace=") {
                    f.workspace = Some(v.to_string());
                } else if let Some(v) = s.strip_prefix("--ma-handoff=") {
                    f.ma_handoff = Some(v.to_string());
                } else if let Some(v) = s.strip_prefix("--aws-profile=") {
                    f.aws_profile = Some(v.to_string());
                } else if let Some(v) = s.strip_prefix("--aws-region=") {
                    f.aws_region = Some(v.to_string());
                }
            }
        }
        i += 1;
    }
    Ok(f)
}

/// Run the fail-silent startup update check and return an upgrade hint if a
/// newer release is published. Never errors or blocks (bounded curl, degrades to
/// `None` offline / opted out / in tests).
fn update_hint<R: CommandRunner>(runner: &R) -> Option<String> {
    match crate::update::check(runner, VERSION) {
        crate::update::Update::Available { latest } => {
            Some(crate::update::upgrade_hint(&latest, VERSION))
        }
        _ => None,
    }
}

/// Resolve the AWS-identity header lines for a plan that touches AWS: the
/// account/profile/region summary plus the caller ARN (or a creds-expired
/// warning). Returns `(lines, warning)` where `warning` is true when creds are
/// missing/expired. Exports the AWS env first so the identity probe + any later
/// AWS calls share the same profile/region. Empty when the plan is AWS-free.
fn aws_identity_lines<R: CommandRunner>(runner: &R, answers: &Answers) -> (Vec<String>, bool) {
    if !answers.touches_aws() {
        return (Vec::new(), false);
    }
    crate::app::export_aws_env(answers);
    let profile = answers.effective_aws_profile();
    let region = answers.effective_aws_region();
    let summary = format!("AWS profile={profile}  region={region}");
    match crate::aws::caller_identity(runner, profile, region) {
        Some(arn) => {
            let acct = crate::aws::account_of(&arn).unwrap_or_else(|| "?".into());
            (
                vec![summary, format!("identity {arn}  (account {acct})")],
                false,
            )
        }
        None => (
            vec![
                summary,
                "AWS credentials for that profile are missing or expired — refresh them \
                 (e.g. `aws sso login` / `ada credentials update`) before applying."
                    .to_string(),
            ],
            true,
        ),
    }
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
        Some("status") => cmd_status(runner, &rest),
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

/// The main run path: collect answers, present the review TUI front door (the
/// version/workspace/AWS-identity header + the editable plan), provision on
/// confirm, then install + launch the MA CLI. With `--dry-run` (or the `plan`
/// subcommand, via `force_dry`) it stops after printing the plan, provisioning
/// nothing. Non-interactive (`-y`) skips the TUI and prints the same context.
fn cmd_run<R: CommandRunner, W: Wizardish>(
    runner: &R,
    wiz: &W,
    args: &[String],
    force_dry: bool,
) -> Result<()> {
    let mut flags = parse_flags(args)?;
    flags.dry_run = flags.dry_run || force_dry;
    let workspace = flags
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);

    // Fail-silent: a hint if a newer release is published on GitHub. Computed
    // once — shown in the review TUI header (interactive) or printed (CI).
    let update_hint = update_hint(runner);

    // Load any saved plan (resume), then collect remaining answers.
    let mut state = State::new(&workspace);
    state.load()?;
    let mut answers = state.plan.answers.clone();

    collect_answers(wiz, &mut answers, flags.non_interactive)?;
    // An explicit --ma-handoff flag overrides the collected/default choice.
    if let Some(h) = flags.ma_handoff.as_deref() {
        answers.ma_handoff = match h {
            "install-cli" => Some(crate::model::MaHandoff::InstallCli),
            "deploy-local-helm" => Some(crate::model::MaHandoff::DeployLocalHelm),
            other => return Err(Error::die(format!("unknown --ma-handoff '{other}'"))),
        };
    }
    // Explicit --aws-profile / --aws-region flags override the wizard/defaults.
    if let Some(p) = flags.aws_profile.clone() {
        answers.aws_profile = Some(p);
    }
    if let Some(r) = flags.aws_region.clone() {
        answers.aws_region = Some(r);
    }
    state.plan.answers = answers.clone();
    state.save()?;

    // Two front doors:
    //   • Interactive — the review TUI is the WHOLE front door. It carries the
    //     version/workspace/AWS-identity/upgrade header AND the editable plan, so
    //     there are no bash-style printed lines before it. Shown EVERY run
    //     (including a resume), so a saved plan never auto-proceeds.
    //   • Non-interactive / dry-run — there's no TUI, so print the same context
    //     (banner, version, upgrade hint, AWS identity, plan) to stdout.
    if flags.non_interactive || flags.dry_run {
        ui::banner("Migration Assistant — Demo Environment Setup");
        ui::dim(&format!(
            "  version={VERSION}  workspace={}",
            workspace.display()
        ));
        if let Some(h) = &update_hint {
            ui::warn(h);
        }
        let (lines, warn) = aws_identity_lines(runner, &answers);
        for (i, l) in lines.iter().enumerate() {
            if i == 0 && warn {
                ui::warn(l);
            } else {
                ui::dim(&format!("  {l}"));
            }
        }
        ui::step("Plan");
        print_plan(&answers);
        if flags.dry_run {
            ui::ok("dry run — no resources created");
            return Ok(());
        }
    } else {
        match review_and_edit(
            runner,
            wiz,
            &mut answers,
            &workspace,
            update_hint.as_deref(),
        )? {
            ReviewDecision::Confirm => {
                // Persist any edits, then re-derive the AWS env in case the
                // profile/region changed during review.
                state.plan.answers = answers.clone();
                state.save()?;
                if answers.touches_aws() {
                    crate::app::export_aws_env(&answers);
                }
            }
            ReviewDecision::Cancel => {
                ui::info("cancelled — nothing was created.");
                return Ok(());
            }
        }
    }

    // Decide whether the cloud path applies real infra. Interactive runs applied
    // on the review-confirm above; a NON-interactive cloud run must opt in with
    // --apply (so `-y` in CI never silently stands up billable infrastructure).
    // --no-apply always wins (emit-only).
    let is_cloud = answers.target == Some(crate::model::Target::Cloud);
    let apply_cloud = is_cloud && !flags.no_apply && (!flags.non_interactive || flags.apply);

    // Provision through the orchestrator.
    let progress = UiProgress;
    let mut app = App::new(runner, &workspace, &progress);
    app.state.plan.answers = answers.clone();

    // A one-shot roadmap of the phases ahead, so the operator sees how many
    // steps remain during a multi-minute local provision (interactive only).
    if !flags.non_interactive && answers.target == Some(crate::model::Target::Local) {
        ui::step("Provisioning roadmap");
        ui::dim(&crate::progress::plain(crate::state::Step::Planned));
    }

    app.preflight(&answers)?;
    let plan = app.provision_with(&answers, apply_cloud)?;
    print_endpoints(&plan, &answers, app.state.plan.aoss_endpoint.as_deref());

    // Cloud emit-only: there's nothing live to watch (no infra was applied), so
    // skip the all-Pending dashboard and tell the operator how to apply + watch.
    if is_cloud && !apply_cloud {
        ui::ok("Terraform written — review it, then apply when ready.");
        ui::info(&format!(
            "  terraform -chdir={ws}/terraform init && terraform -chdir={ws}/terraform apply",
            ws = workspace.display()
        ));
        if flags.non_interactive {
            ui::dim("  (or re-run with --apply to have ma-demo apply it for you)");
        }
        ui::dim("  Then watch it live with:  ma-demo status");
        return Ok(());
    }

    // Prepare the Migration Assistant handoff. Local-helm deploys MA into KIND
    // (returns the console-exec argv); otherwise install the EKS-targeting CLI
    // (returns the launch argv). Both stream their own steps.
    let argv = if answers.deploys_ma_locally() {
        app.deploy_ma_local()?
    } else {
        app.install_ma()?
    };
    ui::ok("Environment ready.");

    // The persistent end state: a live, auto-refreshing dashboard of everything
    // provisioned. It stays up until the operator quits (q/Esc). The MA launch
    // command is printed so they can start the migration when ready. In a
    // non-interactive run there's no TTY, so we skip the dashboard and just
    // print the next-step command.
    // Skip the dashboard when there's no usable TTY: non-interactive runs or an
    // explicit --no-dashboard (scripted/CI/tests).
    if flags.non_interactive || flags.no_dashboard {
        ui::dim("  skipping live dashboard; run `ma-demo status` to open it later.");
        ui::info(&format!(
            "  Launch the Migration Assistant with:  {}",
            argv.join(" ")
        ));
        ui::dim("  See live status any time with:  ma-demo status");
        return Ok(());
    }
    let launch = argv.join(" ");
    ui::info(&format!(
        "  When ready, launch the Migration Assistant:  {launch}"
    ));
    ui::dim("  Opening the live status dashboard… (press q to exit, then run the command above)");
    // Pass the launch command so it stays visible in the dashboard footer.
    dashboard::run_live(
        runner,
        &answers,
        std::time::Duration::from_secs(2),
        Some(&launch),
    )?;
    ui::ok("Dashboard closed. Your environment is still running.");
    ui::info(&format!("  Migration Assistant:  {}", argv.join(" ")));
    ui::dim("  Tear it all down with:  ma-demo destroy");
    Ok(())
}

/// `status` — open the live dashboard for an existing environment (re-probes the
/// real resources every 2s, stays up until q/Esc). Reads the saved plan to know
/// what to probe.
fn cmd_status<R: CommandRunner>(runner: &R, args: &[String]) -> Result<()> {
    let flags = parse_flags(args)?;
    let workspace = flags
        .workspace
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);
    let mut state = State::new(&workspace);
    state.load()?;
    if state.plan.answers.target.is_none() {
        return Err(Error::die(
            "no saved plan found — run `ma-demo run` first (or pass --workspace <dir>).",
        ));
    }
    dashboard::run_live(
        runner,
        &state.plan.answers,
        std::time::Duration::from_secs(2),
        None,
    )?;
    Ok(())
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
        let step = Some(wizard::progress_position(id, answers));
        match wiz.ask(&q, &preselect, step)? {
            Outcome::Answered(ans) => {
                wizard::apply(answers, id, ans);
            }
            Outcome::Cancelled => {
                return Err(Error::die("setup cancelled by user"));
            }
        }
    }
}

/// The operator's decision at the review gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Confirm,
    Cancel,
}

/// Show the editable review of the plan and let the operator edit any field
/// until they confirm or cancel. This is the interactive front door: the review
/// TUI carries a header (version/workspace, the AWS identity being deployed
/// into, an upgrade hint) plus the editable plan, so there are no printed lines
/// before it. Runs every interactive run (including resume), so a fully-answered
/// /saved plan never silently proceeds to provisioning. Editing a field re-asks
/// its question via `wiz` and re-derives downstream answers (so e.g. switching
/// the source engine re-prompts for version); the AWS identity in the header is
/// re-probed each loop, so editing the profile/region updates it in place.
fn review_and_edit<R: CommandRunner, W: Wizardish>(
    runner: &R,
    wiz: &W,
    answers: &mut Answers,
    workspace: &std::path::Path,
    update_hint: Option<&str>,
) -> Result<ReviewDecision> {
    loop {
        let (aws, aws_warning) = aws_identity_lines(runner, answers);
        let context = ReviewContext {
            version: VERSION.to_string(),
            workspace: workspace.display().to_string(),
            aws,
            aws_warning,
            update_hint: update_hint.map(str::to_string),
        };
        let rows = wizard::review_rows(answers);
        match wiz.review(&rows, &context)? {
            ReviewOutcome::Confirm => return Ok(ReviewDecision::Confirm),
            ReviewOutcome::Cancel => return Ok(ReviewDecision::Cancel),
            ReviewOutcome::Edit(qid) => {
                // Re-ask the chosen question, applying the new answer. No step
                // counter — this is an isolated edit, not a position in the flow.
                let q = wizard::build(qid, answers);
                let preselect = preselect_for(qid, answers);
                match wiz.ask(&q, &preselect, None)? {
                    Outcome::Answered(ans) => {
                        wizard::apply(answers, qid, ans);
                    }
                    // Cancelling the edit just returns to the review unchanged.
                    Outcome::Cancelled => {}
                }
                // An edit can open a new branch (e.g. switching to a provisioned
                // target reveals target-kind/version) — fill any newly-required
                // unanswered fields with defaults so the review stays complete.
                wizard::fill_defaults(answers);
            }
        }
    }
}

/// The ids to pre-check for a multi-choice question. On resume we keep the
/// prior selection; on first visit we seed the question's checked-by-default
/// set (e.g. the snapshot-repository plugin).
fn preselect_for(id: QuestionId, a: &Answers) -> Vec<String> {
    match id {
        QuestionId::SourcePlugins => {
            if a.plugins_done {
                a.source_plugins.clone()
            } else {
                wizard::default_checked(&wizard::build(QuestionId::SourcePlugins, a))
            }
        }
        QuestionId::Clients => a.clients.iter().map(|c| c.id().to_string()).collect(),
        _ => Vec::new(),
    }
}

/// `clear` — wipe the local workspace (no Docker/cloud changes).
fn cmd_clear(args: &[String]) -> Result<()> {
    let flags = parse_flags(args)?;
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
    let flags = parse_flags(args)?;
    let workspace = flags
        .workspace
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(default_workspace);
    let answers = Answers::new();
    ui::banner("Destroy local demo environment");

    // Load the saved plan (if any) to see whether an AOSS target was provisioned.
    let mut state = State::new(&workspace);
    let _ = state.load();
    let provisioned_aoss = state.plan.answers.provisions_aoss_target();

    if !runner.has_command("kind") {
        ui::warn("kind not on PATH; skipping cluster deletion");
    } else {
        // Source, target, and the dedicated MA cluster (local helm deploy).
        for cluster in [
            answers.source_cluster(),
            answers.target_cluster(),
            answers.ma_cluster(),
        ] {
            ui::step(&format!("kind delete cluster {cluster}"));
            // Idempotent: kind exits 0 (with a notice) when the cluster is absent.
            // A just-running cluster can lose a `docker rm` race on the first
            // try, so retry once before warning.
            let mut out = runner.run("kind", &["delete", "cluster", "--name", &cluster]);
            if !out.success() {
                out = runner.run("kind", &["delete", "cluster", "--name", &cluster]);
            }
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

    // Tear down the AOSS NextGen target (collection → group → policies), if one
    // was provisioned and the aws CLI is available. The saved plan supplies the
    // profile + region so teardown hits the same account/region the create did.
    if provisioned_aoss {
        destroy_aoss(runner, &state.plan.answers);
    }

    if workspace.exists() {
        std::fs::remove_dir_all(&workspace)?;
        ui::ok("workspace cleared");
    }
    ui::dim("  (cloud) for a Terraform deploy, run `terraform destroy` in terraform/");
    Ok(())
}

/// Tear down an AOSS NextGen target in reverse-dependency order: collection →
/// policies → collection group. Best-effort + idempotent — missing resources are
/// not fatal (the workspace wipe still proceeds). The region + profile come from
/// the saved plan (via the exported AWS env), so teardown targets the same
/// account/region the create used — not whatever the ambient env happens to be.
fn destroy_aoss<R: CommandRunner>(runner: &R, answers: &Answers) {
    if !runner.has_command("aws") {
        ui::warn("aws CLI not on PATH; skipping AOSS teardown");
        return;
    }
    // Export AWS_PROFILE/AWS_REGION from the plan so every `aws` call below hits
    // the right account/region.
    crate::app::export_aws_env(answers);
    let collection = answers.aoss_collection_name();
    let region = answers.effective_aws_region();
    ui::step(&format!("delete AOSS collection {collection}"));

    // Resolve the collection ID from its name (delete-collection wants the ID).
    let bg = crate::plan::aoss_batch_get_args(&collection, region);
    let bg_ref: Vec<&str> = bg.iter().map(String::as_str).collect();
    let out = runner.run("aws", &bg_ref);
    if let Some(id) = parse_aoss_id(&out.stdout) {
        let del = crate::plan::aoss_delete_collection_args(&id, region);
        let del_ref: Vec<&str> = del.iter().map(String::as_str).collect();
        let r = runner.run("aws", &del_ref);
        if r.success() {
            ui::ok(&format!("collection {collection} deleted"));
        } else {
            ui::warn(&format!("collection delete: {}", r.stderr.trim()));
        }
    } else {
        ui::dim("  collection not found (already gone)");
    }

    // Policies (best-effort; names are deterministic from the collection name).
    for (name, ptype) in [
        (format!("{collection}-enc"), "encryption"),
        (format!("{collection}-net"), "network"),
    ] {
        let a = crate::plan::aoss_delete_security_policy_args(&name, ptype, region);
        let aref: Vec<&str> = a.iter().map(String::as_str).collect();
        runner.run("aws", &aref);
    }
    let dp = crate::plan::aoss_delete_access_policy_args(&format!("{collection}-data"), region);
    let dpref: Vec<&str> = dp.iter().map(String::as_str).collect();
    runner.run("aws", &dpref);

    // The collection group does NOT auto-GC once empty — delete it explicitly,
    // or it lingers as a billable NextGen group. Resolve its ID then delete.
    let gg = crate::plan::aoss_batch_get_group_args(&collection, region);
    let gg_ref: Vec<&str> = gg.iter().map(String::as_str).collect();
    let gout = runner.run("aws", &gg_ref);
    if let Some(gid) = parse_aoss_group_id(&gout.stdout) {
        let dg = crate::plan::aoss_delete_collection_group_args(&gid, region);
        let dg_ref: Vec<&str> = dg.iter().map(String::as_str).collect();
        let r = runner.run("aws", &dg_ref);
        if r.success() {
            ui::ok(&format!(
                "collection group {} deleted",
                crate::plan::aoss_group_name(&collection)
            ));
        } else {
            // The group can't delete until the collection is fully gone; surface
            // it so the operator can re-run `destroy` once the collection drains.
            ui::warn(&format!(
                "collection group delete: {} (re-run `ma-demo destroy` once the collection finishes deleting)",
                r.stderr.trim()
            ));
        }
    } else {
        ui::dim("  collection group not found (already gone)");
    }
}

/// Parse the collection ID out of a `batch-get-collection` response (any status).
fn parse_aoss_id(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let details = v.get("collectionDetails")?.as_array()?;
    details.first()?.get("id")?.as_str().map(|s| s.to_string())
}

/// Parse the group ID out of a `batch-get-collection-group` response.
fn parse_aoss_group_id(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let details = v.get("collectionGroupDetails")?.as_array()?;
    details.first()?.get("id")?.as_str().map(|s| s.to_string())
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
        ui::dim(&format!("  target          {}", m.label()));
        if let Some(k) = a.target_kind {
            let v = a.target_version.as_deref().unwrap_or("");
            ui::dim(&format!("  target kind     {} {v}", k.label()));
        }
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
    if let Some(h) = a.ma_handoff {
        ui::dim(&format!("  MA handoff      {}", h.label()));
    }
}

/// Print the resolved endpoints after provisioning (so the operator + MA know
/// where things live). `aoss_endpoint` is the resolved AOSS collection endpoint
/// when an AOSS NextGen target was provisioned.
fn print_endpoints(plan: &ProvisionPlan, answers: &Answers, aoss_endpoint: Option<&str>) {
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
    if let Some(ep) = aoss_endpoint {
        ui::dim(&format!(
            "  target   {ep}  (AOSS Serverless NextGen, SigV4 'aoss')"
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
\x20 ma-demo status [flags]     Live, auto-refreshing dashboard of the environment\n\
\x20 ma-demo destroy [flags]    Delete the local KIND clusters + wipe the workspace\n\
\x20 ma-demo version            Print version\n\
\x20 ma-demo help               This help\n\n\
Flags:\n\
\x20 -y, --non-interactive      Accept defaults; for CI / unattended runs.\n\
\x20 --dry-run, --plan          Stop after printing the plan.\n\
\x20 --workspace DIR            Workspace dir (default ./migration-demo-workspace).\n\
\x20 --ma-handoff KIND          MA handoff: deploy-local-helm | install-cli.\n\
\x20 --no-dashboard             Don't auto-open the live status dashboard.\n\
\x20 --no-apply                 Cloud path: emit Terraform but don't apply it.\n\
\x20 --aws-profile NAME         AWS profile for cloud / AOSS operations.\n\
\x20 --aws-region REGION        AWS region for cloud / AOSS operations.\n\n\
Env:\n\
\x20 MA_DEMO_NO_UPDATE_CHECK=1  Skip the startup check for a newer release.\n\n\
On startup, ma-demo checks GitHub for a newer release and prints an upgrade\n\
hint if one exists (fail-silent; never blocks). Before provisioning, it always\n\
shows a review screen of the full plan — edit any field or cancel — so a saved\n\
plan never auto-deploys.\n\n\
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

    /// A scripted wizard: returns canned outcomes per question id, and a
    /// scriptable review outcome (defaults to Confirm). `reviewed` counts how
    /// many times the review gate was shown.
    struct ScriptWizard {
        answers: std::collections::HashMap<&'static str, Outcome>,
        asked: RefCell<Vec<QuestionId>>,
        review: RefCell<Vec<ReviewOutcome>>,
        reviewed: std::cell::Cell<usize>,
    }
    impl ScriptWizard {
        fn new() -> Self {
            Self {
                answers: Default::default(),
                asked: RefCell::new(Vec::new()),
                review: RefCell::new(Vec::new()),
                reviewed: std::cell::Cell::new(0),
            }
        }
        fn with(mut self, id: QuestionId, out: Outcome) -> Self {
            self.answers.insert(key(id), out);
            self
        }
        /// Queue review outcomes (consumed in order; defaults to Confirm).
        fn with_review(self, outs: Vec<ReviewOutcome>) -> Self {
            *self.review.borrow_mut() = outs;
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
            QuestionId::TargetKind => "tkind",
            QuestionId::TargetVersion => "tversion",
            QuestionId::AwsProfile => "awsprofile",
            QuestionId::AwsRegion => "awsregion",
            QuestionId::Clients => "clients",
            QuestionId::SeedData => "seed",
            QuestionId::MaHandoff => "handoff",
            QuestionId::Review => "review",
        }
    }
    impl Wizardish for ScriptWizard {
        fn ask(
            &self,
            question: &wizard::Question,
            _preselect: &[String],
            _step: Option<(usize, usize)>,
        ) -> Result<Outcome> {
            self.asked.borrow_mut().push(question.id);
            self.answers
                .get(key(question.id))
                .cloned()
                .ok_or_else(|| Error::die(format!("no scripted answer for {:?}", question.id)))
        }
        fn review(
            &self,
            _rows: &[wizard::ReviewRow],
            _context: &ReviewContext,
        ) -> Result<ReviewOutcome> {
            self.reviewed.set(self.reviewed.get() + 1);
            let mut q = self.review.borrow_mut();
            if q.is_empty() {
                Ok(ReviewOutcome::Confirm)
            } else {
                Ok(q.remove(0))
            }
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
        let f = parse_flags(&args(&["-y", "--workspace", "/tmp/ws", "--dry-run"])).unwrap();
        assert!(f.non_interactive);
        assert!(f.dry_run);
        assert_eq!(f.workspace.as_deref(), Some("/tmp/ws"));

        let f2 = parse_flags(&args(&["--workspace=/x", "--plan"])).unwrap();
        assert_eq!(f2.workspace.as_deref(), Some("/x"));
        assert!(f2.dry_run);

        // The cloud-apply opt-in (both spellings) and emit-only flag.
        assert!(parse_flags(&args(&["--apply"])).unwrap().apply);
        assert!(parse_flags(&args(&["--auto-approve"])).unwrap().apply);
        assert!(parse_flags(&args(&["--no-apply"])).unwrap().no_apply);
        assert!(!parse_flags(&args(&["-y"])).unwrap().apply);
    }

    #[test]
    fn parse_flags_rejects_value_flag_followed_by_a_flag() {
        // `--workspace --dry-run` must NOT swallow --dry-run as the path.
        let e = parse_flags(&args(&["--workspace", "--dry-run"])).unwrap_err();
        assert_eq!(e.code, 64);
        // A missing trailing value is the same usage error.
        assert_eq!(parse_flags(&args(&["--workspace"])).unwrap_err().code, 64);
        // The `=` form is unaffected.
        assert_eq!(
            parse_flags(&args(&["--workspace=/ok", "--dry-run"]))
                .unwrap()
                .workspace
                .as_deref(),
            Some("/ok")
        );
    }

    #[test]
    fn non_interactive_cloud_does_not_apply_without_apply_flag() {
        // A `-y` cloud run must NOT stand up real infra unless --apply is given.
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut st = State::new(&ws);
        let mut a = Answers::new();
        a.target = Some(crate::model::Target::Cloud);
        a.source_engine = Some(crate::model::SourceEngine::Elasticsearch);
        a.source_version = Some("7.10.2".into());
        a.plugins_done = true;
        a.snapshot_storage = Some(crate::model::SnapshotStorage::AwsS3);
        a.target_mode = Some(crate::model::TargetMode::LeaveToMa);
        a.aws_profile = Some("default".into());
        a.aws_region = Some("us-east-1".into());
        a.clients_done = true;
        a.seed_data = Some(false);
        st.plan.answers = a;
        st.save().unwrap();

        let r = ready_runner().with_command("terraform");
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["run", "-y", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // Emit-only: terraform files written, but NOT applied.
        assert!(!r.any_call_contains("terraform -chdir"));
        assert!(ws.join("terraform/providers.tf").exists());
    }

    #[test]
    fn non_interactive_cloud_applies_with_apply_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut st = State::new(&ws);
        let mut a = Answers::new();
        a.target = Some(crate::model::Target::Cloud);
        a.source_engine = Some(crate::model::SourceEngine::Elasticsearch);
        a.source_version = Some("7.10.2".into());
        a.plugins_done = true;
        a.snapshot_storage = Some(crate::model::SnapshotStorage::AwsS3);
        a.target_mode = Some(crate::model::TargetMode::LeaveToMa);
        a.aws_profile = Some("default".into());
        a.aws_region = Some("us-east-1".into());
        a.clients_done = true;
        a.seed_data = Some(false);
        st.plan.answers = a;
        st.save().unwrap();

        let r = ready_runner().with_command("terraform");
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&[
                "run",
                "-y",
                "--apply",
                "--no-dashboard",
                "--workspace",
                ws.to_str().unwrap(),
            ]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // --apply → terraform init + apply actually run.
        assert!(r.any_call_contains("terraform -chdir"));
        assert!(r.any_call_contains("apply"));
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
            .with(QuestionId::SeedData, Outcome::Answered(Answer::Bool(true)))
            .with(
                QuestionId::MaHandoff,
                Outcome::Answered(Answer::Choice("install-cli".into())),
            );
        // `plan` so we stop before provisioning Docker, but after walking the wizard.
        let code = dispatch(
            &args(&["plan", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // The wizard skipped TargetVersion (leave-to-ma) but asked the handoff
        // (local run).
        let asked = w.asked.borrow();
        assert!(asked.contains(&QuestionId::Target));
        assert!(asked.contains(&QuestionId::SeedData));
        assert!(!asked.contains(&QuestionId::TargetVersion));
        assert!(asked.contains(&QuestionId::MaHandoff));
    }

    /// A saved/complete plan must STILL show the review gate (the footgun fix)
    /// — it must not silently proceed to provisioning. Cancelling at the review
    /// aborts with nothing created.
    #[test]
    fn saved_plan_shows_review_and_cancel_provisions_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        // Pre-seed a fully-answered cloud/AOSS plan (the dangerous case).
        let mut st = State::new(&ws);
        let mut a = Answers::new();
        a.target = Some(crate::model::Target::Cloud);
        a.source_engine = Some(crate::model::SourceEngine::Solr);
        a.source_version = Some("8.11.3".into());
        a.plugins_done = true;
        a.snapshot_storage = Some(crate::model::SnapshotStorage::AwsS3);
        a.target_mode = Some(crate::model::TargetMode::Provision);
        a.target_kind = Some(crate::model::TargetKind::AossServerlessNextGen);
        a.aws_profile = Some("default".into());
        a.aws_region = Some("us-east-1".into());
        a.clients_done = true;
        a.seed_data = Some(true);
        a.ma_handoff = Some(crate::model::MaHandoff::InstallCli);
        st.plan.answers = a;
        st.save().unwrap();

        let r = ready_runner().with_command("terraform");
        // The wizard asks nothing (all answered); the review is shown → Cancel.
        let w = ScriptWizard::new().with_review(vec![ReviewOutcome::Cancel]);
        let code = dispatch(&args(&["run", "--workspace", ws.to_str().unwrap()]), &r, &w);
        assert_eq!(code, 0);
        // The review WAS shown (footgun fix), and nothing was provisioned.
        assert_eq!(w.reviewed.get(), 1, "review gate must run for a saved plan");
        assert!(!r.any_call_contains("terraform"));
        assert!(
            w.asked.borrow().is_empty(),
            "no questions re-asked for a complete plan"
        );
    }

    /// Editing a field at the review re-asks that question, then re-shows the
    /// review; confirming then proceeds with the edited value.
    #[test]
    fn review_edit_reasks_then_confirms() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut st = State::new(&ws);
        let mut a = Answers::new();
        a.target = Some(crate::model::Target::Local);
        a.source_engine = Some(crate::model::SourceEngine::Elasticsearch);
        a.source_version = Some("8.17.0".into());
        a.plugins_done = true;
        a.snapshot_storage = Some(crate::model::SnapshotStorage::None);
        a.target_mode = Some(crate::model::TargetMode::LeaveToMa);
        a.clients_done = true;
        a.seed_data = Some(false);
        a.ma_handoff = Some(crate::model::MaHandoff::InstallCli);
        st.plan.answers = a;
        st.save().unwrap();

        let r = ready_runner();
        // Review: edit the source version → re-ask returns 7.10.2 → confirm.
        let w = ScriptWizard::new()
            .with(
                QuestionId::SourceVersion,
                Outcome::Answered(Answer::Choice("7.10.2".into())),
            )
            .with_review(vec![
                ReviewOutcome::Edit(QuestionId::SourceVersion),
                ReviewOutcome::Confirm,
            ]);
        // Review only runs on a real run (not dry-run). --no-dashboard so the
        // post-provision live dashboard (needs a TTY) is skipped in tests.
        let code = dispatch(
            &args(&["run", "--no-dashboard", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // The edit re-asked SourceVersion and the review ran twice.
        assert!(w.asked.borrow().contains(&QuestionId::SourceVersion));
        assert_eq!(w.reviewed.get(), 2);
        // The edited value was persisted.
        let mut reloaded = State::new(&ws);
        reloaded.load().unwrap();
        assert_eq!(
            reloaded.plan.answers.source_version.as_deref(),
            Some("7.10.2")
        );
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
    fn destroy_tears_down_aoss_target_when_planned() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        // Persist a plan whose target is AOSS NextGen.
        let mut st = State::new(&ws);
        st.plan.answers.target_mode = Some(crate::model::TargetMode::Provision);
        st.plan.answers.target_kind = Some(crate::model::TargetKind::AossServerlessNextGen);
        st.save().unwrap();

        let r = MockRunner::new()
            .with_command("kind")
            .with_command("aws")
            // More specific stub first (first-match-wins): the group resolve.
            .stub(
                "aws",
                &["batch-get-collection-group"],
                0,
                r#"{"collectionGroupDetails":[{"id":"grp789","name":"cg-ma-demo-target"}]}"#,
            )
            .stub(
                "aws",
                &["batch-get-collection"],
                0,
                r#"{"collectionDetails":[{"status":"ACTIVE","id":"colid123","collectionEndpoint":"https://x.aoss.us-east-1.on.aws"}]}"#,
            );
        let w = ScriptWizard::new();
        let code = dispatch(
            &args(&["destroy", "--workspace", ws.to_str().unwrap()]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // Resolved the ID and deleted the collection by ID.
        assert!(r.any_call_contains("delete-collection --region"));
        assert!(r.any_call_contains("--id colid123"));
        // Deleted the policies too.
        assert!(r.any_call_contains("delete-security-policy"));
        assert!(r.any_call_contains("delete-access-policy"));
        // And tore down the NextGen collection group by its resolved ID (it does
        // not auto-GC, so leaving it would keep billing).
        assert!(r.any_call_contains("delete-collection-group --region"));
        assert!(r.any_call_contains("--id grp789"));
        assert!(!ws.exists());
    }

    #[test]
    fn full_non_interactive_run_provisions_and_launches() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let r = ready_runner();
        let w = ScriptWizard::new();
        // Non-interactive, with the CLI-install handoff explicitly selected
        // (the local-helm default would need helm + the chart).
        let code = dispatch(
            &args(&[
                "-y",
                "--ma-handoff",
                "install-cli",
                "--workspace",
                ws.to_str().unwrap(),
            ]),
            &r,
            &w,
        );
        assert_eq!(code, 0);
        // Provisioned: kind clusters + MA install invoked. (Use the install-URL
        // marker, not the bare repo name — the startup update check also curls a
        // GitHub URL containing "opensearch-migrations".)
        assert!(r.any_call_contains("kind create cluster --name ma-demo-source"));
        assert!(r.any_call_contains("opensearch-migrations/releases/download/3.3.1"));
        // Did NOT exec the MA binary (non-interactive guard).
        assert!(!r.any_call_contains("/migration-assistant"));
    }
}

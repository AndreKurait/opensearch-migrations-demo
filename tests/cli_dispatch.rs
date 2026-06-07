//! Integration: the dispatcher contract — exit codes, the help/version surface,
//! the plan (dry-run) vs run distinction, and the cancel path. Drives the CLI
//! with a scripted wizard + a mock runner. Mirrors the migration-assistant
//! CLI's `cli_dispatch.rs`.

use ma_demo::cli::{self, Wizardish};
use ma_demo::error::{Error, Result};
use ma_demo::runner::MockRunner;
use ma_demo::tui::Outcome;
use ma_demo::wizard::{self, Answer, QuestionId};
use std::collections::HashMap;

/// A scripted wizard returning canned outcomes keyed by question id.
struct Script {
    answers: HashMap<QuestionId, Outcome>,
}
impl Script {
    fn new() -> Self {
        Self {
            answers: HashMap::new(),
        }
    }
    fn full_local() -> Self {
        let mut s = Self::new();
        s.answers.insert(QuestionId::Target, ans("local"));
        s.answers
            .insert(QuestionId::SourceEngine, ans("elasticsearch"));
        s.answers.insert(QuestionId::SourceVersion, ans("7.10.2"));
        s.answers.insert(
            QuestionId::SourcePlugins,
            Outcome::Answered(Answer::Choices(vec!["repository-s3".into()])),
        );
        s.answers
            .insert(QuestionId::SnapshotStorage, ans("localstack"));
        s.answers.insert(QuestionId::TargetMode, ans("provision"));
        s.answers
            .insert(QuestionId::TargetKind, ans("kind-opensearch"));
        s.answers.insert(QuestionId::TargetVersion, ans("3.3.0"));
        s.answers.insert(
            QuestionId::Clients,
            Outcome::Answered(Answer::Choices(vec!["locust".into()])),
        );
        s.answers
            .insert(QuestionId::SeedData, Outcome::Answered(Answer::Bool(true)));
        // Local run → also answer the MA-handoff choice (install the CLI here).
        s.answers.insert(QuestionId::MaHandoff, ans("install-cli"));
        s
    }
}
fn ans(id: &str) -> Outcome {
    Outcome::Answered(Answer::Choice(id.into()))
}
impl Wizardish for Script {
    fn ask(&self, question: &wizard::Question, _preselect: &[String]) -> Result<Outcome> {
        self.answers
            .get(&question.id)
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
    let w = Script::new();
    assert_eq!(cli::dispatch(&args(&["bogus"]), &r, &w), 64);
}

#[test]
fn version_exits_zero_and_is_semverish() {
    let r = MockRunner::new();
    let w = Script::new();
    assert_eq!(cli::dispatch(&args(&["version"]), &r, &w), 0);
    // The compiled-in version is x.y.z(-suffix).
    assert!(cli::VERSION.split('.').count() >= 3);
}

#[test]
fn help_exits_zero_and_describes_the_harness() {
    let r = MockRunner::new();
    let w = Script::new();
    assert_eq!(cli::dispatch(&args(&["help"]), &r, &w), 0);
    let h = cli::help_text();
    assert!(h.contains("OpenSearch Migration Assistant demo"));
    assert!(h.contains("AndreKurait/opensearch-migrations"));
}

#[test]
fn plan_subcommand_collects_but_provisions_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner();
    let w = Script::full_local();
    let code = cli::dispatch(
        &args(&["plan", "--workspace", ws.to_str().unwrap()]),
        &r,
        &w,
    );
    assert_eq!(code, 0);
    // Plan saved, but no kind clusters created.
    assert!(ws.join("plan.json").exists());
    assert!(!r.any_call_contains("kind create"));
}

#[test]
fn full_run_provisions_and_installs_ma() {
    // --no-dashboard skips the live dashboard auto-launch (needs a TTY) so this
    // interactive run can assert the provisioning + MA-install steps.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner();
    let w = Script::full_local();
    let code = cli::dispatch(
        &args(&["run", "--no-dashboard", "--workspace", ws.to_str().unwrap()]),
        &r,
        &w,
    );
    assert_eq!(code, 0);
    assert!(r.any_call_contains("kind create cluster --name ma-demo-source"));
    assert!(r.any_call_contains("kind create cluster --name ma-demo-target"));
    assert!(r.any_call_contains("AndreKurait/opensearch-migrations"));
}

#[test]
fn aoss_nextgen_target_provisions_via_aws_not_a_second_cluster() {
    std::env::set_var("MA_DEMO_TEST", "1");
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner()
        .with_command("aws")
        .stub(
            "aws",
            &["sts", "get-caller-identity"],
            0,
            "arn:aws:sts::874041194807:assumed-role/IibsAdminAccess-DO-NOT-DELETE/me",
        )
        .stub(
            "aws",
            &["batch-get-collection"],
            0,
            r#"{"collectionDetails":[{"status":"ACTIVE","collectionEndpoint":"https://xyz.aoss.us-east-1.on.aws","id":"xyz"}]}"#,
        );
    // A wizard that picks the AOSS NextGen target (skips target version).
    let mut w = Script::new();
    w.answers.insert(QuestionId::Target, ans("local"));
    w.answers
        .insert(QuestionId::SourceEngine, ans("elasticsearch"));
    w.answers.insert(QuestionId::SourceVersion, ans("7.10.2"));
    w.answers.insert(
        QuestionId::SourcePlugins,
        Outcome::Answered(Answer::Choices(vec![])),
    );
    w.answers.insert(QuestionId::SnapshotStorage, ans("none"));
    w.answers.insert(QuestionId::TargetMode, ans("provision"));
    w.answers
        .insert(QuestionId::TargetKind, ans("aoss-nextgen"));
    w.answers.insert(
        QuestionId::Clients,
        Outcome::Answered(Answer::Choices(vec![])),
    );
    w.answers
        .insert(QuestionId::SeedData, Outcome::Answered(Answer::Bool(false)));
    // Local run → the wizard asks the handoff; pick the CLI so this test stays
    // focused on the AOSS target path.
    w.answers.insert(QuestionId::MaHandoff, ans("install-cli"));

    let code = cli::dispatch(
        &args(&["run", "--no-dashboard", "--workspace", ws.to_str().unwrap()]),
        &r,
        &w,
    );
    assert_eq!(code, 0);
    // Source KIND cluster yes; NO target KIND cluster.
    assert!(r.any_call_contains("kind create cluster --name ma-demo-source"));
    assert!(!r.any_call_contains("kind create cluster --name ma-demo-target"));
    // AOSS NextGen collection provisioned via the aws CLI.
    assert!(r.any_call_contains("create-collection-group"));
    assert!(r.any_call_contains("--generation NEXTGEN"));
    assert!(r.any_call_contains("create-collection"));
    std::env::remove_var("MA_DEMO_TEST");
}

#[test]
fn local_helm_handoff_deploys_ma_to_kind_and_execs_console() {
    let chart = tempfile::tempdir().unwrap();
    std::env::set_var("MA_CHART_PATH", chart.path());
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner().with_command("helm");
    let w = Script::full_local();
    // Non-interactive override → deploy MA locally via helm.
    let code = cli::dispatch(
        &args(&[
            "-y",
            "--ma-handoff",
            "deploy-local-helm",
            "--workspace",
            ws.to_str().unwrap(),
        ]),
        &r,
        &w,
    );
    assert_eq!(code, 0);
    // A dedicated MA KIND cluster + helm install of the chart; no CLI download.
    assert!(r.any_call_contains("kind create cluster --name ma-demo-ma"));
    assert!(r.any_call_contains("upgrade --install --create-namespace"));
    assert!(r.any_call_contains("opensearchstaging/opensearch-migrations-console"));
    assert!(!r.any_call_contains("AndreKurait/opensearch-migrations"));
    std::env::remove_var("MA_CHART_PATH");
}

#[test]
fn status_without_a_saved_plan_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner();
    let w = Script::new();
    // No saved plan in this fresh workspace → status should error cleanly
    // rather than open a dashboard with nothing to show.
    let code = cli::dispatch(
        &args(&["status", "--workspace", ws.to_str().unwrap()]),
        &r,
        &w,
    );
    assert_eq!(code, 1);
}

#[test]
fn preflight_failure_aborts_before_provisioning() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    // kind missing → preflight fails.
    let r = MockRunner::new()
        .with_command("docker")
        .with_command("kubectl")
        .with_command("curl")
        .stub("docker", &["info"], 0, "ok");
    let w = Script::full_local();
    let code = cli::dispatch(&args(&["run", "--workspace", ws.to_str().unwrap()]), &r, &w);
    assert_eq!(code, 1);
    assert!(!r.any_call_contains("kind create"));
}

#[test]
fn cancel_during_wizard_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner();
    let mut w = Script::new();
    w.answers.insert(QuestionId::Target, Outcome::Cancelled);
    let code = cli::dispatch(&args(&["--workspace", ws.to_str().unwrap()]), &r, &w);
    assert_eq!(code, 1);
    assert!(!r.any_call_contains("kind create"));
}

#[test]
fn resume_reuses_saved_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    let r = ready_runner();
    // First pass: plan only (saves answers).
    let w = Script::full_local();
    cli::dispatch(
        &args(&["plan", "--workspace", ws.to_str().unwrap()]),
        &r,
        &w,
    );
    // Second pass with an EMPTY script: if resume works, the wizard never asks
    // (all answers are already saved) and the plan run succeeds.
    let empty = Script::new();
    let code = cli::dispatch(
        &args(&["plan", "--workspace", ws.to_str().unwrap()]),
        &r,
        &empty,
    );
    assert_eq!(code, 0, "resume should not need to ask any question again");
}

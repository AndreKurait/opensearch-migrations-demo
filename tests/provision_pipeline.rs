//! Integration: the full local provisioning pipeline through the orchestrator
//! against a `MockRunner`. Asserts the exact external-command sequence and the
//! persisted state transitions — no Docker. Mirrors the migration-assistant
//! CLI's `deploy_pipeline.rs`.

use ma_demo::app::{App, Progress, SilentProgress};
use ma_demo::model::{Answers, ClientApp, SnapshotStorage, SourceEngine, Target, TargetMode};
use ma_demo::runner::MockRunner;
use ma_demo::state::Step;
use std::sync::Mutex;

/// A progress sink that records every step/info line for assertions.
struct RecordingProgress {
    lines: Mutex<Vec<String>>,
}
impl RecordingProgress {
    fn new() -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
        }
    }
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}
impl Progress for RecordingProgress {
    fn step(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("step:{msg}"));
    }
    fn info(&self, msg: &str) {
        self.lines.lock().unwrap().push(format!("info:{msg}"));
    }
}

fn full_local() -> Answers {
    let mut a = Answers::new();
    a.target = Some(Target::Local);
    a.source_engine = Some(SourceEngine::Elasticsearch);
    a.source_version = Some("7.10.2".into());
    a.source_plugins = vec!["repository-s3".into()];
    a.snapshot_storage = Some(SnapshotStorage::LocalStack);
    a.target_mode = Some(TargetMode::Provision);
    a.target_version = Some("3.3.0".into());
    a.clients = vec![ClientApp::Locust, ClientApp::SampleSearchApp];
    a.seed_data = Some(true);
    a
}

fn ready_runner() -> MockRunner {
    MockRunner::new()
        .with_command("docker")
        .with_command("kind")
        .with_command("kubectl")
        .with_command("curl")
        .with_command("bash")
        .stub("docker", &["info"], 0, "Server Version: 29")
}

#[test]
fn full_local_pipeline_runs_every_stage_in_order() {
    let answers = full_local();
    let r = ready_runner();
    let tmp = tempfile::tempdir().unwrap();
    let prog = RecordingProgress::new();
    let mut app = App::new(&r, tmp.path(), &prog);
    app.state.plan.answers = answers.clone();

    app.preflight(&answers).unwrap();
    let plan = app.provision(&answers).unwrap();
    let argv = app.install_ma().unwrap();

    // The recorded external commands, in order.
    let kind_calls = r.calls_to("kind");
    assert_eq!(kind_calls.len(), 2, "one kind create per cluster");
    assert!(kind_calls[0].joined().contains("ma-demo-source"));
    assert!(kind_calls[1].joined().contains("ma-demo-target"));

    // Source manifests applied to the source context, target to the target.
    assert!(r.any_call_contains("kubectl --context kind-ma-demo-source apply"));
    assert!(r.any_call_contains("kubectl --context kind-ma-demo-target apply"));

    // Readiness waits issued for both clusters.
    assert!(r.any_call_contains("--context kind-ma-demo-source wait"));
    assert!(r.any_call_contains("--context kind-ma-demo-target wait"));

    // MA installed from the fork release; launch argv points at the workspace bin.
    assert!(r.any_call_contains("AndreKurait/opensearch-migrations"));
    assert!(argv[0].ends_with("/migration-assistant"));

    // State advanced all the way to Ready.
    assert_eq!(app.state.plan.step, Step::Ready);

    // The endpoints the plan resolved.
    assert_eq!(plan.source_host, "source-elasticsearch");
    assert_eq!(plan.target_host.as_deref(), Some("target-opensearch"));

    // Progress narrated the key steps.
    let lines = prog.lines();
    assert!(lines.iter().any(|l| l.contains("Preflight")));
    assert!(lines.iter().any(|l| l.contains("ma-demo-source")));
    assert!(lines
        .iter()
        .any(|l| l.contains("Installing Migration Assistant")));
}

#[test]
fn solr_source_skips_target_and_localstack_when_minimal() {
    let mut a = Answers::new();
    a.target = Some(Target::Local);
    a.source_engine = Some(SourceEngine::Solr);
    a.source_version = Some("9.7.0".into());
    a.snapshot_storage = Some(SnapshotStorage::None);
    a.target_mode = Some(TargetMode::LeaveToMa);
    a.clients = vec![];
    a.seed_data = Some(false);

    let r = ready_runner();
    let tmp = tempfile::tempdir().unwrap();
    let prog = SilentProgress;
    let mut app = App::new(&r, tmp.path(), &prog);
    app.state.plan.answers = a.clone();
    app.preflight(&a).unwrap();
    let plan = app.provision(&a).unwrap();

    // Only the source cluster is created.
    assert_eq!(r.calls_to("kind").len(), 1);
    assert!(r.any_call_contains("ma-demo-source"));
    assert!(!r.any_call_contains("ma-demo-target"));
    // No localstack / data-seed / client manifests written.
    assert!(!tmp.path().join("source-localstack.yaml").exists());
    assert!(!tmp.path().join("source-data-seed.yaml").exists());
    assert!(!tmp.path().join("source-locust.yaml").exists());
    // Solr source manifest is present with the right image.
    let src = std::fs::read_to_string(tmp.path().join("source-source.yaml")).unwrap();
    assert!(src.contains("solr:9.7.0"));
    assert!(plan.target_host.is_none());
}

#[test]
fn state_persists_across_a_reload() {
    let answers = full_local();
    let r = ready_runner();
    let tmp = tempfile::tempdir().unwrap();
    {
        let prog = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &prog);
        app.state.plan.answers = answers.clone();
        app.preflight(&answers).unwrap();
        app.provision(&answers).unwrap();
    }
    // A fresh State over the same dir reloads the saved plan + step.
    let mut reloaded = ma_demo::state::State::new(tmp.path());
    reloaded.load().unwrap();
    assert_eq!(
        reloaded.plan.answers.source_version.as_deref(),
        Some("7.10.2")
    );
    assert!(reloaded.plan.step.index() >= Step::TargetUp.index());
}

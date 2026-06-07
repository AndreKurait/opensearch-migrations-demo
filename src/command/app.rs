//! The provisioning orchestrator.
//!
//! Walks a [`ProvisionPlan`] and performs each [`Action`] through the
//! [`CommandRunner`] seam: writing rendered manifests to the workspace,
//! `kind create cluster`, `kubectl apply`, `kubectl wait`. Then it emits the
//! cloud terraform (when applicable), installs the Migration Assistant CLI from
//! the fork release, and returns the launch argv for the dispatcher to exec.
//!
//! Every external call goes through the runner, so the whole pipeline is
//! asserted against a [`MockRunner`] with no Docker/AWS — the same discipline
//! the migration-assistant CLI uses for its deploy pipeline.

use crate::error::{Error, Result};
use crate::model::{Answers, Target};
use crate::plan::{self, Action, ClusterRole, ProvisionPlan};
use crate::runner::CommandRunner;
use crate::state::{State, Step};
use crate::{manifests, terraform};
use std::path::{Path, PathBuf};

/// Required external tools for the local (KIND) path.
pub const REQUIRED_LOCAL_TOOLS: [&str; 4] = ["docker", "kind", "kubectl", "curl"];

/// The kubectl context name KIND assigns to a cluster (`kind-<name>`).
pub fn kube_context(cluster: &str) -> String {
    format!("kind-{cluster}")
}

/// A sink for human-facing progress lines. The real app prints via `ui`; tests
/// capture into a Vec. Keeps the orchestrator's output assertable.
pub trait Progress {
    fn step(&self, msg: &str);
    fn info(&self, msg: &str);
}

/// A no-op progress sink (tests that don't care about output).
pub struct SilentProgress;
impl Progress for SilentProgress {
    fn step(&self, _msg: &str) {}
    fn info(&self, _msg: &str) {}
}

/// The orchestrator, bound to a runner, a workspace, and a progress sink.
pub struct App<'a, R: CommandRunner, P: Progress> {
    pub runner: &'a R,
    pub state: State,
    pub progress: &'a P,
}

impl<'a, R: CommandRunner, P: Progress> App<'a, R, P> {
    pub fn new(runner: &'a R, workspace: impl Into<PathBuf>, progress: &'a P) -> Self {
        Self {
            runner,
            state: State::new(workspace.into()),
            progress,
        }
    }

    /// Preflight: every required tool resolves on PATH AND the Docker daemon
    /// answers. Returns an actionable error otherwise. Only the local path
    /// needs Docker/KIND; the cloud path just needs terraform later (the user
    /// runs it), so we check tools by target.
    pub fn preflight(&mut self, answers: &Answers) -> Result<()> {
        self.progress.step("Preflight checks");
        if answers.target == Some(Target::Local) {
            for tool in REQUIRED_LOCAL_TOOLS {
                if !self.runner.has_command(tool) {
                    return Err(Error::die(format!(
                        "required tool not found on PATH: {tool}. Install it and re-run."
                    )));
                }
            }
            // Docker daemon must answer, not just be on PATH.
            if !self.runner.run_ok("docker", &["info"]) {
                return Err(Error::die(
                    "Docker is installed but the daemon is not responding. Start Docker and re-run.",
                ));
            }
            self.progress
                .info("all local tools present; docker daemon up");
        } else {
            self.progress
                .info("cloud target: terraform will be emitted for you to apply");
        }
        self.state.advance(Step::PreflightDone);
        self.state.save()?;
        Ok(())
    }

    /// Execute the full plan for `answers`. Local: walk the KIND actions. Cloud:
    /// write the terraform files. Returns once the environment is provisioned
    /// (before the MA install/launch, which the dispatcher drives).
    pub fn provision(&mut self, answers: &Answers) -> Result<ProvisionPlan> {
        let plan = plan::build(answers);
        if answers.target == Some(Target::Cloud) {
            self.emit_terraform(answers)?;
            return Ok(plan);
        }
        self.run_actions(&plan)?;
        Ok(plan)
    }

    /// Walk a local plan's actions, advancing the recorded step as roles finish.
    fn run_actions(&mut self, plan: &ProvisionPlan) -> Result<()> {
        for action in &plan.actions {
            self.progress.step(&action.describe());
            self.perform(action)?;
            self.advance_for(action);
            self.state.save()?;
        }
        Ok(())
    }

    /// Perform one action through the runner.
    fn perform(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::CreateKindCluster { name, config, .. } => {
                let cfg_path = self.write_artifact(&format!("kind-{name}.yaml"), config)?;
                let out = self.runner.run(
                    "kind",
                    &[
                        "create",
                        "cluster",
                        "--name",
                        name,
                        "--config",
                        &cfg_path.to_string_lossy(),
                    ],
                );
                if !out.success() && !Self::already_exists(&out.stderr) {
                    return Err(Error::die(format!(
                        "kind create cluster {name} failed: {}",
                        out.stderr.trim()
                    )));
                }
                Ok(())
            }
            Action::ApplyManifest {
                role, name, body, ..
            } => {
                let path =
                    self.write_artifact(&format!("{}-{name}.yaml", role_stem(*role)), body)?;
                let ctx = self.context_for(*role);
                let path_str = path.to_string_lossy().to_string();
                let out = self
                    .runner
                    .run("kubectl", &["--context", &ctx, "apply", "-f", &path_str]);
                if out.success() {
                    return Ok(());
                }
                // Resume/re-run robustness: some resources (notably Jobs) have an
                // immutable pod template, so a plain `apply` of a changed spec is
                // rejected. Recover by deleting then re-applying — idempotent for
                // the run-it-again-until-it-works flow.
                if Self::is_immutable_error(&out.stderr) {
                    self.runner.run(
                        "kubectl",
                        &[
                            "--context",
                            &ctx,
                            "delete",
                            "-f",
                            &path_str,
                            "--ignore-not-found",
                            "--wait=true",
                        ],
                    );
                    let retry = self
                        .runner
                        .run("kubectl", &["--context", &ctx, "apply", "-f", &path_str]);
                    if retry.success() {
                        return Ok(());
                    }
                    return Err(Error::die(format!(
                        "kubectl apply {name} failed after delete+retry: {}",
                        retry.stderr.trim()
                    )));
                }
                Err(Error::die(format!(
                    "kubectl apply {name} failed: {}",
                    out.stderr.trim()
                )))
            }
            Action::WaitReady { role, .. } => {
                let ctx = self.context_for(*role);
                // Best-effort readiness wait; a timeout is not fatal to the
                // demo (the operator can inspect pods), so we don't error out.
                self.runner.run(
                    "kubectl",
                    &[
                        "--context",
                        &ctx,
                        "wait",
                        "--for=condition=ready",
                        "pod",
                        "--all",
                        "--namespace",
                        manifests::NAMESPACE,
                        "--timeout=300s",
                    ],
                );
                Ok(())
            }
        }
    }

    /// The kubectl context for a cluster role, derived from the plan's cluster
    /// names.
    fn context_for(&self, role: ClusterRole) -> String {
        let cluster = match role {
            ClusterRole::Source => self.state.plan.answers.source_cluster(),
            ClusterRole::Target => self.state.plan.answers.target_cluster(),
        };
        kube_context(&cluster)
    }

    /// Advance the recorded step when an action marks a milestone.
    fn advance_for(&mut self, action: &Action) {
        match action {
            Action::WaitReady {
                role: ClusterRole::Source,
                ..
            } => self.state.advance(Step::SourceUp),
            Action::WaitReady {
                role: ClusterRole::Target,
                ..
            } => self.state.advance(Step::TargetUp),
            Action::ApplyManifest { name, .. } if name == "localstack" => {
                self.state.advance(Step::SnapshotUp)
            }
            Action::ApplyManifest { name, .. } if name == "data-seed" => {
                self.state.advance(Step::DataSeeded)
            }
            Action::ApplyManifest { name, .. }
                if name == "locust" || name == "sample-search-app" =>
            {
                self.state.advance(Step::ClientsUp)
            }
            _ => {}
        }
    }

    /// Write the cloud terraform files into the workspace.
    fn emit_terraform(&mut self, answers: &Answers) -> Result<()> {
        self.progress.step("Emitting Terraform (cloud)");
        for f in terraform::files(answers) {
            self.write_artifact(&f.path, &f.body)?;
            self.progress.info(&format!("wrote {}", f.path));
        }
        self.progress
            .info("review the terraform/ files, then run `terraform init && terraform apply`");
        Ok(())
    }

    /// Install the Migration Assistant CLI from the fork release into the
    /// workspace's `bin/`, returning the launch argv. Local + cloud both end
    /// here — the MA CLI then drives the migration itself.
    pub fn install_ma(&mut self) -> Result<Vec<String>> {
        self.progress.step("Installing Migration Assistant CLI");
        let bin_dir = self.state.dir().join("bin");
        std::fs::create_dir_all(&bin_dir)?;
        let bin_dir_str = bin_dir.to_string_lossy().to_string();
        let cmd = plan::ma_install_command(&bin_dir_str);
        let out = self.runner.run("bash", &["-c", &cmd]);
        if !out.success() {
            return Err(Error::die(format!(
                "Migration Assistant install failed: {}",
                out.stderr.trim()
            )));
        }
        self.state.advance(Step::Ready);
        self.state.save()?;
        self.progress.info(&format!(
            "Migration Assistant {} installed at {}/migration-assistant",
            plan::MA_VERSION,
            bin_dir_str
        ));
        Ok(plan::ma_launch_argv(&bin_dir_str))
    }

    /// Write `name` (relative path) under the workspace with `body`, creating
    /// parent dirs. Returns the absolute path.
    fn write_artifact(&self, name: &str, body: &str) -> Result<PathBuf> {
        let path = self.state.dir().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, body)?;
        Ok(path)
    }

    /// Whether a kind error means the cluster already exists (idempotent
    /// re-run).
    fn already_exists(stderr: &str) -> bool {
        stderr.contains("already exist")
    }

    /// Whether a `kubectl apply` error is an immutable-field rejection — the
    /// signal to recover by delete + re-apply. Jobs (immutable pod template) are
    /// the common case when re-running the harness over an existing deployment.
    fn is_immutable_error(stderr: &str) -> bool {
        stderr.contains("field is immutable")
            || stderr.contains("may not be changed")
            || stderr.contains("updates to") && stderr.contains("are forbidden")
    }
}

fn role_stem(role: ClusterRole) -> &'static str {
    match role {
        ClusterRole::Source => "source",
        ClusterRole::Target => "target",
    }
}

/// Whether `path` looks like a writable workspace (exists or can be created).
/// Small helper used by the dispatcher before constructing an [`App`].
pub fn ensure_workspace(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClientApp, SnapshotStorage, SourceEngine, TargetMode};
    use crate::runner::MockRunner;

    fn full_answers() -> Answers {
        let mut a = Answers::new();
        a.target = Some(Target::Local);
        a.source_engine = Some(SourceEngine::Elasticsearch);
        a.source_version = Some("7.10.2".into());
        a.snapshot_storage = Some(SnapshotStorage::LocalStack);
        a.target_mode = Some(TargetMode::Provision);
        a.target_version = Some("3.3.0".into());
        a.clients = vec![ClientApp::Locust, ClientApp::SampleSearchApp];
        a.seed_data = Some(true);
        a
    }

    /// A mock with all local tools present + docker daemon up.
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
    fn preflight_passes_when_tools_present() {
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        assert!(app.preflight(&full_answers()).is_ok());
        assert_eq!(app.state.plan.step, Step::PreflightDone);
    }

    #[test]
    fn preflight_fails_when_kind_missing() {
        let r = MockRunner::new()
            .with_command("docker")
            .with_command("kubectl")
            .with_command("curl")
            .stub("docker", &["info"], 0, "ok");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let err = app.preflight(&full_answers()).unwrap_err();
        assert!(err.message.contains("kind"));
    }

    #[test]
    fn preflight_fails_when_docker_daemon_down() {
        let r = MockRunner::new()
            .with_command("docker")
            .with_command("kind")
            .with_command("kubectl")
            .with_command("curl")
            .stub("docker", &["info"], 1, "");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let err = app.preflight(&full_answers()).unwrap_err();
        assert!(err.message.contains("daemon"));
    }

    #[test]
    fn provision_local_runs_kind_and_kubectl_in_order() {
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = full_answers();
        app.provision(&full_answers()).unwrap();

        // Source + target clusters created.
        assert!(r.any_call_contains("kind create cluster --name ma-demo-source"));
        assert!(r.any_call_contains("kind create cluster --name ma-demo-target"));
        // Manifests applied to the right contexts.
        assert!(r.any_call_contains("kubectl --context kind-ma-demo-source apply"));
        assert!(r.any_call_contains("kubectl --context kind-ma-demo-target apply"));
        // The recorded step advanced to at least TargetUp.
        assert!(app.state.plan.step.index() >= Step::TargetUp.index());
    }

    #[test]
    fn provision_writes_manifests_to_workspace() {
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = full_answers();
        app.provision(&full_answers()).unwrap();
        // Source manifest written with the ES image.
        let src = std::fs::read_to_string(tmp.path().join("source-source.yaml")).unwrap();
        assert!(src.contains("elasticsearch:7.10.2"));
        // KIND config written.
        assert!(tmp.path().join("kind-ma-demo-source.yaml").exists());
    }

    #[test]
    fn kind_already_exists_is_not_fatal() {
        let r = ready_runner().stub_stderr(
            "kind",
            &["create", "cluster"],
            1,
            "ERROR: node(s) already exist for a cluster with the name \"ma-demo-source\"",
        );
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = full_answers();
        // Should not error despite the kind "already exists" stderr.
        assert!(app.provision(&full_answers()).is_ok());
    }

    #[test]
    fn kubectl_apply_failure_is_fatal() {
        let r = ready_runner().stub_stderr("kubectl", &["apply"], 1, "connection refused");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = full_answers();
        let err = app.provision(&full_answers()).unwrap_err();
        assert!(err.message.contains("kubectl apply"));
    }

    #[test]
    fn immutable_apply_error_recovers_via_delete_then_retry() {
        // The FIRST apply hits an immutable-field error (the Job re-run case);
        // the orchestrator should delete and re-apply rather than hard-fail. The
        // stub fires once, so the retry apply falls through to the default-0
        // reply (success). Every later apply also succeeds by default.
        let r = ready_runner().stub_stderr_once(
            "kubectl",
            &["apply"],
            1,
            "The Job \"data-seed\" is invalid: spec.template: field is immutable",
            1,
        );
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = full_answers();
        // Provision should NOT error — the immutable apply is recovered.
        assert!(app.provision(&full_answers()).is_ok());
        // A delete was issued as part of the recovery.
        assert!(r.any_call_contains("delete -f"));
    }

    #[test]
    fn is_immutable_error_classifies_job_template_rejection() {
        assert!(App::<MockRunner, SilentProgress>::is_immutable_error(
            "spec.template: Invalid value: ...: field is immutable"
        ));
        assert!(!App::<MockRunner, SilentProgress>::is_immutable_error(
            "connection refused"
        ));
    }

    #[test]
    fn cloud_provision_writes_terraform_no_kind() {
        let mut a = full_answers();
        a.target = Some(Target::Cloud);
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = a.clone();
        app.provision(&a).unwrap();
        // No kind calls on the cloud path.
        assert!(!r.any_call_contains("kind create"));
        // Terraform files written.
        assert!(tmp.path().join("terraform/providers.tf").exists());
        assert!(tmp.path().join("terraform/source.tf").exists());
        assert!(tmp.path().join("terraform/target.tf").exists());
    }

    #[test]
    fn install_ma_curls_fork_release_and_returns_launch_argv() {
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let argv = app.install_ma().unwrap();
        assert!(r.any_call_contains("AndreKurait/opensearch-migrations"));
        assert_eq!(argv.len(), 1);
        assert!(argv[0].ends_with("/migration-assistant"));
        assert_eq!(app.state.plan.step, Step::Ready);
    }

    #[test]
    fn install_ma_failure_is_fatal() {
        let r = ready_runner().stub_stderr("bash", &["-c"], 1, "download failed");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let err = app.install_ma().unwrap_err();
        assert!(err.message.contains("install failed"));
    }

    #[test]
    fn kube_context_prefixes_with_kind() {
        assert_eq!(kube_context("ma-demo-source"), "kind-ma-demo-source");
    }
}

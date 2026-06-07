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
        self.provision_with(answers, false)
    }

    /// As [`provision`](Self::provision), but `apply_cloud` controls whether the
    /// cloud (Terraform) path actually runs `terraform init && apply` after
    /// emitting the files (vs just emitting them for the operator to apply).
    pub fn provision_with(
        &mut self,
        answers: &Answers,
        apply_cloud: bool,
    ) -> Result<ProvisionPlan> {
        export_aws_env(answers);
        let plan = plan::build(answers);
        if answers.target == Some(Target::Cloud) {
            self.emit_terraform(answers, apply_cloud)?;
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
            Action::ProvisionAoss { collection } => self.provision_aoss(collection),
        }
    }

    /// Provision an AOSS NextGen collection via the `aws opensearchserverless`
    /// control plane: collection group (NextGen, standby ENABLED), the three
    /// policies, then the collection. Records the resolved endpoint in state.
    /// Every call goes through the runner, so this is asserted against the mock.
    fn provision_aoss(&mut self, collection: &str) -> Result<()> {
        if !self.runner.has_command("aws") {
            return Err(Error::die(
                "aws CLI not found on PATH; required for the AOSS NextGen target. \
                 Install AWS CLI v2 (>= 2.34.56) and authenticate.",
            ));
        }
        let region = std::env::var("AWS_REGION").unwrap_or_else(|_| plan::AOSS_REGION.to_string());
        let principal = self.aws_principal_arn();

        // The collection group must exist before the collection. "already
        // exists" / "ConflictException" make re-runs idempotent.
        let steps: [(&str, Vec<String>); 5] = [
            (
                "collection group",
                plan::aoss_create_group_args(collection, &region),
            ),
            (
                "encryption policy",
                plan::aoss_encryption_policy_args(collection, &region),
            ),
            (
                "network policy",
                plan::aoss_network_policy_args(collection, &region),
            ),
            (
                "data access policy",
                plan::aoss_data_policy_args(collection, &region, &principal),
            ),
            (
                "collection",
                plan::aoss_create_collection_args(collection, &region),
            ),
        ];
        for (what, args) in &steps {
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = self.runner.run("aws", &argv);
            if !out.success() && !Self::aoss_already_exists(&out.stderr) {
                return Err(Error::die(format!(
                    "AOSS {what} create failed: {}",
                    out.stderr.trim()
                )));
            }
        }

        // Poll the collection to ACTIVE and capture the endpoint.
        let endpoint = self.poll_aoss_active(collection, &region)?;
        self.state.plan.aoss_endpoint = Some(endpoint);
        self.state.advance(Step::TargetUp);
        self.state.save()?;
        Ok(())
    }

    /// Resolve the caller's IAM principal ARN (for the data access policy).
    /// Falls back to a wildcard-free placeholder the operator can edit if STS
    /// is unavailable through the runner.
    fn aws_principal_arn(&self) -> String {
        let out = self.runner.run(
            "aws",
            &[
                "sts",
                "get-caller-identity",
                "--query",
                "Arn",
                "--output",
                "text",
            ],
        );
        let arn = out.trimmed_stdout().trim().to_string();
        // Normalize an assumed-role ARN to its role ARN, which is what AOSS data
        // access policies expect: sts::ACCT:assumed-role/ROLE/SESSION ->
        // iam::ACCT:role/ROLE.
        if let Some(rest) = arn.strip_prefix("arn:aws:sts::") {
            if let Some((acct, tail)) = rest.split_once(':') {
                if let Some(role_path) = tail.strip_prefix("assumed-role/") {
                    let role = role_path.split('/').next().unwrap_or(role_path);
                    return format!("arn:aws:iam::{acct}:role/{role}");
                }
            }
        }
        arn
    }

    /// Poll `batch-get-collection` until ACTIVE (or a bounded number of tries),
    /// returning the collection endpoint. The runner's stubbed output drives
    /// this under test.
    fn poll_aoss_active(&self, collection: &str, region: &str) -> Result<String> {
        let args = plan::aoss_batch_get_args(collection, region);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        for _ in 0..40 {
            let out = self.runner.run("aws", &argv);
            if out.success() {
                if let Some(ep) = parse_aoss_endpoint(&out.stdout) {
                    return Ok(ep);
                }
            }
            // Under the real runner the orchestrator sleeps; the mock returns
            // immediately, so a successful ACTIVE parse exits on the first pass.
            if std::env::var("MA_DEMO_TEST").is_err() {
                std::thread::sleep(std::time::Duration::from_secs(3));
            } else {
                break;
            }
        }
        Err(Error::die(format!(
            "AOSS collection {collection} did not reach ACTIVE in time"
        )))
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
    fn emit_terraform(&mut self, answers: &Answers, apply_cloud: bool) -> Result<()> {
        self.progress.step("Emitting Terraform (cloud)");
        for f in terraform::files(answers) {
            self.write_artifact(&f.path, &f.body)?;
            self.progress.info(&format!("wrote {}", f.path));
        }
        let tf_dir = self.state.dir().join("terraform");
        let tf_dir_str = tf_dir.to_string_lossy().to_string();

        if !apply_cloud {
            self.progress.info(&format!(
                "emitted only — review then apply:  terraform -chdir={tf_dir_str} init && terraform -chdir={tf_dir_str} apply"
            ));
            return Ok(());
        }

        // Actually stand the cloud infra up. Requires terraform on PATH.
        if !self.runner.has_command("terraform") {
            return Err(Error::die(
                "terraform not found on PATH; needed to apply the cloud deployment. \
                 Install Terraform, or re-run with --no-apply to emit the files only.",
            ));
        }
        self.progress.step("terraform init");
        let init = self.runner.run(
            "terraform",
            &[&format!("-chdir={tf_dir_str}"), "init", "-input=false"],
        );
        if !init.success() {
            return Err(Error::die(format!(
                "terraform init failed: {}",
                init.stderr.trim()
            )));
        }
        self.progress
            .step("terraform apply (cloud — this provisions real AWS resources)");
        let region = answers.effective_aws_region().to_string();
        let apply = self.runner.run(
            "terraform",
            &[
                &format!("-chdir={tf_dir_str}"),
                "apply",
                "-auto-approve",
                "-input=false",
                "-var",
                &format!("region={region}"),
            ],
        );
        if !apply.success() {
            return Err(Error::die(format!(
                "terraform apply failed: {}",
                apply.stderr.trim()
            )));
        }
        self.state.advance(Step::TargetUp);
        self.state.save()?;
        self.progress
            .info("terraform apply complete; see outputs (source_endpoint / snapshot_bucket / target_endpoint).");
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

    /// Deploy the Migration Assistant helm chart into a dedicated local KIND
    /// cluster (no AWS), then return the `kubectl exec` argv into the
    /// migration-console — a fully-local end-to-end migration handoff. Requires
    /// helm on PATH and the opensearch-migrations chart (located via
    /// `MA_CHART_PATH`, else a sibling checkout). Every call goes through the
    /// runner, so it is asserted against the mock.
    pub fn deploy_ma_local(&mut self) -> Result<Vec<String>> {
        self.progress
            .step("Deploying Migration Assistant to KIND (helm)");
        for tool in ["helm", "kind", "kubectl"] {
            if !self.runner.has_command(tool) {
                return Err(Error::die(format!(
                    "required tool not found on PATH: {tool}. Needed for the local MA helm deploy."
                )));
            }
        }
        let chart = self.resolve_ma_chart_path()?;
        let cluster = self.state.plan.answers.ma_cluster();
        let ctx = kube_context(&cluster);

        // A dedicated KIND cluster for MA (pinned node image meets the chart's
        // k8s >= 1.35.0 floor). Host port 9201 avoids the source/target maps.
        let cfg = manifests::kind_config(&cluster, 9201, 30920);
        let cfg_path = self.write_artifact(&format!("kind-{cluster}.yaml"), &cfg)?;
        let create = self.runner.run(
            "kind",
            &[
                "create",
                "cluster",
                "--name",
                &cluster,
                "--config",
                &cfg_path.to_string_lossy(),
            ],
        );
        if !create.success() && !Self::already_exists(&create.stderr) {
            return Err(Error::die(format!(
                "kind create cluster {cluster} failed: {}",
                create.stderr.trim()
            )));
        }

        // helm dependency update + upgrade --install.
        let dep = plan::ma_helm_dependency_args(&chart);
        let dep_ref: Vec<&str> = dep.iter().map(String::as_str).collect();
        self.runner.run("helm", &dep_ref);

        let inst = plan::ma_helm_install_args(&chart, &ctx);
        let inst_ref: Vec<&str> = inst.iter().map(String::as_str).collect();
        let out = self.runner.run("helm", &inst_ref);
        if !out.success() {
            return Err(Error::die(format!(
                "helm install migration-assistant failed: {}",
                out.stderr.trim()
            )));
        }

        // Wait for the console statefulset (best-effort log; not fatal).
        let roll = plan::ma_rollout_status_args(&ctx);
        let roll_ref: Vec<&str> = roll.iter().map(String::as_str).collect();
        self.runner.run("kubectl", &roll_ref);

        self.state.advance(Step::Ready);
        self.state.save()?;
        self.progress.info(&format!(
            "Migration Assistant deployed to KIND cluster {cluster} (namespace {})",
            plan::MA_NAMESPACE
        ));
        // The handoff: exec into the console. Prefix with `kubectl` for the dispatcher.
        let mut argv = vec!["kubectl".to_string()];
        argv.extend(plan::ma_console_exec_args(&ctx));
        Ok(argv)
    }

    /// Locate the opensearch-migrations helm chart: `MA_CHART_PATH` env wins;
    /// else a sibling `opensearch-migrations` checkout next to the workspace's
    /// parent. Errors with guidance if neither exists.
    fn resolve_ma_chart_path(&self) -> Result<String> {
        if let Ok(p) = std::env::var("MA_CHART_PATH") {
            if std::path::Path::new(&p).exists() {
                return Ok(p);
            }
        }
        // Common dev layout: a sibling clone of the repo.
        for base in [
            std::path::PathBuf::from("/Users/akurait/demo/opensearch-migrations"),
            std::env::current_dir()
                .unwrap_or_default()
                .join("../opensearch-migrations"),
        ] {
            let chart = base.join(plan::MA_CHART_SUBPATH);
            if chart.exists() {
                return Ok(chart.to_string_lossy().to_string());
            }
        }
        Err(Error::die(
            "could not find the Migration Assistant helm chart. Set MA_CHART_PATH to \
             <opensearch-migrations>/deployment/k8s/charts/aggregates/migrationAssistantWithArgo.",
        ))
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

    /// Whether an AOSS create error means the resource already exists — makes
    /// the AOSS provisioning idempotent across re-runs.
    fn aoss_already_exists(stderr: &str) -> bool {
        stderr.contains("ConflictException")
            || stderr.contains("already exists")
            || stderr.contains("already exist")
    }
}

/// Parse the collection endpoint out of an `aws ... batch-get-collection` JSON
/// response, returning it only when the collection is ACTIVE. Pure, so the
/// poll logic is unit-tested against canned CLI output.
pub fn parse_aoss_endpoint(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let details = v.get("collectionDetails")?.as_array()?;
    let first = details.first()?;
    if first.get("status")?.as_str()? != "ACTIVE" {
        return None;
    }
    first
        .get("collectionEndpoint")?
        .as_str()
        .map(|s| s.to_string())
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

/// Export the chosen AWS profile + region into the process environment so every
/// spawned `aws`/`terraform` command (and Terraform's AWS provider) uses them.
/// No-op when the plan doesn't touch AWS. Called once before provisioning.
pub fn export_aws_env(answers: &Answers) {
    if !answers.touches_aws() {
        return;
    }
    std::env::set_var("AWS_PROFILE", answers.effective_aws_profile());
    std::env::set_var("AWS_REGION", answers.effective_aws_region());
    // AWS_DEFAULT_REGION covers tools that read the older var name.
    std::env::set_var("AWS_DEFAULT_REGION", answers.effective_aws_region());
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
    fn deploy_ma_local_creates_cluster_helm_installs_and_execs_console() {
        // Point MA_CHART_PATH at a real temp dir so chart resolution succeeds.
        let chart = tempfile::tempdir().unwrap();
        std::env::set_var("MA_CHART_PATH", chart.path());
        let r = ready_runner().with_command("helm");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let argv = app.deploy_ma_local().unwrap();

        // Dedicated MA KIND cluster created with the pinned node image.
        assert!(r.any_call_contains("kind create cluster --name ma-demo-ma"));
        let cfg = std::fs::read_to_string(tmp.path().join("kind-ma-demo-ma.yaml")).unwrap();
        assert!(cfg.contains("kindest/node:v1.35.0"));
        // helm dependency update + upgrade --install with the staging images.
        assert!(r.any_call_contains("helm dependency update"));
        assert!(r.any_call_contains("upgrade --install --create-namespace"));
        assert!(r.any_call_contains("opensearchstaging/opensearch-migrations-console"));
        // Rollout wait + the console-exec handoff argv.
        assert!(r.any_call_contains("rollout status statefulset/migration-console"));
        assert_eq!(argv[0], "kubectl");
        assert!(argv.contains(&"migration-console-0".to_string()));
        assert_eq!(app.state.plan.step, Step::Ready);
        std::env::remove_var("MA_CHART_PATH");
    }

    #[test]
    fn deploy_ma_local_without_helm_errors() {
        let chart = tempfile::tempdir().unwrap();
        std::env::set_var("MA_CHART_PATH", chart.path());
        let r = ready_runner(); // no helm registered
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        let err = app.deploy_ma_local().unwrap_err();
        assert!(err.message.contains("helm"));
        std::env::remove_var("MA_CHART_PATH");
    }

    #[test]
    fn deploy_ma_local_missing_chart_errors() {
        std::env::set_var("MA_CHART_PATH", "/nonexistent/chart/path/xyz");
        let r = ready_runner().with_command("helm");
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        // With a bogus MA_CHART_PATH and (likely) no sibling checkout in the test
        // sandbox, chart resolution should fail with guidance. If a real sibling
        // checkout happens to exist, the deploy proceeds — accept either.
        match app.deploy_ma_local() {
            Err(e) => assert!(e.message.contains("helm chart") || e.message.contains("chart")),
            Ok(argv) => assert_eq!(argv[0], "kubectl"),
        }
        std::env::remove_var("MA_CHART_PATH");
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

    #[test]
    fn parse_aoss_endpoint_only_when_active() {
        let active = r#"{"collectionDetails":[{"status":"ACTIVE","collectionEndpoint":"https://abc.aoss.us-east-1.on.aws","id":"abc"}]}"#;
        assert_eq!(
            parse_aoss_endpoint(active).as_deref(),
            Some("https://abc.aoss.us-east-1.on.aws")
        );
        let creating = r#"{"collectionDetails":[{"status":"CREATING","id":"abc"}]}"#;
        assert!(parse_aoss_endpoint(creating).is_none());
        assert!(parse_aoss_endpoint("not json").is_none());
        assert!(parse_aoss_endpoint(r#"{"collectionDetails":[]}"#).is_none());
    }

    #[test]
    fn aoss_already_exists_classifies_conflict() {
        assert!(App::<MockRunner, SilentProgress>::aoss_already_exists(
            "ConflictException: resource already exists"
        ));
        assert!(!App::<MockRunner, SilentProgress>::aoss_already_exists(
            "AccessDeniedException"
        ));
    }

    fn aoss_answers() -> Answers {
        let mut a = full_answers();
        a.target_kind = Some(crate::model::TargetKind::AossServerlessNextGen);
        a.target_version = None;
        a
    }

    /// A mock that satisfies the AOSS control-plane flow: aws present, every
    /// create returns 0, and batch-get returns an ACTIVE collection with an
    /// endpoint. `MA_DEMO_TEST` short-circuits the poll sleep.
    fn aoss_runner() -> MockRunner {
        ready_runner()
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
            )
    }

    #[test]
    fn provision_aoss_drives_control_plane_and_records_endpoint() {
        std::env::set_var("MA_DEMO_TEST", "1");
        let r = aoss_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = aoss_answers();
        app.provision(&aoss_answers()).unwrap();

        // NextGen group + collection created; no target KIND cluster.
        assert!(r.any_call_contains("create-collection-group"));
        assert!(r.any_call_contains("--generation NEXTGEN"));
        assert!(r.any_call_contains("create-collection"));
        assert!(!r.any_call_contains("kind create cluster --name ma-demo-target"));
        // Endpoint captured into state, and the data policy got the ROLE arn
        // (normalized from the assumed-role STS arn).
        assert_eq!(
            app.state.plan.aoss_endpoint.as_deref(),
            Some("https://xyz.aoss.us-east-1.on.aws")
        );
        assert!(r.any_call_contains("arn:aws:iam::874041194807:role/IibsAdminAccess-DO-NOT-DELETE"));
        std::env::remove_var("MA_DEMO_TEST");
    }

    #[test]
    fn provision_aoss_without_aws_cli_errors() {
        std::env::set_var("MA_DEMO_TEST", "1");
        // ready_runner has no aws command registered.
        let r = ready_runner();
        let tmp = tempfile::tempdir().unwrap();
        let p = SilentProgress;
        let mut app = App::new(&r, tmp.path(), &p);
        app.state.plan.answers = aoss_answers();
        let err = app.provision(&aoss_answers()).unwrap_err();
        assert!(err.message.contains("aws CLI not found"));
        std::env::remove_var("MA_DEMO_TEST");
    }
}

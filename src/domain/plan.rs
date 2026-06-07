//! The provisioning planner — turns [`Answers`] into an ordered, typed list of
//! actions, plus the Migration Assistant install + launch handoff.
//!
//! Pure: a [`ProvisionPlan`] is a complete description of *what* the harness
//! will do, with every artifact body already rendered. The orchestrator
//! ([`crate::command::app`]) walks the plan and performs the I/O through the
//! [`CommandRunner`] seam. Because the plan is data, the exact action sequence
//! for any set of answers is asserted in unit tests with no Docker.

use crate::manifests;
use crate::model::{Answers, SnapshotStorage, SourceEngine, Target};

/// The fork release the harness installs the Migration Assistant CLI from.
pub const MA_REPO: &str = "AndreKurait/opensearch-migrations";
/// The release tag whose artifacts this harness was built against.
pub const MA_VERSION: &str = "3.3.1";

/// Which KIND cluster an action runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterRole {
    Source,
    Target,
}

/// One provisioning action. The orchestrator matches on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create a KIND cluster with the given name and rendered kind-config YAML.
    CreateKindCluster {
        name: String,
        config: String,
        role: ClusterRole,
    },
    /// Write a manifest file and `kubectl apply` it to a cluster's context.
    ApplyManifest {
        /// The cluster context to apply against.
        role: ClusterRole,
        /// A short filename stem (e.g. "source", "localstack").
        name: String,
        /// The rendered YAML body.
        body: String,
    },
    /// Wait for a rollout/condition (a kubectl wait) — described for the user;
    /// the orchestrator decides the exact wait args.
    WaitReady { role: ClusterRole, what: String },
    /// Provision an Amazon OpenSearch Serverless NextGen collection as the
    /// target (cloud). The orchestrator drives the `aws opensearchserverless`
    /// control-plane calls; `collection` is the collection name.
    ProvisionAoss { collection: String },
}

impl Action {
    /// A one-line human description for the activity log / dry-run.
    pub fn describe(&self) -> String {
        match self {
            Action::CreateKindCluster { name, role, .. } => {
                format!("create KIND cluster {name} ({})", role_label(*role))
            }
            Action::ApplyManifest { role, name, .. } => {
                format!("apply {name} → {} cluster", role_label(*role))
            }
            Action::WaitReady { role, what } => {
                format!("wait for {what} ({})", role_label(*role))
            }
            Action::ProvisionAoss { collection } => {
                format!("provision AOSS NextGen collection {collection}")
            }
        }
    }
}

fn role_label(r: ClusterRole) -> &'static str {
    match r {
        ClusterRole::Source => "source",
        ClusterRole::Target => "target",
    }
}

/// The complete provisioning plan derived from answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionPlan {
    pub actions: Vec<Action>,
    /// The in-cluster DNS host of the source service (what clients + MA target).
    pub source_host: String,
    /// The in-cluster DNS host of the target service, when one is provisioned.
    pub target_host: Option<String>,
}

/// The in-cluster service name for the chosen source engine.
pub fn source_service_name(engine: SourceEngine) -> &'static str {
    match engine {
        SourceEngine::Elasticsearch => "source-elasticsearch",
        SourceEngine::OpenSearch => "source-opensearch",
        SourceEngine::Solr => "source-solr",
    }
}

/// Build the full local (KIND) provisioning plan from answers. Cloud plans emit
/// terraform instead (see [`crate::terraform`]) and produce an empty action
/// list here.
pub fn build(answers: &Answers) -> ProvisionPlan {
    let engine = answers.source_engine.unwrap_or(SourceEngine::Elasticsearch);
    let source_host = source_service_name(engine).to_string();
    let mut actions = Vec::new();
    let mut target_host = None;

    if answers.target != Some(Target::Local) {
        // Cloud (or unset): no KIND actions; terraform carries the plan.
        return ProvisionPlan {
            actions,
            source_host,
            target_host,
        };
    }

    // ---- source cluster ----
    let src_cluster = answers.source_cluster();
    actions.push(Action::CreateKindCluster {
        name: src_cluster.clone(),
        config: manifests::kind_config(&src_cluster, 19200, source_node_port(engine)),
        role: ClusterRole::Source,
    });
    actions.push(Action::ApplyManifest {
        role: ClusterRole::Source,
        name: "namespace".into(),
        body: manifests::namespace(),
    });
    // Snapshot storage (LocalStack) goes up before the source so the source can
    // register the repo against it.
    if answers.snapshot_storage == Some(SnapshotStorage::LocalStack) {
        actions.push(Action::ApplyManifest {
            role: ClusterRole::Source,
            name: "localstack".into(),
            body: manifests::localstack(),
        });
    }
    actions.push(Action::ApplyManifest {
        role: ClusterRole::Source,
        name: "source".into(),
        body: manifests::source(answers),
    });
    actions.push(Action::WaitReady {
        role: ClusterRole::Source,
        what: "source cluster".into(),
    });

    // ---- client apps (against the source) ----
    for (name, body) in manifests::client_manifests(answers, &source_host) {
        actions.push(Action::ApplyManifest {
            role: ClusterRole::Source,
            name: name.into(),
            body,
        });
    }

    // ---- sample data seed (against the source) ----
    if answers.seed_data == Some(true) {
        actions.push(Action::ApplyManifest {
            role: ClusterRole::Source,
            name: "data-seed".into(),
            body: manifests::data_seed_job(&source_host),
        });
    }

    // ---- target (optional) — kind depends on the chosen target_kind ----
    if answers.provisions_local_target() {
        // Local OpenSearch in its own KIND cluster.
        let tgt_cluster = answers.target_cluster();
        let ver = answers.target_version.as_deref().unwrap_or("3.3.0");
        actions.push(Action::CreateKindCluster {
            name: tgt_cluster.clone(),
            config: manifests::kind_config(&tgt_cluster, 29200, 30920),
            role: ClusterRole::Target,
        });
        actions.push(Action::ApplyManifest {
            role: ClusterRole::Target,
            name: "namespace".into(),
            body: manifests::namespace(),
        });
        actions.push(Action::ApplyManifest {
            role: ClusterRole::Target,
            name: "target".into(),
            body: manifests::target(ver),
        });
        actions.push(Action::WaitReady {
            role: ClusterRole::Target,
            what: "target cluster".into(),
        });
        target_host = Some("target-opensearch".to_string());
    } else if answers.provisions_aoss_target() {
        // Cloud AOSS NextGen collection — no KIND cluster; the orchestrator
        // drives the control-plane via `aws opensearchserverless`. The endpoint
        // is only known after creation, so target_host stays None here.
        actions.push(Action::ProvisionAoss {
            collection: answers.aoss_collection_name(),
        });
    }

    ProvisionPlan {
        actions,
        source_host,
        target_host,
    }
}

/// The NodePort the source service publishes (Solr uses 30983, ES/OS 30920).
fn source_node_port(engine: SourceEngine) -> u16 {
    match engine {
        SourceEngine::Solr => 30983,
        _ => 30920,
    }
}

/// The shell command that installs the Migration Assistant CLI from the fork's
/// release into `bin_dir`, with no Rust toolchain required. Returns the full
/// `bash -c` script. `MIGRATE_INSTALL=path` is the non-launching install mode.
pub fn ma_install_command(bin_dir: &str) -> String {
    format!(
        "set -e; \
         curl -fsSL https://github.com/{MA_REPO}/releases/download/{MA_VERSION}/install.sh -o /tmp/ma-install.sh; \
         MIGRATE_INSTALL=path MIGRATE_VERSION={MA_VERSION} BIN_DIR={bin_dir} bash /tmp/ma-install.sh"
    )
}

/// The argv the harness execs to launch the installed Migration Assistant CLI.
/// The operator takes over from here.
pub fn ma_launch_argv(bin_dir: &str) -> Vec<String> {
    vec![format!("{bin_dir}/migration-assistant")]
}

// ---------------------------------------------------------------------------
// AOSS NextGen target — control-plane command sequences (pure data).
//
// These mirror the verified live flow: a NextGen collection group (standby
// ENABLED), the three security/access policies, then the collection. The
// orchestrator runs each through the CommandRunner seam and polls for ACTIVE.
// Pure builders so the exact argv is asserted without touching AWS.
// ---------------------------------------------------------------------------

/// The default region for the AOSS target. Overridable by the orchestrator via
/// the `AWS_REGION` env at run time; baked here for the command builders.
pub const AOSS_REGION: &str = "us-east-1";

/// The collection group name for a given collection (NextGen groups collections).
pub fn aoss_group_name(collection: &str) -> String {
    format!("cg-{collection}")
}

/// `aws opensearchserverless create-collection-group …` argv (NextGen, standby
/// ENABLED — both required for a genuine NextGen group).
pub fn aoss_create_group_args(collection: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "create-collection-group".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        aoss_group_name(collection),
        "--standby-replicas".into(),
        "ENABLED".into(),
        "--generation".into(),
        "NEXTGEN".into(),
    ]
}

/// The encryption-policy create argv (AWS-owned key, scoped to the collection).
pub fn aoss_encryption_policy_args(collection: &str, region: &str) -> Vec<String> {
    let policy = format!(
        r#"{{"Rules":[{{"ResourceType":"collection","Resource":["collection/{collection}"]}}],"AWSOwnedKey":true}}"#
    );
    vec![
        "opensearchserverless".into(),
        "create-security-policy".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        format!("{collection}-enc"),
        "--type".into(),
        "encryption".into(),
        "--policy".into(),
        policy,
    ]
}

/// The network-policy create argv (public access for the demo target).
pub fn aoss_network_policy_args(collection: &str, region: &str) -> Vec<String> {
    let policy = format!(
        r#"[{{"Rules":[{{"ResourceType":"dashboard","Resource":["collection/{collection}"]}},{{"ResourceType":"collection","Resource":["collection/{collection}"]}}],"AllowFromPublic":true}}]"#
    );
    vec![
        "opensearchserverless".into(),
        "create-security-policy".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        format!("{collection}-net"),
        "--type".into(),
        "network".into(),
        "--policy".into(),
        policy,
    ]
}

/// The data-access-policy create argv granting the migration principal the
/// data-plane perms it needs (collection + index resources).
pub fn aoss_data_policy_args(collection: &str, region: &str, principal_arn: &str) -> Vec<String> {
    let policy = format!(
        r#"[{{"Rules":[{{"ResourceType":"collection","Resource":["collection/{collection}"],"Permission":["aoss:CreateCollectionItems","aoss:DeleteCollectionItems","aoss:UpdateCollectionItems","aoss:DescribeCollectionItems"]}},{{"ResourceType":"index","Resource":["index/{collection}/*"],"Permission":["aoss:CreateIndex","aoss:DeleteIndex","aoss:UpdateIndex","aoss:DescribeIndex","aoss:ReadDocument","aoss:WriteDocument"]}}],"Principal":["{principal_arn}"]}}]"#
    );
    vec![
        "opensearchserverless".into(),
        "create-access-policy".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        format!("{collection}-data"),
        "--type".into(),
        "data".into(),
        "--policy".into(),
        policy,
    ]
}

/// The collection create argv (SEARCH type, NextGen group, standby ENABLED).
pub fn aoss_create_collection_args(collection: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "create-collection".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        collection.into(),
        "--type".into(),
        "SEARCH".into(),
        "--collection-group-name".into(),
        aoss_group_name(collection),
        "--standby-replicas".into(),
        "ENABLED".into(),
    ]
}

/// The batch-get argv used to poll the collection to ACTIVE + read its endpoint.
pub fn aoss_batch_get_args(collection: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "batch-get-collection".into(),
        "--region".into(),
        region.into(),
        "--names".into(),
        collection.into(),
    ]
}

/// `delete-collection` argv (needs the collection ID, not the name).
pub fn aoss_delete_collection_args(collection_id: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "delete-collection".into(),
        "--region".into(),
        region.into(),
        "--id".into(),
        collection_id.into(),
    ]
}

/// `delete-security-policy` argv for one policy type (encryption|network).
pub fn aoss_delete_security_policy_args(name: &str, ptype: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "delete-security-policy".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        name.into(),
        "--type".into(),
        ptype.into(),
    ]
}

/// `delete-access-policy` argv for the data policy.
pub fn aoss_delete_access_policy_args(name: &str, region: &str) -> Vec<String> {
    vec![
        "opensearchserverless".into(),
        "delete-access-policy".into(),
        "--region".into(),
        region.into(),
        "--name".into(),
        name.into(),
        "--type".into(),
        "data".into(),
    ]
}

// ---------------------------------------------------------------------------
// Local Migration Assistant deploy (helm into a dedicated KIND cluster) —
// command sequences (pure data). Mirrors the upstream release CI's deploy:
//   helm dependency update <chart>
//   helm --kube-context <ctx> upgrade --install --create-namespace -n <ns> <rel> <chart> -f values
//   kubectl rollout status statefulset/migration-console
// The orchestrator runs each through the CommandRunner seam.
// ---------------------------------------------------------------------------

/// The MA helm release name + namespace used by the upstream deploy.
pub const MA_RELEASE: &str = "ma";
pub const MA_NAMESPACE: &str = "ma";
/// The chart subpath within the opensearch-migrations repo checkout.
pub const MA_CHART_SUBPATH: &str = "deployment/k8s/charts/aggregates/migrationAssistantWithArgo";
/// The published image-repository prefix the chart pulls MA images from.
pub const MA_IMAGE_PREFIX: &str = "opensearchstaging";

/// `helm dependency update <chart>` argv.
pub fn ma_helm_dependency_args(chart_path: &str) -> Vec<String> {
    vec!["dependency".into(), "update".into(), chart_path.into()]
}

/// `helm upgrade --install …` argv for the MA chart against a kube context,
/// with the image prefix + tag set via `--set` (so no values file is needed).
pub fn ma_helm_install_args(chart_path: &str, kube_context: &str) -> Vec<String> {
    vec![
        "--kube-context".into(),
        kube_context.into(),
        "upgrade".into(),
        "--install".into(),
        "--create-namespace".into(),
        "-n".into(),
        MA_NAMESPACE.into(),
        MA_RELEASE.into(),
        chart_path.into(),
        "--timeout".into(),
        "15m".into(),
        "--set".into(),
        format!(
            "images.migrationConsole.repository={MA_IMAGE_PREFIX}/opensearch-migrations-console"
        ),
        "--set".into(),
        format!("images.migrationConsole.tag={MA_VERSION}"),
        "--set".into(),
        format!("images.installer.repository={MA_IMAGE_PREFIX}/opensearch-migrations-console"),
        "--set".into(),
        format!("images.installer.tag={MA_VERSION}"),
    ]
}

/// `kubectl rollout status statefulset/migration-console` argv.
pub fn ma_rollout_status_args(kube_context: &str) -> Vec<String> {
    vec![
        "--context".into(),
        kube_context.into(),
        "-n".into(),
        MA_NAMESPACE.into(),
        "rollout".into(),
        "status".into(),
        "statefulset/migration-console".into(),
        "--timeout".into(),
        "15m".into(),
    ]
}

/// The argv to `kubectl exec` into the migration-console (the local handoff
/// equivalent of the CLI's `console` subcommand).
pub fn ma_console_exec_args(kube_context: &str) -> Vec<String> {
    vec![
        "--context".into(),
        kube_context.into(),
        "exec".into(),
        "--stdin".into(),
        "--tty".into(),
        "--namespace".into(),
        MA_NAMESPACE.into(),
        "migration-console-0".into(),
        "--".into(),
        "/bin/bash".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClientApp, SnapshotStorage, TargetKind, TargetMode};

    fn base() -> Answers {
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

    fn describes(plan: &ProvisionPlan) -> Vec<String> {
        plan.actions.iter().map(|a| a.describe()).collect()
    }

    #[test]
    fn full_local_plan_has_expected_ordered_actions() {
        let plan = build(&base());
        let d = describes(&plan);
        // Source cluster first, then namespace, localstack, source, wait.
        assert_eq!(d[0], "create KIND cluster ma-demo-source (source)");
        assert!(d.contains(&"apply namespace → source cluster".to_string()));
        assert!(d.contains(&"apply localstack → source cluster".to_string()));
        assert!(d.contains(&"apply source → source cluster".to_string()));
        // Clients + seed.
        assert!(d.contains(&"apply locust → source cluster".to_string()));
        assert!(d.contains(&"apply sample-search-app → source cluster".to_string()));
        assert!(d.contains(&"apply data-seed → source cluster".to_string()));
        // Target cluster created after source work.
        assert!(d.contains(&"create KIND cluster ma-demo-target (target)".to_string()));
        assert!(d.contains(&"apply target → target cluster".to_string()));
        assert_eq!(plan.source_host, "source-elasticsearch");
        assert_eq!(plan.target_host.as_deref(), Some("target-opensearch"));
    }

    #[test]
    fn localstack_applied_before_source() {
        let plan = build(&base());
        let names: Vec<&str> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::ApplyManifest { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let ls = names.iter().position(|n| *n == "localstack").unwrap();
        let src = names.iter().position(|n| *n == "source").unwrap();
        assert!(ls < src, "localstack must apply before source");
    }

    #[test]
    fn leave_to_ma_omits_target_cluster() {
        let mut a = base();
        a.target_mode = Some(TargetMode::LeaveToMa);
        a.target_version = None;
        let plan = build(&a);
        let d = describes(&plan);
        assert!(!d.iter().any(|s| s.contains("target cluster")));
        assert!(plan.target_host.is_none());
    }

    #[test]
    fn aoss_target_emits_provision_action_not_a_kind_cluster() {
        let mut a = base();
        a.target_kind = Some(TargetKind::AossServerlessNextGen);
        a.target_version = None;
        let plan = build(&a);
        let d = describes(&plan);
        // No SECOND (target) KIND cluster — the source one still exists.
        assert!(!d
            .iter()
            .any(|s| s.contains("create KIND cluster ma-demo-target")));
        // A ProvisionAoss action is present for the collection.
        assert!(d
            .iter()
            .any(|s| s.contains("provision AOSS NextGen collection ma-demo-target")));
        // The endpoint isn't known until creation, so target_host stays None.
        assert!(plan.target_host.is_none());
        assert!(matches!(
            plan.actions.last(),
            Some(Action::ProvisionAoss { .. })
        ));
    }

    #[test]
    fn local_kind_target_still_builds_second_cluster() {
        let mut a = base();
        a.target_kind = Some(TargetKind::KindOpenSearch);
        let plan = build(&a);
        let d = describes(&plan);
        assert!(d
            .iter()
            .any(|s| s.contains("create KIND cluster ma-demo-target")));
        assert!(!d.iter().any(|s| s.contains("AOSS")));
        assert_eq!(plan.target_host.as_deref(), Some("target-opensearch"));
    }

    #[test]
    fn aoss_command_builders_match_verified_live_flow() {
        let g = aoss_create_group_args("ma-demo-target", "us-east-1");
        assert!(g.contains(&"create-collection-group".to_string()));
        assert!(g.contains(&"NEXTGEN".to_string()));
        assert!(g.contains(&"ENABLED".to_string()));
        let c = aoss_create_collection_args("ma-demo-target", "us-east-1");
        assert!(c.contains(&"SEARCH".to_string()));
        assert!(c.contains(&"cg-ma-demo-target".to_string()));
        let dp = aoss_data_policy_args("ma-demo-target", "us-east-1", "arn:aws:iam::1:role/R");
        assert!(dp.iter().any(|s| s.contains("aoss:WriteDocument")));
        assert!(dp.iter().any(|s| s.contains("arn:aws:iam::1:role/R")));
    }

    #[test]
    fn no_snapshot_storage_omits_localstack() {
        let mut a = base();
        a.snapshot_storage = Some(SnapshotStorage::None);
        let plan = build(&a);
        assert!(!describes(&plan).iter().any(|s| s.contains("localstack")));
    }

    #[test]
    fn no_clients_no_seed_minimal_plan() {
        let mut a = base();
        a.clients = vec![];
        a.seed_data = Some(false);
        a.target_mode = Some(TargetMode::LeaveToMa);
        a.snapshot_storage = Some(SnapshotStorage::None);
        let plan = build(&a);
        let d = describes(&plan);
        assert!(!d.iter().any(|s| s.contains("locust")));
        assert!(!d.iter().any(|s| s.contains("data-seed")));
        assert!(!d.iter().any(|s| s.contains("target")));
        // Still has: create source, namespace, source, wait.
        assert!(d
            .iter()
            .any(|s| s.contains("create KIND cluster ma-demo-source")));
    }

    #[test]
    fn solr_source_uses_solr_service_and_node_port() {
        let mut a = base();
        a.source_engine = Some(SourceEngine::Solr);
        a.source_version = Some("9.7.0".into());
        let plan = build(&a);
        assert_eq!(plan.source_host, "source-solr");
        // The source kind cluster maps the solr node port.
        match &plan.actions[0] {
            Action::CreateKindCluster { config, .. } => {
                assert!(config.contains("containerPort: 30983"));
            }
            _ => panic!("first action should create the source cluster"),
        }
    }

    #[test]
    fn cloud_target_produces_no_kind_actions() {
        let mut a = base();
        a.target = Some(Target::Cloud);
        let plan = build(&a);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn ma_install_command_points_at_fork_release() {
        let cmd = ma_install_command("/home/u/.local/bin");
        assert!(cmd.contains("AndreKurait/opensearch-migrations"));
        assert!(cmd.contains("releases/download/3.3.1/install.sh"));
        assert!(cmd.contains("BIN_DIR=/home/u/.local/bin"));
        assert!(cmd.contains("MIGRATE_INSTALL=path"));
    }

    #[test]
    fn ma_launch_argv_targets_installed_binary() {
        let argv = ma_launch_argv("/home/u/.local/bin");
        assert_eq!(
            argv,
            vec!["/home/u/.local/bin/migration-assistant".to_string()]
        );
    }
}

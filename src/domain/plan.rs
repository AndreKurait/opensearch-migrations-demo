//! The provisioning planner — turns [`Answers`] into an ordered, typed list of
//! actions, plus the Migration Assistant install + launch handoff.
//!
//! Pure: a [`ProvisionPlan`] is a complete description of *what* the harness
//! will do, with every artifact body already rendered. The orchestrator
//! ([`crate::command::app`]) walks the plan and performs the I/O through the
//! [`CommandRunner`] seam. Because the plan is data, the exact action sequence
//! for any set of answers is asserted in unit tests with no Docker.

use crate::manifests;
use crate::model::{Answers, SnapshotStorage, SourceEngine, Target, TargetMode};

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

    // ---- target cluster (optional) ----
    if answers.target_mode == Some(TargetMode::Provision) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClientApp, SnapshotStorage};

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

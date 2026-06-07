//! The harness data model — the answers the wizard collects and the typed enums
//! that drive provisioning.
//!
//! Everything here is plain data with `serde` derives so the collected plan can
//! be persisted to `plan.json` and re-loaded on resume. The wizard mutates an
//! [`Answers`] value; the provisioning planners (kind / clusters / clients /
//! terraform) are pure functions *over* an [`Answers`].

use serde::{Deserialize, Serialize};

/// Where the test environment is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Local Docker via KIND (one cluster per role).
    Local,
    /// AWS via emitted (untested) Terraform.
    Cloud,
}

impl Target {
    pub fn id(self) -> &'static str {
        match self {
            Target::Local => "local",
            Target::Cloud => "cloud",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Target::Local => "Local (KIND / Docker)",
            Target::Cloud => "Cloud (AWS / Terraform)",
        }
    }
}

/// The source search engine to migrate FROM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceEngine {
    Elasticsearch,
    OpenSearch,
    Solr,
}

impl SourceEngine {
    pub fn id(self) -> &'static str {
        match self {
            SourceEngine::Elasticsearch => "elasticsearch",
            SourceEngine::OpenSearch => "opensearch",
            SourceEngine::Solr => "solr",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SourceEngine::Elasticsearch => "Elasticsearch",
            SourceEngine::OpenSearch => "OpenSearch",
            SourceEngine::Solr => "Solr",
        }
    }

    /// The selectable versions for this engine, newest-typical first. These are
    /// the versions the Migration Assistant documents as supported sources and
    /// for which public container images exist.
    pub fn versions(self) -> &'static [&'static str] {
        match self {
            // ES images live on docker.elastic.co; these are the common
            // migration-source majors the MA test matrix exercises.
            SourceEngine::Elasticsearch => &["8.17.0", "7.17.22", "7.10.2", "6.8.23", "5.6.16"],
            // OS source images on docker hub / public.ecr.aws.
            SourceEngine::OpenSearch => &["2.19.0", "2.15.0", "1.3.20"],
            // Solr is backfill-only; 8 and 9 are the dev-sandbox versions.
            SourceEngine::Solr => &["9.7.0", "8.11.3"],
        }
    }

    /// HTTP-search engines (ES/OS) share the 9200 REST API + snapshot model;
    /// Solr does not. Several planning branches key off this.
    pub fn is_http_search(self) -> bool {
        matches!(self, SourceEngine::Elasticsearch | SourceEngine::OpenSearch)
    }
}

/// Where snapshots (the backfill source-of-truth) are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotStorage {
    /// LocalStack S3 in-cluster (no real AWS).
    LocalStack,
    /// A real AWS S3 bucket (operator supplies credentials).
    AwsS3,
    /// Don't provision snapshot storage — leave it to the Migration Assistant.
    None,
}

impl SnapshotStorage {
    pub fn id(self) -> &'static str {
        match self {
            SnapshotStorage::LocalStack => "localstack",
            SnapshotStorage::AwsS3 => "aws-s3",
            SnapshotStorage::None => "none",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SnapshotStorage::LocalStack => "LocalStack (simulated S3, no AWS)",
            SnapshotStorage::AwsS3 => "Real AWS S3 bucket",
            SnapshotStorage::None => "None (let Migration Assistant decide)",
        }
    }
}

/// Whether/how the target OpenSearch cluster is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetMode {
    /// Provision a target OpenSearch cluster as part of the harness.
    Provision,
    /// Provision nothing — the Migration Assistant will stand up / point at its
    /// own target.
    LeaveToMa,
}

impl TargetMode {
    pub fn id(self) -> &'static str {
        match self {
            TargetMode::Provision => "provision",
            TargetMode::LeaveToMa => "leave-to-ma",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            TargetMode::Provision => "Provision a migration target",
            TargetMode::LeaveToMa => "Leave the target to the Migration Assistant",
        }
    }
}

/// Which kind of target to provision (only asked when `TargetMode::Provision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetKind {
    /// A self-managed OpenSearch cluster in a local KIND cluster.
    KindOpenSearch,
    /// An Amazon OpenSearch Serverless NextGen collection (cloud, scale-to-zero,
    /// ~5s to ACTIVE). Stood up via the `aws opensearchserverless` CLI.
    AossServerlessNextGen,
}

impl TargetKind {
    pub fn id(self) -> &'static str {
        match self {
            TargetKind::KindOpenSearch => "kind-opensearch",
            TargetKind::AossServerlessNextGen => "aoss-nextgen",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            TargetKind::KindOpenSearch => "Local OpenSearch (KIND cluster)",
            TargetKind::AossServerlessNextGen => {
                "Amazon OpenSearch Serverless — NextGen collection (cloud, fast)"
            }
        }
    }
    /// Whether this kind needs the local KIND target cluster stood up.
    pub fn is_local(self) -> bool {
        matches!(self, TargetKind::KindOpenSearch)
    }
}

/// The selectable target OpenSearch versions, newest first.
pub const TARGET_VERSIONS: &[&str] = &["3.3.0", "3.1.0", "2.19.0"];

/// A client application to deploy against the source (generates load / traffic
/// so the migration has something live to capture + replay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientApp {
    /// A Locust deployment driving continuous index+search load.
    Locust,
    /// A small always-on sample app issuing representative queries.
    SampleSearchApp,
}

impl ClientApp {
    pub fn id(self) -> &'static str {
        match self {
            ClientApp::Locust => "locust",
            ClientApp::SampleSearchApp => "sample-search-app",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ClientApp::Locust => "Locust load generator",
            ClientApp::SampleSearchApp => "Sample search application",
        }
    }
    /// All client apps, in display order.
    pub const ALL: [ClientApp; 2] = [ClientApp::Locust, ClientApp::SampleSearchApp];
}

/// The full set of answers the wizard collects. Optional fields are `None`
/// until the corresponding question is answered (so a partially-completed plan
/// round-trips through `plan.json`). The derived `Default` is all-unset/empty,
/// which is exactly the fresh-plan starting state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Answers {
    pub target: Option<Target>,
    pub source_engine: Option<SourceEngine>,
    pub source_version: Option<String>,
    /// Engine plugins to install on the source (e.g. analysis-icu, repository-s3).
    pub source_plugins: Vec<String>,
    /// True once the plugins question has been answered. Needed because an empty
    /// plugin list is a legal answer, so emptiness can't mean "unanswered".
    #[serde(default)]
    pub plugins_done: bool,
    pub snapshot_storage: Option<SnapshotStorage>,
    pub target_mode: Option<TargetMode>,
    /// Which kind of target to provision (only set when `target_mode` is
    /// `Provision`). `None` while unasked or when leaving the target to MA.
    #[serde(default)]
    pub target_kind: Option<TargetKind>,
    pub target_version: Option<String>,
    /// Client apps to deploy against the source.
    pub clients: Vec<ClientApp>,
    /// True once the clients question has been answered (empty is legal — see
    /// `plugins_done`).
    #[serde(default)]
    pub clients_done: bool,
    /// Whether to pre-seed the source with sample documents before migrating.
    pub seed_data: Option<bool>,
}

impl Answers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The KIND/k8s namespace + name stem all resources share.
    pub const STACK: &'static str = "ma-demo";

    /// The source KIND cluster name.
    pub fn source_cluster(&self) -> String {
        format!("{}-source", Self::STACK)
    }

    /// The target KIND cluster name.
    pub fn target_cluster(&self) -> String {
        format!("{}-target", Self::STACK)
    }

    /// Whether the source engine speaks the HTTP search API (ES/OS, not Solr).
    pub fn source_is_http(&self) -> bool {
        self.source_engine
            .map(|e| e.is_http_search())
            .unwrap_or(false)
    }

    /// Whether a given client app was selected.
    pub fn has_client(&self, c: ClientApp) -> bool {
        self.clients.contains(&c)
    }

    /// Whether the plan provisions a LOCAL (KIND) target cluster — true only
    /// when provisioning a target whose kind is the local OpenSearch cluster.
    /// An AOSS NextGen target is cloud, so it needs no second KIND cluster.
    pub fn provisions_local_target(&self) -> bool {
        self.target_mode == Some(TargetMode::Provision)
            && self.target_kind.map(|k| k.is_local()).unwrap_or(true)
    }

    /// Whether the plan provisions an AOSS NextGen collection as the target.
    pub fn provisions_aoss_target(&self) -> bool {
        self.target_mode == Some(TargetMode::Provision)
            && self.target_kind == Some(TargetKind::AossServerlessNextGen)
    }

    /// The AOSS collection name for the NextGen target (DNS-ish, ≤32 chars).
    pub fn aoss_collection_name(&self) -> String {
        format!("{}-target", Self::STACK)
    }

    /// A one-line human summary of the plan, for the review screen + logs.
    pub fn summary(&self) -> String {
        let tgt = self.target.map(|t| t.label()).unwrap_or("—");
        let eng = self.source_engine.map(|e| e.label()).unwrap_or("—");
        let ver = self.source_version.as_deref().unwrap_or("—");
        format!("{tgt} · source: {eng} {ver}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_versions_are_non_empty() {
        for e in [
            SourceEngine::Elasticsearch,
            SourceEngine::OpenSearch,
            SourceEngine::Solr,
        ] {
            assert!(!e.versions().is_empty(), "{} has no versions", e.id());
        }
    }

    #[test]
    fn solr_is_not_http_search() {
        assert!(!SourceEngine::Solr.is_http_search());
        assert!(SourceEngine::Elasticsearch.is_http_search());
        assert!(SourceEngine::OpenSearch.is_http_search());
    }

    #[test]
    fn cluster_names_are_distinct_and_prefixed() {
        let a = Answers::new();
        assert_eq!(a.source_cluster(), "ma-demo-source");
        assert_eq!(a.target_cluster(), "ma-demo-target");
        assert_ne!(a.source_cluster(), a.target_cluster());
    }

    #[test]
    fn answers_roundtrip_through_json() {
        let mut a = Answers::new();
        a.target = Some(Target::Local);
        a.source_engine = Some(SourceEngine::Elasticsearch);
        a.source_version = Some("7.10.2".into());
        a.source_plugins = vec!["analysis-icu".into(), "repository-s3".into()];
        a.snapshot_storage = Some(SnapshotStorage::LocalStack);
        a.target_mode = Some(TargetMode::Provision);
        a.target_kind = Some(TargetKind::AossServerlessNextGen);
        a.target_version = Some("3.3.0".into());
        a.clients = vec![ClientApp::Locust, ClientApp::SampleSearchApp];
        a.seed_data = Some(true);

        let json = serde_json::to_string(&a).unwrap();
        let back: Answers = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn target_kind_distinguishes_local_vs_aoss() {
        let mut a = Answers::new();
        a.target_mode = Some(TargetMode::Provision);
        // Default (kind unset) is treated as the local OpenSearch cluster.
        assert!(a.provisions_local_target());
        assert!(!a.provisions_aoss_target());

        a.target_kind = Some(TargetKind::AossServerlessNextGen);
        assert!(!a.provisions_local_target());
        assert!(a.provisions_aoss_target());
        assert_eq!(a.aoss_collection_name(), "ma-demo-target");

        a.target_kind = Some(TargetKind::KindOpenSearch);
        assert!(a.provisions_local_target());
        assert!(!a.provisions_aoss_target());

        // Leaving the target to MA provisions neither.
        a.target_mode = Some(TargetMode::LeaveToMa);
        assert!(!a.provisions_local_target());
        assert!(!a.provisions_aoss_target());
    }

    #[test]
    fn default_answers_are_all_unset() {
        let a = Answers::new();
        assert!(a.target.is_none());
        assert!(a.source_engine.is_none());
        assert!(a.source_plugins.is_empty());
        assert!(a.clients.is_empty());
    }

    #[test]
    fn has_client_reflects_selection() {
        let mut a = Answers::new();
        assert!(!a.has_client(ClientApp::Locust));
        a.clients.push(ClientApp::Locust);
        assert!(a.has_client(ClientApp::Locust));
        assert!(!a.has_client(ClientApp::SampleSearchApp));
    }

    #[test]
    fn summary_handles_partial_answers() {
        let a = Answers::new();
        assert!(a.summary().contains('—'));
        let mut a2 = Answers::new();
        a2.target = Some(Target::Local);
        a2.source_engine = Some(SourceEngine::Solr);
        a2.source_version = Some("9.7.0".into());
        assert!(a2.summary().contains("Solr 9.7.0"));
    }
}

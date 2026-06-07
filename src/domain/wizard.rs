//! The wizard state machine — the adaptive question flow.
//!
//! The flow is a *derived sequence*: [`next_question`] looks at the current
//! [`Answers`] and returns the first question that still needs an answer,
//! skipping branches that don't apply (e.g. the target-version question is
//! skipped when the operator leaves the target to the Migration Assistant).
//! This keeps the flow a pure function of state — no hidden cursor — so the
//! whole question graph is unit-tested by feeding answers and asserting the
//! next question.
//!
//! The TUI ([`crate::view::tui`]) renders whatever [`next_question`] returns and
//! turns key events into [`Answer`]s applied via [`apply`]; the non-interactive
//! path fills answers from defaults the same way.

use crate::model::{
    Answers, ClientApp, MaHandoff, SnapshotStorage, SourceEngine, Target, TargetKind, TargetMode,
    TARGET_VERSIONS,
};

/// The identity of each wizard question. Stable ids so the TUI, the
/// non-interactive defaults, and the tests all refer to the same steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuestionId {
    Target,
    SourceEngine,
    SourceVersion,
    SourcePlugins,
    SnapshotStorage,
    TargetMode,
    /// Which kind of target to provision (local KIND OpenSearch vs AOSS NextGen).
    TargetKind,
    TargetVersion,
    Clients,
    SeedData,
    /// How to hand off to the Migration Assistant (local runs only).
    MaHandoff,
    /// Terminal: everything needed is answered; show the review screen.
    Review,
}

/// What kind of input a question takes — drives how the TUI renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Pick exactly one of the options.
    SingleChoice(Vec<Choice>),
    /// Pick any subset of the options (space toggles, enter confirms).
    MultiChoice(Vec<Choice>),
    /// Yes/no.
    YesNo,
}

/// One selectable option: a stable id and a human label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

impl Choice {
    fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// A fully-resolved question to present: its id, prompt, help, and input kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: QuestionId,
    pub title: String,
    pub help: String,
    pub kind: Kind,
}

/// A typed answer the TUI produces for the current question. `apply` writes it
/// into [`Answers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The id of the chosen option (for SingleChoice).
    Choice(String),
    /// The ids of the chosen options (for MultiChoice).
    Choices(Vec<String>),
    /// A yes/no answer.
    Bool(bool),
}

/// The ordered list of *candidate* questions. [`next_question`] walks this in
/// order and returns the first whose answer is still missing AND that applies
/// given the current answers.
const FLOW: [QuestionId; 11] = [
    QuestionId::Target,
    QuestionId::SourceEngine,
    QuestionId::SourceVersion,
    QuestionId::SourcePlugins,
    QuestionId::SnapshotStorage,
    QuestionId::TargetMode,
    QuestionId::TargetKind,
    QuestionId::TargetVersion,
    QuestionId::Clients,
    QuestionId::SeedData,
    QuestionId::MaHandoff,
];

/// Whether question `q` applies given the answers so far. Questions that don't
/// apply are skipped (their value stays at its default).
fn applies(q: QuestionId, a: &Answers) -> bool {
    match q {
        // The target-kind question only applies when provisioning a target.
        QuestionId::TargetKind => a.target_mode == Some(TargetMode::Provision),
        // The target-version question applies only when provisioning a LOCAL
        // OpenSearch target — an AOSS NextGen collection takes no version, and
        // leaving the target to MA needs no version either.
        QuestionId::TargetVersion => a.provisions_local_target(),
        // The MA-handoff choice (CLI vs local helm deploy) is only meaningful
        // for local runs; cloud always installs the EKS-targeting CLI.
        QuestionId::MaHandoff => a.target == Some(Target::Local),
        _ => true,
    }
}

/// Whether question `q` has been answered. The single-choice / yes-no
/// questions are "answered" when their `Option` field is set. The two
/// list-valued questions (plugins, clients) accept an empty answer as legal, so
/// emptiness can't mean "unanswered" — they carry explicit `*_done` flags set
/// by [`apply`]. This keeps [`next_question_id`] a pure function of `Answers`.
fn answered(q: QuestionId, a: &Answers) -> bool {
    match q {
        QuestionId::Target => a.target.is_some(),
        QuestionId::SourceEngine => a.source_engine.is_some(),
        QuestionId::SourceVersion => a.source_version.is_some(),
        QuestionId::SourcePlugins => a.plugins_done,
        QuestionId::SnapshotStorage => a.snapshot_storage.is_some(),
        QuestionId::TargetMode => a.target_mode.is_some(),
        QuestionId::TargetKind => a.target_kind.is_some(),
        QuestionId::TargetVersion => a.target_version.is_some(),
        QuestionId::Clients => a.clients_done,
        QuestionId::SeedData => a.seed_data.is_some(),
        QuestionId::MaHandoff => a.ma_handoff.is_some(),
        QuestionId::Review => true,
    }
}

/// Return the next question to present, or [`QuestionId::Review`] when the flow
/// is complete. Pure function of `Answers`.
pub fn next_question_id(a: &Answers) -> QuestionId {
    for &q in FLOW.iter() {
        if applies(q, a) && !answered(q, a) {
            return q;
        }
    }
    QuestionId::Review
}

/// Build the fully-resolved [`Question`] for the next step (options depend on
/// prior answers — e.g. the source-version options depend on the chosen
/// engine).
pub fn next_question(a: &Answers) -> Question {
    build(next_question_id(a), a)
}

/// Construct the presentable question for `id` given the answers so far.
pub fn build(id: QuestionId, a: &Answers) -> Question {
    match id {
        QuestionId::Target => Question {
            id,
            title: "Where should the test environment run?".into(),
            help: "Local stands up KIND clusters in Docker. Cloud emits Terraform for AWS (written but not applied by this harness).".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(Target::Local.id(), Target::Local.label()),
                Choice::new(Target::Cloud.id(), Target::Cloud.label()),
            ]),
        },
        QuestionId::SourceEngine => Question {
            id,
            title: "What is the SOURCE search engine to migrate from?".into(),
            help: "The Migration Assistant migrates from Elasticsearch, OpenSearch, or Solr into OpenSearch.".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(SourceEngine::Elasticsearch.id(), SourceEngine::Elasticsearch.label()),
                Choice::new(SourceEngine::OpenSearch.id(), SourceEngine::OpenSearch.label()),
                Choice::new(SourceEngine::Solr.id(), SourceEngine::Solr.label()),
            ]),
        },
        QuestionId::SourceVersion => {
            let eng = a.source_engine.unwrap_or(SourceEngine::Elasticsearch);
            Question {
                id,
                title: format!("Which {} version?", eng.label()),
                help: "Pick the source version to stand up. These are versions the Migration Assistant documents as supported sources.".into(),
                kind: Kind::SingleChoice(
                    eng.versions()
                        .iter()
                        .map(|v| Choice::new(*v, *v))
                        .collect(),
                ),
            }
        }
        QuestionId::SourcePlugins => {
            let eng = a.source_engine.unwrap_or(SourceEngine::Elasticsearch);
            Question {
                id,
                title: format!("Which plugins should the {} source load?", eng.label()),
                help: "Optional. repository-s3 is needed for snapshot backfill on Elasticsearch < 7.x; analysis plugins exercise mapping migration.".into(),
                kind: Kind::MultiChoice(plugin_choices(eng)),
            }
        }
        QuestionId::SnapshotStorage => Question {
            id,
            title: "Where should snapshots (the backfill source) live?".into(),
            help: "LocalStack simulates S3 in-cluster with no AWS account. Choose None to let the Migration Assistant configure its own repository.".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(SnapshotStorage::LocalStack.id(), SnapshotStorage::LocalStack.label()),
                Choice::new(SnapshotStorage::AwsS3.id(), SnapshotStorage::AwsS3.label()),
                Choice::new(SnapshotStorage::None.id(), SnapshotStorage::None.label()),
            ]),
        },
        QuestionId::TargetMode => Question {
            id,
            title: "Should the harness provision the migration TARGET?".into(),
            help: "Provision stands up a target you can pick (local OpenSearch or a cloud AOSS NextGen collection). Otherwise the Migration Assistant deploys / points at its own target.".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(TargetMode::Provision.id(), TargetMode::Provision.label()),
                Choice::new(TargetMode::LeaveToMa.id(), TargetMode::LeaveToMa.label()),
            ]),
        },
        QuestionId::TargetKind => Question {
            id,
            title: "What kind of target should the harness provision?".into(),
            help: "Local OpenSearch runs in a second KIND cluster. AOSS Serverless NextGen is a managed cloud collection that goes ACTIVE in ~5s and scales to zero — fastest to stand up.".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(
                    TargetKind::KindOpenSearch.id(),
                    TargetKind::KindOpenSearch.label(),
                ),
                Choice::new(
                    TargetKind::AossServerlessNextGen.id(),
                    TargetKind::AossServerlessNextGen.label(),
                ),
            ]),
        },
        QuestionId::TargetVersion => Question {
            id,
            title: "Which target OpenSearch version?".into(),
            help: "The OpenSearch version to stand up in the local KIND target cluster.".into(),
            kind: Kind::SingleChoice(
                TARGET_VERSIONS.iter().map(|v| Choice::new(*v, *v)).collect(),
            ),
        },
        QuestionId::Clients => Question {
            id,
            title: "Which client applications should drive load against the source?".into(),
            help: "Optional. These give the migration live traffic + data to capture and replay. Space toggles, Enter confirms.".into(),
            kind: Kind::MultiChoice(
                ClientApp::ALL
                    .iter()
                    .map(|c| Choice::new(c.id(), c.label()))
                    .collect(),
            ),
        },
        QuestionId::SeedData => Question {
            id,
            title: "Pre-seed the source with sample documents before migrating?".into(),
            help: "Seeds the source with sample docs over its REST API so the migration has real indices to move.".into(),
            kind: Kind::YesNo,
        },
        QuestionId::MaHandoff => Question {
            id,
            title: "How should the Migration Assistant be launched?".into(),
            help: "Install the MA CLI (it deploys MA to EKS — needs AWS), or deploy MA locally into its own KIND cluster via helm for a fully-local, no-AWS migration.".into(),
            kind: Kind::SingleChoice(vec![
                Choice::new(MaHandoff::DeployLocalHelm.id(), MaHandoff::DeployLocalHelm.label()),
                Choice::new(MaHandoff::InstallCli.id(), MaHandoff::InstallCli.label()),
            ]),
        },
        QuestionId::Review => Question {
            id,
            title: "Review the plan".into(),
            help: "Everything below will be provisioned, then the Migration Assistant CLI launches.".into(),
            kind: Kind::YesNo,
        },
    }
}

/// The plugin options offered for a source engine.
fn plugin_choices(eng: SourceEngine) -> Vec<Choice> {
    match eng {
        SourceEngine::Elasticsearch => vec![
            Choice::new("repository-s3", "repository-s3 (S3 snapshot repo)"),
            Choice::new("analysis-icu", "analysis-icu (ICU analyzer)"),
            Choice::new("analysis-phonetic", "analysis-phonetic"),
            Choice::new("mapper-size", "mapper-size"),
        ],
        SourceEngine::OpenSearch => vec![
            Choice::new("repository-s3", "repository-s3 (S3 snapshot repo)"),
            Choice::new("analysis-icu", "analysis-icu (ICU analyzer)"),
            Choice::new("analysis-phonetic", "analysis-phonetic"),
        ],
        // Solr "plugins" here are configset/handler conveniences; keep a small,
        // representative set so the multi-choice screen is non-empty.
        SourceEngine::Solr => vec![
            Choice::new("dih", "DataImportHandler configset"),
            Choice::new("schema-api", "Managed schema API"),
        ],
    }
}

/// Apply a typed [`Answer`] for `id` into `Answers`. Returns the next question
/// id. This is the only mutation point for the answer set (TEA `update`).
pub fn apply(a: &mut Answers, id: QuestionId, ans: Answer) -> QuestionId {
    match (id, ans) {
        (QuestionId::Target, Answer::Choice(c)) => {
            a.target = parse_target(&c);
        }
        (QuestionId::SourceEngine, Answer::Choice(c)) => {
            let eng = parse_engine(&c);
            // Changing the engine invalidates a previously-chosen version +
            // plugin set (the options differ per engine).
            if a.source_engine != eng {
                a.source_version = None;
                a.source_plugins.clear();
                a.plugins_done = false;
            }
            a.source_engine = eng;
        }
        (QuestionId::SourceVersion, Answer::Choice(c)) => {
            a.source_version = Some(c);
        }
        (QuestionId::SourcePlugins, Answer::Choices(cs)) => {
            a.source_plugins = cs;
            a.plugins_done = true;
        }
        (QuestionId::SnapshotStorage, Answer::Choice(c)) => {
            a.snapshot_storage = parse_snapshot(&c);
        }
        (QuestionId::TargetMode, Answer::Choice(c)) => {
            let mode = parse_target_mode(&c);
            // Leaving the target to MA clears both the kind and the version.
            if mode == Some(TargetMode::LeaveToMa) {
                a.target_kind = None;
                a.target_version = None;
            }
            a.target_mode = mode;
        }
        (QuestionId::TargetKind, Answer::Choice(c)) => {
            let kind = parse_target_kind(&c);
            // An AOSS NextGen target takes no OpenSearch version — clear it so
            // the version question is skipped.
            if kind == Some(TargetKind::AossServerlessNextGen) {
                a.target_version = None;
            }
            a.target_kind = kind;
        }
        (QuestionId::TargetVersion, Answer::Choice(c)) => {
            a.target_version = Some(c);
        }
        (QuestionId::Clients, Answer::Choices(cs)) => {
            a.clients = cs.iter().filter_map(|c| parse_client(c)).collect();
            a.clients_done = true;
        }
        (QuestionId::SeedData, Answer::Bool(b)) => {
            a.seed_data = Some(b);
        }
        (QuestionId::MaHandoff, Answer::Choice(c)) => {
            a.ma_handoff = parse_ma_handoff(&c);
        }
        // Any other (id, answer) pairing is a programming error in the caller;
        // ignore it rather than panic so the loop stays robust.
        _ => {}
    }
    next_question_id(a)
}

fn parse_target(s: &str) -> Option<Target> {
    match s {
        "local" => Some(Target::Local),
        "cloud" => Some(Target::Cloud),
        _ => None,
    }
}
fn parse_engine(s: &str) -> Option<SourceEngine> {
    match s {
        "elasticsearch" => Some(SourceEngine::Elasticsearch),
        "opensearch" => Some(SourceEngine::OpenSearch),
        "solr" => Some(SourceEngine::Solr),
        _ => None,
    }
}
fn parse_snapshot(s: &str) -> Option<SnapshotStorage> {
    match s {
        "localstack" => Some(SnapshotStorage::LocalStack),
        "aws-s3" => Some(SnapshotStorage::AwsS3),
        "none" => Some(SnapshotStorage::None),
        _ => None,
    }
}
fn parse_target_mode(s: &str) -> Option<TargetMode> {
    match s {
        "provision" => Some(TargetMode::Provision),
        "leave-to-ma" => Some(TargetMode::LeaveToMa),
        _ => None,
    }
}
fn parse_target_kind(s: &str) -> Option<TargetKind> {
    match s {
        "kind-opensearch" => Some(TargetKind::KindOpenSearch),
        "aoss-nextgen" => Some(TargetKind::AossServerlessNextGen),
        _ => None,
    }
}
fn parse_client(s: &str) -> Option<ClientApp> {
    match s {
        "locust" => Some(ClientApp::Locust),
        "sample-search-app" => Some(ClientApp::SampleSearchApp),
        _ => None,
    }
}
fn parse_ma_handoff(s: &str) -> Option<MaHandoff> {
    match s {
        "install-cli" => Some(MaHandoff::InstallCli),
        "deploy-local-helm" => Some(MaHandoff::DeployLocalHelm),
        _ => None,
    }
}

/// Fill every remaining answer with a sensible default (the non-interactive /
/// `-y` path). Idempotent: only unset fields are filled.
pub fn fill_defaults(a: &mut Answers) {
    if a.target.is_none() {
        a.target = Some(Target::Local);
    }
    if a.source_engine.is_none() {
        a.source_engine = Some(SourceEngine::Elasticsearch);
    }
    if a.source_version.is_none() {
        let eng = a.source_engine.unwrap();
        a.source_version = eng.versions().first().map(|s| s.to_string());
    }
    if a.snapshot_storage.is_none() {
        a.snapshot_storage = Some(SnapshotStorage::LocalStack);
    }
    if a.target_mode.is_none() {
        a.target_mode = Some(TargetMode::Provision);
    }
    // Default the target kind to the local KIND OpenSearch cluster when
    // provisioning (the zero-cloud-dependency default).
    if a.target_mode == Some(TargetMode::Provision) && a.target_kind.is_none() {
        a.target_kind = Some(TargetKind::KindOpenSearch);
    }
    // Only a local OpenSearch target takes a version (AOSS NextGen doesn't).
    if a.provisions_local_target() && a.target_version.is_none() {
        a.target_version = Some(TARGET_VERSIONS[0].to_string());
    }
    if a.seed_data.is_none() {
        a.seed_data = Some(true);
    }
    if !a.clients_done {
        if a.clients.is_empty() {
            a.clients = vec![ClientApp::Locust, ClientApp::SampleSearchApp];
        }
        a.clients_done = true;
    }
    // MA handoff: local runs default to the no-AWS local helm deploy; cloud runs
    // install the EKS-targeting CLI.
    if a.ma_handoff.is_none() {
        a.ma_handoff = Some(if a.target == Some(Target::Local) {
            MaHandoff::DeployLocalHelm
        } else {
            MaHandoff::InstallCli
        });
    }
    // Plugins default to empty but the question counts as answered.
    a.plugins_done = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_starts_at_target() {
        let a = Answers::new();
        assert_eq!(next_question_id(&a), QuestionId::Target);
    }

    #[test]
    fn full_happy_path_provision_target() {
        let mut a = Answers::new();
        assert_eq!(
            apply(&mut a, QuestionId::Target, Answer::Choice("local".into())),
            QuestionId::SourceEngine
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::SourceEngine,
                Answer::Choice("elasticsearch".into())
            ),
            QuestionId::SourceVersion
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::SourceVersion,
                Answer::Choice("7.10.2".into())
            ),
            QuestionId::SourcePlugins
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::SourcePlugins,
                Answer::Choices(vec!["repository-s3".into()])
            ),
            QuestionId::SnapshotStorage
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::SnapshotStorage,
                Answer::Choice("localstack".into())
            ),
            QuestionId::TargetMode
        );
        // Provision → now asks which KIND of target.
        assert_eq!(
            apply(
                &mut a,
                QuestionId::TargetMode,
                Answer::Choice("provision".into())
            ),
            QuestionId::TargetKind
        );
        // Local OpenSearch → then asks the version.
        assert_eq!(
            apply(
                &mut a,
                QuestionId::TargetKind,
                Answer::Choice("kind-opensearch".into())
            ),
            QuestionId::TargetVersion
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::TargetVersion,
                Answer::Choice("3.3.0".into())
            ),
            QuestionId::Clients
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::Clients,
                Answer::Choices(vec!["locust".into()])
            ),
            QuestionId::SeedData
        );
        // Local run → SeedData is followed by the MA-handoff choice.
        assert_eq!(
            apply(&mut a, QuestionId::SeedData, Answer::Bool(true)),
            QuestionId::MaHandoff
        );
        assert_eq!(
            apply(
                &mut a,
                QuestionId::MaHandoff,
                Answer::Choice("deploy-local-helm".into())
            ),
            QuestionId::Review
        );
        // Everything is set.
        assert_eq!(a.source_version.as_deref(), Some("7.10.2"));
        assert_eq!(a.source_plugins, vec!["repository-s3".to_string()]);
        assert_eq!(a.target_version.as_deref(), Some("3.3.0"));
        assert_eq!(a.clients, vec![ClientApp::Locust]);
        assert_eq!(a.seed_data, Some(true));
        assert!(a.deploys_ma_locally());
    }

    #[test]
    fn leave_to_ma_skips_target_version() {
        let mut a = Answers::new();
        apply(&mut a, QuestionId::Target, Answer::Choice("local".into()));
        apply(
            &mut a,
            QuestionId::SourceEngine,
            Answer::Choice("opensearch".into()),
        );
        apply(
            &mut a,
            QuestionId::SourceVersion,
            Answer::Choice("2.15.0".into()),
        );
        apply(&mut a, QuestionId::SourcePlugins, Answer::Choices(vec![]));
        apply(
            &mut a,
            QuestionId::SnapshotStorage,
            Answer::Choice("none".into()),
        );
        // Leaving the target to MA must skip straight to Clients (no TargetVersion).
        let next = apply(
            &mut a,
            QuestionId::TargetMode,
            Answer::Choice("leave-to-ma".into()),
        );
        assert_eq!(next, QuestionId::Clients);
        assert!(a.target_version.is_none());
    }

    #[test]
    fn aoss_nextgen_target_skips_version_question() {
        let mut a = Answers::new();
        apply(&mut a, QuestionId::Target, Answer::Choice("local".into()));
        apply(
            &mut a,
            QuestionId::SourceEngine,
            Answer::Choice("elasticsearch".into()),
        );
        apply(
            &mut a,
            QuestionId::SourceVersion,
            Answer::Choice("7.10.2".into()),
        );
        apply(&mut a, QuestionId::SourcePlugins, Answer::Choices(vec![]));
        apply(
            &mut a,
            QuestionId::SnapshotStorage,
            Answer::Choice("none".into()),
        );
        // Provision → asks the kind.
        assert_eq!(
            apply(
                &mut a,
                QuestionId::TargetMode,
                Answer::Choice("provision".into())
            ),
            QuestionId::TargetKind
        );
        // Choosing AOSS NextGen must skip the OpenSearch-version question.
        let next = apply(
            &mut a,
            QuestionId::TargetKind,
            Answer::Choice("aoss-nextgen".into()),
        );
        assert_eq!(next, QuestionId::Clients);
        assert!(a.target_version.is_none());
        assert!(a.provisions_aoss_target());
        assert!(!a.provisions_local_target());
    }

    #[test]
    fn changing_engine_resets_version_and_plugins() {
        let mut a = Answers::new();
        apply(&mut a, QuestionId::Target, Answer::Choice("local".into()));
        apply(
            &mut a,
            QuestionId::SourceEngine,
            Answer::Choice("elasticsearch".into()),
        );
        apply(
            &mut a,
            QuestionId::SourceVersion,
            Answer::Choice("7.10.2".into()),
        );
        apply(
            &mut a,
            QuestionId::SourcePlugins,
            Answer::Choices(vec!["analysis-icu".into()]),
        );
        // Re-answer the engine question with a different engine.
        apply(
            &mut a,
            QuestionId::SourceEngine,
            Answer::Choice("solr".into()),
        );
        assert_eq!(a.source_engine, Some(SourceEngine::Solr));
        assert!(a.source_version.is_none(), "version should reset");
        assert!(a.source_plugins.is_empty(), "plugins should reset");
    }

    #[test]
    fn source_version_options_track_engine() {
        let mut a = Answers::new();
        a.source_engine = Some(SourceEngine::Solr);
        let q = build(QuestionId::SourceVersion, &a);
        match q.kind {
            Kind::SingleChoice(opts) => {
                let ids: Vec<&str> = opts.iter().map(|c| c.id.as_str()).collect();
                assert!(ids.contains(&"9.7.0"));
                assert!(
                    !ids.contains(&"7.10.2"),
                    "ES versions must not leak into Solr"
                );
            }
            _ => panic!("expected single choice"),
        }
    }

    #[test]
    fn fill_defaults_completes_the_plan() {
        let mut a = Answers::new();
        fill_defaults(&mut a);
        assert_eq!(next_question_id(&a), QuestionId::Review);
        assert_eq!(a.target, Some(Target::Local));
        assert_eq!(a.source_engine, Some(SourceEngine::Elasticsearch));
        assert!(a.source_version.is_some());
        assert_eq!(a.target_mode, Some(TargetMode::Provision));
        assert!(a.target_version.is_some());
        assert_eq!(a.seed_data, Some(true));
        assert_eq!(a.clients.len(), 2);
    }

    #[test]
    fn fill_defaults_leave_to_ma_has_no_target_version() {
        let mut a = Answers::new();
        a.target_mode = Some(TargetMode::LeaveToMa);
        fill_defaults(&mut a);
        assert!(a.target_version.is_none());
        assert_eq!(next_question_id(&a), QuestionId::Review);
    }

    #[test]
    fn empty_plugin_and_client_selections_still_advance() {
        let mut a = Answers::new();
        apply(&mut a, QuestionId::Target, Answer::Choice("local".into()));
        apply(
            &mut a,
            QuestionId::SourceEngine,
            Answer::Choice("elasticsearch".into()),
        );
        apply(
            &mut a,
            QuestionId::SourceVersion,
            Answer::Choice("8.17.0".into()),
        );
        // Empty plugins selection.
        let after_plugins = apply(&mut a, QuestionId::SourcePlugins, Answer::Choices(vec![]));
        assert_eq!(after_plugins, QuestionId::SnapshotStorage);
        apply(
            &mut a,
            QuestionId::SnapshotStorage,
            Answer::Choice("localstack".into()),
        );
        apply(
            &mut a,
            QuestionId::TargetMode,
            Answer::Choice("leave-to-ma".into()),
        );
        // Empty clients selection still advances to SeedData.
        let after_clients = apply(&mut a, QuestionId::Clients, Answer::Choices(vec![]));
        assert_eq!(after_clients, QuestionId::SeedData);
        assert!(a.clients.is_empty());
    }

    #[test]
    fn review_is_terminal() {
        let mut a = Answers::new();
        fill_defaults(&mut a);
        // Applying anything at review just re-derives review.
        assert_eq!(next_question_id(&a), QuestionId::Review);
    }
}

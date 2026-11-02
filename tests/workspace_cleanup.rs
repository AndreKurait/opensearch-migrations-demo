//! Cleanup must retain the metadata needed to finish or retry teardown.
//! All external calls are mocked; these tests never touch cloud resources.

use ma_demo::cli::{self, Wizardish};
use ma_demo::error::Result;
use ma_demo::model::{Target, TargetKind, TargetMode};
use ma_demo::runner::MockRunner;
use ma_demo::state::State;
use ma_demo::tui::{Outcome, ReviewContext, ReviewOutcome};
use ma_demo::wizard;
use std::path::Path;

struct NoWizard;
impl Wizardish for NoWizard {
    fn ask(
        &self,
        _: &wizard::Question,
        _: &[String],
        _: Option<(usize, usize)>,
    ) -> Result<Outcome> {
        panic!("cleanup must not launch the provisioning wizard")
    }

    fn review(&self, _: &[wizard::ReviewRow], _: &ReviewContext) -> Result<ReviewOutcome> {
        panic!("cleanup must not review a provisioning plan")
    }
}

fn cleanup(command: &str, workspace: &Path, runner: &MockRunner) -> i32 {
    cli::dispatch(
        &[
            command.to_string(),
            "--workspace".to_string(),
            workspace.to_string_lossy().into_owned(),
        ],
        runner,
        &NoWizard,
    )
}

#[test]
fn terraform_files_survive_both_cleanup_commands_without_a_plan() {
    for command in ["clear", "destroy"] {
        let tmp = tempfile::tempdir().unwrap();
        let terraform = tmp.path().join("terraform");
        std::fs::create_dir(&terraform).unwrap();
        let original = br#"{"resources":[{"type":"aws_instance"}]}"#;
        std::fs::write(terraform.join("terraform.tfstate"), original).unwrap();
        let runner = MockRunner::new().with_command("kind");
        assert_eq!(cleanup(command, tmp.path(), &runner), 1);
        assert_eq!(
            std::fs::read(terraform.join("terraform.tfstate")).unwrap(),
            original
        );
        assert!(
            runner.calls().is_empty(),
            "must refuse before resource deletion"
        );
    }
}

#[test]
fn saved_cloud_plan_is_preserved_even_without_terraform_directory() {
    for command in ["clear", "destroy"] {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = State::new(tmp.path());
        state.plan.answers.target = Some(Target::Cloud);
        state.save().unwrap();
        let original = std::fs::read(state.plan_path()).unwrap();
        let runner = MockRunner::new().with_command("kind");
        assert_eq!(cleanup(command, tmp.path(), &runner), 1);
        assert_eq!(std::fs::read(state.plan_path()).unwrap(), original);
        assert!(runner.calls().is_empty());
    }
}

#[test]
fn corrupt_plan_is_preserved_and_does_not_trigger_default_resource_deletion() {
    for command in ["clear", "destroy"] {
        for contents in ["{bad json", "", " \n\t\r\n"] {
            let tmp = tempfile::tempdir().unwrap();
            let plan = tmp.path().join("plan.json");
            std::fs::write(&plan, contents).unwrap();
            let runner = MockRunner::new().with_command("kind");
            assert_eq!(cleanup(command, tmp.path(), &runner), 1);
            assert_eq!(std::fs::read_to_string(plan).unwrap(), contents);
            assert!(runner.calls().is_empty());
        }
    }
}

#[test]
fn aoss_plan_survives_clear_and_unavailable_aws_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = State::new(tmp.path());
    state.plan.answers.target = Some(Target::Local);
    state.plan.answers.target_mode = Some(TargetMode::Provision);
    state.plan.answers.target_kind = Some(TargetKind::AossServerlessNextGen);
    state.save().unwrap();
    let original = std::fs::read(state.plan_path()).unwrap();
    let runner = MockRunner::new().with_command("kind");
    assert_eq!(cleanup("clear", tmp.path(), &runner), 1);
    assert!(runner.calls().is_empty());
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 1);
    assert_eq!(std::fs::read(state.plan_path()).unwrap(), original);
}

#[test]
fn failed_aoss_inspection_keeps_plan_and_does_not_delete_cloud_policies() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = State::new(tmp.path());
    state.plan.answers.target = Some(Target::Local);
    state.plan.answers.target_mode = Some(TargetMode::Provision);
    state.plan.answers.target_kind = Some(TargetKind::AossServerlessNextGen);
    state.save().unwrap();
    let runner = MockRunner::new()
        .with_command("kind")
        .with_command("aws")
        .stub("aws", &["batch-get-collection"], 1, "");
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 1);
    assert!(state.plan_path().exists());
    assert!(!runner.any_call_contains("delete-security-policy"));
    assert!(!runner.any_call_contains("delete-collection"));
}

#[test]
fn failed_kind_deletion_keeps_workspace_for_retry() {
    let tmp = tempfile::tempdir().unwrap();
    State::new(tmp.path()).save().unwrap();
    let runner = MockRunner::new().with_command("kind").default_status(1);
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 1);
    assert!(tmp.path().join("plan.json").exists());
    assert_eq!(
        runner.calls_to("kind").len(),
        6,
        "each cluster gets one retry"
    );
}

const COLLECTION: &str = r#"{"collectionDetails":[{"id":"col123","name":"ma-demo-target"}]}"#;
const GROUP: &str = r#"{"collectionGroupDetails":[{"id":"grp123","name":"cg-ma-demo-target"}]}"#;
const ABSENT_COLLECTION: &str =
    r#"{"collectionErrorDetails":[{"name":"ma-demo-target","errorCode":"NOT_FOUND"}]}"#;
const ABSENT_GROUP: &str =
    r#"{"collectionGroupErrorDetails":[{"name":"cg-ma-demo-target","errorCode":"NOT_FOUND"}]}"#;

fn save_aoss_plan(workspace: &Path) -> State {
    let mut state = State::new(workspace);
    state.plan.answers.target = Some(Target::Local);
    state.plan.answers.target_mode = Some(TargetMode::Provision);
    state.plan.answers.target_kind = Some(TargetKind::AossServerlessNextGen);
    state.save().unwrap();
    state
}

fn aoss_runner(collection: &str, group: &str) -> MockRunner {
    MockRunner::new()
        .with_command("kind")
        .with_command("aws")
        // The collection command is a substring of the group command.
        .stub("aws", &["batch-get-collection-group"], 0, group)
        .stub("aws", &["batch-get-collection"], 0, collection)
}

fn assert_no_cloud_deletion(runner: &MockRunner) {
    assert!(
        runner
            .calls_to("aws")
            .iter()
            .all(|call| !call.args.iter().any(|arg| arg.starts_with("delete-"))),
        "lookup failure must stop all AWS deletes"
    );
}

#[test]
fn per_item_lookup_errors_stop_deletion_even_when_batch_command_succeeds() {
    for (collection, group) in [
        (
            r#"{"collectionErrorDetails":[{"name":"ma-demo-target","errorCode":"ACCESS_DENIED"}]}"#,
            GROUP,
        ),
        (
            COLLECTION,
            r#"{"collectionGroupErrorDetails":[{"name":"cg-ma-demo-target","errorCode":"INTERNAL_FAILURE"}]}"#,
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let state = save_aoss_plan(tmp.path());
        let original = std::fs::read(state.plan_path()).unwrap();
        let runner = aoss_runner(collection, group);
        assert_eq!(cleanup("destroy", tmp.path(), &runner), 1);
        assert_no_cloud_deletion(&runner);
        assert_eq!(std::fs::read(state.plan_path()).unwrap(), original);
    }
}

#[test]
fn failed_group_command_prevents_deletion_of_collection_and_policies() {
    let tmp = tempfile::tempdir().unwrap();
    let state = save_aoss_plan(tmp.path());
    let runner = MockRunner::new()
        .with_command("kind")
        .with_command("aws")
        .stub("aws", &["batch-get-collection-group"], 1, "")
        .stub("aws", &["batch-get-collection"], 0, COLLECTION);
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 1);
    assert_no_cloud_deletion(&runner);
    assert!(state.plan_path().exists());
}

#[test]
fn malformed_ambiguous_or_unrelated_lookup_results_do_not_mean_absent() {
    let cases = [
        "",
        "null",
        "[]",
        "{}",
        r#"{"collectionDetails":[]}"#,
        r#"{"collectionDetails":null}"#,
        r#"{"collectionDetails":[{"name":"ma-demo-target"}]}"#,
        r#"{"collectionDetails":[{"name":"ma-demo-target","id":""}]}"#,
        r#"{"collectionDetails":[{"name":"ma-demo-target","id":17}]}"#,
        r#"{"collectionDetails":[{"name":"other-collection","id":"col123"}]}"#,
        r#"{"collectionDetails":[{"name":"ma-demo-target","id":"col123"},{"name":"ma-demo-target","id":"col456"}]}"#,
        r#"{"collectionDetails":[{"name":"ma-demo-target","id":"col123"}],"collectionErrorDetails":[{"name":"ma-demo-target","errorCode":"NOT_FOUND"}]}"#,
        r#"{"collectionErrorDetails":[{"name":"other-collection","errorCode":"NOT_FOUND"}]}"#,
        r#"{"collectionErrorDetails":[{"name":"ma-demo-target"}]}"#,
        r#"{"collectionErrorDetails":[{"name":"ma-demo-target","errorCode":"ResourceNotFoundException"}]}"#,
    ];
    for collection in cases {
        let tmp = tempfile::tempdir().unwrap();
        let state = save_aoss_plan(tmp.path());
        let runner = aoss_runner(collection, GROUP);
        assert_eq!(cleanup("destroy", tmp.path(), &runner), 1, "{collection}");
        assert_no_cloud_deletion(&runner);
        assert!(state.plan_path().exists());
    }
    for group in [
        "{}",
        r#"{"collectionGroupDetails":[]}"#,
        r#"{"collectionGroupErrorDetails":{}}"#,
        r#"{"collectionGroupDetails":[{"name":"cg-ma-demo-target","id":"grp123"}],"collectionGroupErrorDetails":[{"name":"cg-ma-demo-target","errorCode":"NOT_FOUND"}]}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        save_aoss_plan(tmp.path());
        let runner = aoss_runner(COLLECTION, group);
        assert_eq!(cleanup("destroy", tmp.path(), &runner), 1, "{group}");
        assert_no_cloud_deletion(&runner);
    }
}

#[test]
fn both_lookups_finish_before_the_first_cloud_delete() {
    let tmp = tempfile::tempdir().unwrap();
    save_aoss_plan(tmp.path());
    let runner = aoss_runner(COLLECTION, GROUP);
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 0);
    let calls = runner.calls_to("aws");
    assert_eq!(calls[0].args[1], "batch-get-collection");
    assert_eq!(calls[1].args[1], "batch-get-collection-group");
    assert!(calls[2].args[1].starts_with("delete-"));
}

#[test]
fn confirmed_not_found_lookup_and_policy_deletes_are_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = save_aoss_plan(tmp.path());
    let original = std::fs::read(state.plan_path()).unwrap();
    let runner = aoss_runner(ABSENT_COLLECTION, ABSENT_GROUP)
        .stub_stderr(
            "aws",
            &["delete-security-policy"],
            254,
            "An error occurred (ResourceNotFoundException) when calling the DeleteSecurityPolicy operation: Policy does not exist",
        )
        .stub_stderr(
            "aws",
            &["delete-access-policy"],
            254,
            "An error occurred (ResourceNotFoundException) when calling the DeleteAccessPolicy operation: Policy does not exist",
        );
    for _ in 0..2 {
        assert_eq!(cleanup("destroy", tmp.path(), &runner), 0);
        assert_eq!(std::fs::read(state.plan_path()).unwrap(), original);
    }
    assert!(runner.calls_to("aws").iter().all(|call| {
        call.args[1] != "delete-collection" && call.args[1] != "delete-collection-group"
    }));
}

#[test]
fn resource_disappearing_between_lookup_and_delete_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    save_aoss_plan(tmp.path());
    let runner = aoss_runner(COLLECTION, GROUP)
        .stub_stderr(
            "aws",
            &["delete-collection-group"],
            254,
            "An error occurred (ResourceNotFoundException) when calling the DeleteCollectionGroup operation: Group does not exist",
        )
        .stub_stderr(
            "aws",
            &["delete-collection"],
            254,
            "An error occurred (ResourceNotFoundException) when calling the DeleteCollection operation: Collection does not exist",
        );
    assert_eq!(cleanup("destroy", tmp.path(), &runner), 0);
}

#[test]
fn generic_not_found_text_or_wrong_aws_error_does_not_hide_delete_failure() {
    for stderr in [
        "policy not found",
        "ResourceNotFoundException",
        "An error occurred (AccessDeniedException) when calling the DeleteSecurityPolicy operation: ResourceNotFoundException not found",
        "An error occurred (ResourceNotFoundException) when calling the GetSecurityPolicy operation: Policy does not exist",
        "An error occurred (ResourceNotFoundException) when calling the DeleteSecurityPolicy operation:",
        "An error occurred (ResourceNotFoundException) when calling the DeleteSecurityPolicy operation: Missing\nanother command failed",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let state = save_aoss_plan(tmp.path());
        let runner = aoss_runner(ABSENT_COLLECTION, ABSENT_GROUP).stub_stderr(
            "aws",
            &["delete-security-policy"],
            254,
            stderr,
        );
        assert_eq!(cleanup("destroy", tmp.path(), &runner), 1, "{stderr}");
        assert!(state.plan_path().exists());
    }
}

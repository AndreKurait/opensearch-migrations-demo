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
        let tmp = tempfile::tempdir().unwrap();
        let plan = tmp.path().join("plan.json");
        std::fs::write(&plan, "{bad json").unwrap();
        let runner = MockRunner::new().with_command("kind");
        assert_eq!(cleanup(command, tmp.path(), &runner), 1);
        assert_eq!(std::fs::read_to_string(plan).unwrap(), "{bad json");
        assert!(runner.calls().is_empty());
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

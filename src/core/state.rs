//! Persisted harness state — the collected [`Answers`] plus the provisioning
//! step reached, saved to `plan.json` under the workspace so a run can resume.
//!
//! Mirrors the migration-assistant CLI's per-stage state directory: the harness
//! writes everything under `./migration-demo-workspace/` (one removable folder).

use crate::error::Result;
use crate::model::Answers;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The provisioning step the harness has reached. Used to resume and to drive
/// the progress timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Step {
    /// Nothing done yet (fresh plan).
    Planned,
    /// Preflight checks (docker/kind/kubectl/helm present + daemon up) passed.
    PreflightDone,
    /// Source KIND cluster + workload created.
    SourceUp,
    /// Snapshot storage (LocalStack) deployed.
    SnapshotUp,
    /// Target KIND cluster + OpenSearch created (or skipped).
    TargetUp,
    /// Client apps deployed (or skipped).
    ClientsUp,
    /// Sample data seeded (or skipped).
    DataSeeded,
    /// Migration Assistant CLI installed + ready to launch.
    Ready,
}

impl Step {
    /// The ordered list of steps, for the timeline.
    pub const ORDER: [Step; 8] = [
        Step::Planned,
        Step::PreflightDone,
        Step::SourceUp,
        Step::SnapshotUp,
        Step::TargetUp,
        Step::ClientsUp,
        Step::DataSeeded,
        Step::Ready,
    ];

    /// A short human label for the timeline.
    pub fn label(self) -> &'static str {
        match self {
            Step::Planned => "Plan collected",
            Step::PreflightDone => "Preflight checks",
            Step::SourceUp => "Source cluster",
            Step::SnapshotUp => "Snapshot storage",
            Step::TargetUp => "Target cluster",
            Step::ClientsUp => "Client apps",
            Step::DataSeeded => "Seed sample data",
            Step::Ready => "Launch Migration Assistant",
        }
    }

    /// This step's index in [`Step::ORDER`].
    pub fn index(self) -> usize {
        Step::ORDER.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// The serialized plan written to `plan.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub answers: Answers,
    pub step: Step,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            answers: Answers::new(),
            step: Step::Planned,
        }
    }
}

/// State handle bound to a workspace directory.
#[derive(Debug, Clone)]
pub struct State {
    dir: PathBuf,
    pub plan: Plan,
}

impl State {
    /// A state handle for `dir` (created lazily on save). Starts with a fresh
    /// plan; call [`State::load`] to merge any persisted plan.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            plan: Plan::default(),
        }
    }

    /// The workspace directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The plan file path.
    pub fn plan_path(&self) -> PathBuf {
        self.dir.join("plan.json")
    }

    /// Load `plan.json` if present, replacing the in-memory plan. A missing
    /// file is not an error (fresh workspace).
    pub fn load(&mut self) -> Result<()> {
        let path = self.plan_path();
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(&path)?;
        if text.trim().is_empty() {
            return Ok(());
        }
        self.plan = serde_json::from_str(&text)?;
        Ok(())
    }

    /// Persist the current plan to `plan.json` (pretty-printed), creating the
    /// workspace directory if needed.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(&self.plan)?;
        std::fs::write(self.plan_path(), text)?;
        Ok(())
    }

    /// Advance the recorded step (monotonic — never moves backward).
    pub fn advance(&mut self, step: Step) {
        if step.index() > self.plan.step.index() {
            self.plan.step = step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceEngine, Target};

    #[test]
    fn step_order_and_index_are_consistent() {
        assert_eq!(Step::Planned.index(), 0);
        assert_eq!(Step::Ready.index(), Step::ORDER.len() - 1);
        for (i, s) in Step::ORDER.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = State::new(tmp.path());
        s.plan.answers.target = Some(Target::Local);
        s.plan.answers.source_engine = Some(SourceEngine::Solr);
        s.advance(Step::SourceUp);
        s.save().unwrap();

        let mut s2 = State::new(tmp.path());
        s2.load().unwrap();
        assert_eq!(s2.plan.answers.target, Some(Target::Local));
        assert_eq!(s2.plan.answers.source_engine, Some(SourceEngine::Solr));
        assert_eq!(s2.plan.step, Step::SourceUp);
    }

    #[test]
    fn load_missing_file_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = State::new(tmp.path().join("nope"));
        assert!(s.load().is_ok());
        assert_eq!(s.plan.step, Step::Planned);
    }

    #[test]
    fn advance_is_monotonic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = State::new(tmp.path());
        s.advance(Step::TargetUp);
        assert_eq!(s.plan.step, Step::TargetUp);
        // A lower step does not move it backward.
        s.advance(Step::SourceUp);
        assert_eq!(s.plan.step, Step::TargetUp);
        // A higher step advances.
        s.advance(Step::Ready);
        assert_eq!(s.plan.step, Step::Ready);
    }
}

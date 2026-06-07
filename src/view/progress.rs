//! The provisioning progress timeline — a pure projection of the current
//! [`Step`] to a list of rows with done/current/pending status, plus a Ratatui
//! renderer asserted via `TestBackend`.
//!
//! Mirrors the migration-assistant CLI's resume timeline so the two surfaces
//! look identical.

use crate::state::Step;
use ratatui::{
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

/// The status of one timeline phase relative to the current step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStatus {
    Done,
    Current,
    Pending,
}

impl PhaseStatus {
    /// The glyph for this status (● done, ◐ current, ○ pending).
    pub fn marker(self) -> char {
        match self {
            PhaseStatus::Done => '●',
            PhaseStatus::Current => '◐',
            PhaseStatus::Pending => '○',
        }
    }
}

/// One row of the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub label: &'static str,
    pub status: PhaseStatus,
}

/// Build the timeline rows for a given current step. Steps before `current` are
/// Done, the step at `current` is Current, the rest Pending.
pub fn rows(current: Step) -> Vec<Row> {
    let cur = current.index();
    Step::ORDER
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let status = match i.cmp(&cur) {
                std::cmp::Ordering::Less => PhaseStatus::Done,
                std::cmp::Ordering::Equal => PhaseStatus::Current,
                std::cmp::Ordering::Greater => PhaseStatus::Pending,
            };
            Row {
                label: s.label(),
                status,
            }
        })
        .collect()
}

/// Render the timeline into `frame` for the given current step.
pub fn render(frame: &mut Frame, current: Step, title: &str) {
    let lines: Vec<Line> = rows(current)
        .iter()
        .map(|r| {
            let glyph = Span::from(format!("  {} ", r.status.marker()));
            let (glyph, label) = match r.status {
                PhaseStatus::Done => (glyph.green(), Span::raw(r.label).dim()),
                PhaseStatus::Current => (glyph.yellow(), Span::raw(r.label).bold()),
                PhaseStatus::Pending => (glyph.dim(), Span::raw(r.label).dim()),
            };
            Line::from(vec![glyph, label])
        })
        .collect();
    let block = Block::bordered().title(format!(" {title} "));
    frame.render_widget(Paragraph::new(lines).block(block), frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn rows_mark_done_current_pending() {
        let r = rows(Step::SourceUp);
        // Planned + Preflight are before SourceUp → Done.
        assert_eq!(r[0].status, PhaseStatus::Done);
        assert_eq!(r[1].status, PhaseStatus::Done);
        // SourceUp itself → Current.
        assert_eq!(r[2].status, PhaseStatus::Current);
        assert_eq!(r[2].label, "Source cluster");
        // The rest → Pending.
        assert_eq!(r[3].status, PhaseStatus::Pending);
        assert_eq!(r.last().unwrap().status, PhaseStatus::Pending);
    }

    #[test]
    fn first_step_has_no_done_rows() {
        let r = rows(Step::Planned);
        assert_eq!(r[0].status, PhaseStatus::Current);
        assert!(r[1..].iter().all(|x| x.status == PhaseStatus::Pending));
    }

    #[test]
    fn last_step_has_no_pending_rows() {
        let r = rows(Step::Ready);
        assert_eq!(r.last().unwrap().status, PhaseStatus::Current);
        assert!(r[..r.len() - 1]
            .iter()
            .all(|x| x.status == PhaseStatus::Done));
    }

    #[test]
    fn markers_are_distinct() {
        assert_eq!(PhaseStatus::Done.marker(), '●');
        assert_eq!(PhaseStatus::Current.marker(), '◐');
        assert_eq!(PhaseStatus::Pending.marker(), '○');
    }

    #[test]
    fn timeline_renders_exact_frame() {
        let mut t = Terminal::new(TestBackend::new(44, 10)).unwrap();
        t.draw(|f| render(f, Step::SourceUp, "Setup")).unwrap();
        let buf = t.backend().buffer().clone();
        let actual: Vec<String> = (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .collect();
        assert_eq!(
            actual,
            vec![
                "┌ Setup ───────────────────────────────────┐",
                "│  ● Plan collected                        │",
                "│  ● Preflight checks                      │",
                "│  ◐ Source cluster                        │",
                "│  ○ Snapshot storage                      │",
                "│  ○ Target cluster                        │",
                "│  ○ Client apps                           │",
                "│  ○ Seed sample data                      │",
                "│  ○ Launch Migration Assistant            │",
                "└──────────────────────────────────────────┘",
            ]
        );
    }
}

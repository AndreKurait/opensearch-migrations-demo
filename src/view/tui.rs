//! The Ratatui wizard front-end, built The-Elm-Architecture way (Model →
//! Message → update → view).
//!
//! [`Screen`] is the immediate-mode model for *one* wizard question: the
//! [`Question`] to present plus the cursor/selection state. Key events become
//! [`ScreenMsg`]s, [`Screen::update`] is the only mutation point, and the view
//! is a pure projection asserted via `TestBackend`. The driver loop
//! ([`run`]) feeds questions in and reads [`Answer`]s out; the wizard state
//! machine ([`crate::wizard`]) decides what comes next.
//!
//! On a non-interactive run the TUI is never entered — the dispatcher fills
//! answers from defaults directly.

use crate::wizard::{Answer, Choice, Kind, Question};
use ratatui::{
    layout::{Constraint, Layout},
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, Paragraph, Widget, Wrap},
    Frame,
};

/// A message the screen reacts to — the only way the model changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMsg {
    /// Move the cursor down one option.
    Down,
    /// Move the cursor up one option.
    Up,
    /// Toggle the option under the cursor (multi-choice) or pick yes/no.
    Toggle,
    /// Confirm: select the cursor option (single) / accept the set (multi) /
    /// accept the yes-no.
    Confirm,
    /// Cancel the whole wizard.
    Cancel,
}

/// What the screen produced when the loop should stop for this question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The user answered; carry the typed answer.
    Answered(Answer),
    /// The user cancelled the wizard.
    Cancelled,
}

/// The immediate-mode model for one question.
#[derive(Debug, Clone)]
pub struct Screen {
    pub question: Question,
    /// Cursor position over the options (single/multi) — unused for yes/no
    /// where the cursor is the bool itself.
    pub cursor: usize,
    /// For multi-choice: the set of selected option indices.
    pub selected: Vec<usize>,
    /// For yes/no: the current bool under the cursor.
    pub yes: bool,
    /// Set when the question is answered/cancelled — stops the loop.
    pub outcome: Option<Outcome>,
}

impl Screen {
    /// A fresh screen for `question`. For multi-choice, options whose ids are in
    /// `preselect` start selected (resume-friendly).
    pub fn new(question: Question, preselect: &[String]) -> Self {
        let selected = match &question.kind {
            Kind::MultiChoice(choices) => choices
                .iter()
                .enumerate()
                .filter(|(_, c)| preselect.contains(&c.id))
                .map(|(i, _)| i)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            question,
            cursor: 0,
            selected,
            yes: true,
            outcome: None,
        }
    }

    /// The option list for choice questions (empty for yes/no).
    fn choices(&self) -> &[Choice] {
        match &self.question.kind {
            Kind::SingleChoice(c) | Kind::MultiChoice(c) => c,
            Kind::YesNo => &[],
        }
    }

    fn is_multi(&self) -> bool {
        matches!(self.question.kind, Kind::MultiChoice(_))
    }
    fn is_yesno(&self) -> bool {
        matches!(self.question.kind, Kind::YesNo)
    }

    /// Apply a message — the sole mutation point (TEA `update`).
    pub fn update(&mut self, msg: ScreenMsg) {
        match msg {
            ScreenMsg::Down => self.move_cursor(1),
            ScreenMsg::Up => self.move_cursor(-1),
            ScreenMsg::Toggle => self.toggle(),
            ScreenMsg::Confirm => self.confirm(),
            ScreenMsg::Cancel => self.outcome = Some(Outcome::Cancelled),
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        if self.is_yesno() {
            // Up/Down flips the yes/no choice.
            self.yes = !self.yes;
            return;
        }
        let n = self.choices().len();
        if n == 0 {
            return;
        }
        let cur = self.cursor as i32 + delta;
        // Wrap.
        self.cursor = cur.rem_euclid(n as i32) as usize;
    }

    fn toggle(&mut self) {
        if self.is_yesno() {
            self.yes = !self.yes;
            return;
        }
        if self.is_multi() {
            if let Some(pos) = self.selected.iter().position(|i| *i == self.cursor) {
                self.selected.remove(pos);
            } else {
                self.selected.push(self.cursor);
            }
        }
        // Single-choice: toggle is a no-op (Confirm selects).
    }

    fn confirm(&mut self) {
        let ans = match &self.question.kind {
            Kind::SingleChoice(choices) => choices
                .get(self.cursor)
                .map(|c| Answer::Choice(c.id.clone())),
            Kind::MultiChoice(choices) => {
                let mut idx = self.selected.clone();
                idx.sort_unstable();
                let ids = idx
                    .iter()
                    .filter_map(|i| choices.get(*i).map(|c| c.id.clone()))
                    .collect();
                Some(Answer::Choices(ids))
            }
            Kind::YesNo => Some(Answer::Bool(self.yes)),
        };
        if let Some(a) = ans {
            self.outcome = Some(Outcome::Answered(a));
        }
    }

    /// Map a key to a [`ScreenMsg`], or `None` if inert. Pure.
    pub fn key_to_msg(&self, code: ratatui::crossterm::event::KeyCode) -> Option<ScreenMsg> {
        use ratatui::crossterm::event::KeyCode::*;
        match code {
            Down | Char('j') => Some(ScreenMsg::Down),
            Up | Char('k') => Some(ScreenMsg::Up),
            Char(' ') => Some(ScreenMsg::Toggle),
            Enter => Some(ScreenMsg::Confirm),
            Esc => Some(ScreenMsg::Cancel),
            // y/n shortcuts on yes-no screens.
            Char('y') | Char('Y') if self.is_yesno() => {
                // Caller-visible: set + confirm in one is handled by update
                // sequence in the loop; here we just map to Toggle→Confirm via
                // Confirm after forcing yes. Simpler: treat as Confirm of yes.
                Some(ScreenMsg::Confirm)
            }
            _ => None,
        }
    }

    /// Render the whole question screen (immediate mode).
    pub fn view(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }
}

impl Widget for &Screen {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let block = Block::bordered().title(" Migration Assistant — Demo Setup ".bold());
        let inner = block.inner(area);
        block.render(area, buf);

        // title (1) · blank (1) · options (min) · blank (1) · help (2) · hint (1)
        let [title_a, _gap, body_a, help_a, hint_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);

        Paragraph::new(Line::from(self.question.title.as_str().bold())).render(title_a, buf);

        self.render_body(body_a, buf);

        Paragraph::new(self.question.help.as_str().dim())
            .wrap(Wrap { trim: true })
            .render(help_a, buf);

        Paragraph::new(self.hint().dim()).render(hint_a, buf);
    }
}

impl Screen {
    fn render_body(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        match &self.question.kind {
            Kind::YesNo => {
                let yes = option_span("Yes", self.yes, None);
                let no = option_span("No", !self.yes, None);
                let yes = if self.yes { yes.reversed() } else { yes };
                let no = if !self.yes { no.reversed() } else { no };
                Paragraph::new(vec![Line::from(yes), Line::from(no)]).render(area, buf);
            }
            Kind::SingleChoice(choices) | Kind::MultiChoice(choices) => {
                let multi = self.is_multi();
                let lines: Vec<Line> = choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let on_cursor = i == self.cursor;
                        // Multi-choice shows a checkbox; single-choice shows none.
                        let checkbox = if multi {
                            Some(self.selected.contains(&i))
                        } else {
                            None
                        };
                        let span = option_span(&c.label, on_cursor, checkbox);
                        let span = if matches!(checkbox, Some(true)) {
                            span.bold()
                        } else {
                            span
                        };
                        let span = if on_cursor { span.reversed() } else { span };
                        Line::from(span)
                    })
                    .collect();
                Paragraph::new(lines).render(area, buf);
            }
        }
    }

    /// The footer hint for this question kind.
    fn hint(&self) -> &'static str {
        match self.question.kind {
            Kind::SingleChoice(_) => "↑↓ move · Enter select · Esc cancel",
            Kind::MultiChoice(_) => "↑↓ move · Space toggle · Enter confirm · Esc cancel",
            Kind::YesNo => "↑↓/Space flip · Enter confirm · Esc cancel",
        }
    }
}

/// Build a prefix-marked option span: a cursor caret, an optional checkbox
/// (`Some` only for multi-choice), then the label.
fn option_span(label: &str, on_cursor: bool, checkbox: Option<bool>) -> Span<'static> {
    let caret = if on_cursor { "▶ " } else { "  " };
    let check = match checkbox {
        Some(true) => "[x] ",
        Some(false) => "[ ] ",
        None => "",
    };
    Span::from(format!("{caret}{check}{label}"))
}

/// Drive one question interactively to an [`Outcome`]. Sets up the terminal,
/// runs the draw→input loop, and ALWAYS restores the terminal before returning.
/// `preselect` seeds multi-choice selections (resume).
pub fn run(question: Question, preselect: &[String]) -> std::io::Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let result = run_event_loop(&mut terminal, Screen::new(question, preselect));
    ratatui::restore();
    result
}

/// The draw → input loop for a single screen. Returns the screen's outcome.
fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut screen: Screen,
) -> std::io::Result<Outcome> {
    use ratatui::crossterm::event::{self, Event};
    loop {
        terminal.draw(|f| screen.view(f))?;
        if let Some(outcome) = &screen.outcome {
            return Ok(outcome.clone());
        }
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.is_press() {
                    if let Some(msg) = screen.key_to_msg(key.code) {
                        screen.update(msg);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Answers;
    use crate::wizard::{self, QuestionId};
    use ratatui::crossterm::event::KeyCode;

    fn single() -> Screen {
        let q = wizard::build(QuestionId::Target, &Answers::new());
        Screen::new(q, &[])
    }
    fn multi() -> Screen {
        let mut a = Answers::new();
        a.source_engine = Some(crate::model::SourceEngine::Elasticsearch);
        let q = wizard::build(QuestionId::SourcePlugins, &a);
        Screen::new(q, &[])
    }
    fn yesno() -> Screen {
        let q = wizard::build(QuestionId::SeedData, &Answers::new());
        Screen::new(q, &[])
    }

    #[test]
    fn single_choice_cursor_wraps() {
        let mut s = single();
        assert_eq!(s.cursor, 0);
        s.update(ScreenMsg::Up); // wraps to last
        assert_eq!(s.cursor, 1);
        s.update(ScreenMsg::Down); // wraps to first
        assert_eq!(s.cursor, 0);
        s.update(ScreenMsg::Down);
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn single_choice_confirm_emits_choice() {
        let mut s = single();
        s.update(ScreenMsg::Down); // cursor on second option (cloud)
        s.update(ScreenMsg::Confirm);
        assert_eq!(
            s.outcome,
            Some(Outcome::Answered(Answer::Choice("cloud".into())))
        );
    }

    #[test]
    fn multi_choice_toggle_builds_set_in_order() {
        let mut s = multi();
        // Toggle option 0, move to 2, toggle it.
        s.update(ScreenMsg::Toggle);
        s.update(ScreenMsg::Down);
        s.update(ScreenMsg::Down);
        s.update(ScreenMsg::Toggle);
        s.update(ScreenMsg::Confirm);
        match s.outcome {
            Some(Outcome::Answered(Answer::Choices(ids))) => {
                // Sorted by index → repository-s3 (0) then analysis-phonetic (2).
                assert_eq!(ids.len(), 2);
                assert_eq!(ids[0], "repository-s3");
            }
            other => panic!("expected choices, got {other:?}"),
        }
    }

    #[test]
    fn multi_choice_toggle_off_removes() {
        let mut s = multi();
        s.update(ScreenMsg::Toggle); // select 0
        s.update(ScreenMsg::Toggle); // deselect 0
        s.update(ScreenMsg::Confirm);
        assert_eq!(s.outcome, Some(Outcome::Answered(Answer::Choices(vec![]))));
    }

    #[test]
    fn multi_choice_preselect_starts_checked() {
        let mut a = Answers::new();
        a.source_engine = Some(crate::model::SourceEngine::Elasticsearch);
        let q = wizard::build(QuestionId::SourcePlugins, &a);
        let s = Screen::new(q, &["analysis-icu".to_string()]);
        // analysis-icu is option index 1.
        assert!(s.selected.contains(&1));
    }

    #[test]
    fn yesno_flip_and_confirm() {
        let mut s = yesno();
        assert!(s.yes);
        s.update(ScreenMsg::Down);
        assert!(!s.yes);
        s.update(ScreenMsg::Confirm);
        assert_eq!(s.outcome, Some(Outcome::Answered(Answer::Bool(false))));
    }

    #[test]
    fn cancel_sets_cancelled() {
        let mut s = single();
        s.update(ScreenMsg::Cancel);
        assert_eq!(s.outcome, Some(Outcome::Cancelled));
    }

    #[test]
    fn key_bindings_map() {
        let s = single();
        assert_eq!(s.key_to_msg(KeyCode::Down), Some(ScreenMsg::Down));
        assert_eq!(s.key_to_msg(KeyCode::Char('j')), Some(ScreenMsg::Down));
        assert_eq!(s.key_to_msg(KeyCode::Up), Some(ScreenMsg::Up));
        assert_eq!(s.key_to_msg(KeyCode::Enter), Some(ScreenMsg::Confirm));
        assert_eq!(s.key_to_msg(KeyCode::Esc), Some(ScreenMsg::Cancel));
        assert_eq!(s.key_to_msg(KeyCode::Char(' ')), Some(ScreenMsg::Toggle));
    }

    #[test]
    fn renders_without_panicking_and_shows_title() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut t = Terminal::new(TestBackend::new(70, 16)).unwrap();
        let s = single();
        t.draw(|f| s.view(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(text.contains("Where should the test environment run"));
        assert!(text.contains("Local"));
        assert!(text.contains("Cloud"));
    }
}

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

use crate::wizard::{Answer, Choice, Kind, Question, QuestionId, ReviewRow};
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
    /// On a yes/no screen: force the value to the given bool, then confirm — the
    /// `y`/`n` shortcuts. (A plain `Confirm` accepts whatever the cursor is on;
    /// this is what makes `y`/`n` answer *yes*/*no* regardless of the cursor.)
    ConfirmYesNo(bool),
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
    /// `(step, total)` for the footer's "Step N of M" affordance — `None` hides
    /// it (e.g. an isolated edit re-ask from the review screen).
    pub step: Option<(usize, usize)>,
}

impl Screen {
    /// A fresh screen for `question`. For multi-choice, options whose ids are in
    /// `preselect` start selected (resume-friendly). When `preselect` is empty,
    /// options marked `default_on` (e.g. the snapshot-repository plugin) start
    /// checked instead.
    pub fn new(question: Question, preselect: &[String]) -> Self {
        let selected = match &question.kind {
            Kind::MultiChoice(choices) => choices
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    if preselect.is_empty() {
                        c.default_on
                    } else {
                        preselect.contains(&c.id)
                    }
                })
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
            step: None,
        }
    }

    /// Set the `(step, total)` shown in the footer. Builder-style so `new` stays
    /// the common path and tests don't need to pass a step.
    pub fn with_step(mut self, step: Option<(usize, usize)>) -> Self {
        self.step = step;
        self
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
            ScreenMsg::ConfirmYesNo(v) => {
                self.yes = v;
                self.confirm();
            }
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
            // y/n shortcuts on yes-no screens force the value (not "confirm
            // whatever the cursor is on"), so `y` always answers yes and `n` no.
            Char('y') | Char('Y') if self.is_yesno() => Some(ScreenMsg::ConfirmYesNo(true)),
            Char('n') | Char('N') if self.is_yesno() => Some(ScreenMsg::ConfirmYesNo(false)),
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

        // Help area: the focused option's description when it has one (so each
        // plugin/choice is explained as you move the cursor), else the
        // question's general help.
        let help_text = self
            .focused_description()
            .unwrap_or_else(|| self.question.help.clone());
        Paragraph::new(help_text.dim())
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

    /// The description of the option under the cursor, if any.
    fn focused_description(&self) -> Option<String> {
        let c = self.choices().get(self.cursor)?;
        if c.description.is_empty() {
            None
        } else {
            Some(c.description.clone())
        }
    }

    /// The footer hint for this question kind: a "Step N of M" prefix (when a
    /// step is set), the key bindings (including the y/n yes-no shortcuts, the
    /// `q`/`^C` aborts), and a note that everything is editable on the review.
    fn hint(&self) -> String {
        let keys = match self.question.kind {
            Kind::SingleChoice(_) => "↑↓ move · Enter select · Esc/^C cancel",
            Kind::MultiChoice(_) => "↑↓ move · Space toggle · Enter confirm · Esc/^C cancel",
            Kind::YesNo => "↑↓/Space flip · y/n or Enter confirm · Esc/^C cancel",
        };
        let prefix = match self.step {
            Some((n, total)) => format!("Step {n} of {total} · "),
            None => String::new(),
        };
        format!("{prefix}{keys} · editable on the final review")
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

/// Whether a key event is Ctrl-C (or Ctrl-D). In the terminal's raw mode the
/// kernel does NOT translate Ctrl-C into SIGINT — it arrives as a key event
/// with the CONTROL modifier — so every interactive loop must treat it as a
/// quit/cancel itself, or Ctrl-C appears to do nothing. Pure, so it's testable.
pub fn is_ctrl_c(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
}

/// Drive one question interactively to an [`Outcome`]. Sets up the terminal,
/// runs the draw→input loop, and ALWAYS restores the terminal before returning.
/// `preselect` seeds multi-choice selections (resume); `step` is the optional
/// `(n, total)` shown in the footer.
pub fn run(
    question: Question,
    preselect: &[String],
    step: Option<(usize, usize)>,
) -> std::io::Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let result = run_event_loop(
        &mut terminal,
        Screen::new(question, preselect).with_step(step),
    );
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
                    // Ctrl-C is not SIGINT in raw mode — treat it as Cancel.
                    let msg = if is_ctrl_c(&key) {
                        Some(ScreenMsg::Cancel)
                    } else {
                        screen.key_to_msg(key.code)
                    };
                    if let Some(msg) = msg {
                        screen.update(msg);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Review screen — the editable "here's the whole plan" surface shown before
// provisioning (every run, including resume). The user can move to any row and
// Enter to edit it, press `c`/F to confirm, or Esc to cancel.
// ---------------------------------------------------------------------------

/// What the review screen returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// Proceed to provisioning with the plan as shown.
    Confirm,
    /// Re-ask this question to edit its value, then return to the review.
    Edit(QuestionId),
    /// Abort the run.
    Cancel,
}

/// A message the review screen reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMsg {
    Down,
    Up,
    /// Edit the focused row.
    Edit,
    /// Confirm + provision.
    Confirm,
    /// Cancel the run.
    Cancel,
}

/// The contextual header shown above the plan rows on the review screen — the
/// information that used to be printed as bash-style lines before the TUI
/// (version, workspace, the AWS identity being deployed into, and an optional
/// upgrade hint). Folding it into the review makes the interactive front door a
/// single cohesive screen. Pure data, so the header is asserted via `TestBackend`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewContext {
    /// The harness version (e.g. `0.1.2`).
    pub version: String,
    /// The workspace directory the run writes into.
    pub workspace: String,
    /// AWS identity lines, shown only when the plan touches AWS (account /
    /// profile / region, and the caller ARN or a creds-expired warning).
    pub aws: Vec<String>,
    /// Set when AWS credentials are missing/expired — rendered as a warning so
    /// the operator fixes them before confirming a cloud deploy.
    pub aws_warning: bool,
    /// A one-line "a newer release is available" hint, if the startup check
    /// found one.
    pub update_hint: Option<String>,
}

/// The review model: the rows + the cursor + the contextual header.
#[derive(Debug, Clone)]
pub struct ReviewScreen {
    pub rows: Vec<ReviewRow>,
    pub cursor: usize,
    pub outcome: Option<ReviewOutcome>,
    pub context: ReviewContext,
}

impl ReviewScreen {
    pub fn new(rows: Vec<ReviewRow>) -> Self {
        Self::with_context(rows, ReviewContext::default())
    }

    pub fn with_context(rows: Vec<ReviewRow>, context: ReviewContext) -> Self {
        Self {
            rows,
            cursor: 0,
            outcome: None,
            context,
        }
    }

    /// The header lines (version/workspace, AWS identity, upgrade hint) rendered
    /// above the plan rows. Pure, so the header content is unit-tested.
    fn header_lines(&self) -> Vec<Line<'static>> {
        let c = &self.context;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if !c.version.is_empty() || !c.workspace.is_empty() {
            lines.push(Line::from(vec![
                Span::from("ma-demo ").dim(),
                Span::from(c.version.clone()).bold(),
                Span::from("  ·  workspace ").dim(),
                Span::from(c.workspace.clone()).dim(),
            ]));
        }
        for (i, l) in c.aws.iter().enumerate() {
            let span = Span::from(l.clone());
            // The first AWS line is the account/profile/region summary; style it
            // as a warning when creds are missing/expired so it stands out.
            let span = if i == 0 && c.aws_warning {
                span.yellow()
            } else {
                span.dim()
            };
            lines.push(Line::from(span));
        }
        if let Some(h) = &c.update_hint {
            lines.push(Line::from(Span::from(h.clone()).yellow()));
        }
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        // The pre-provision reassurance. When the plan touches AWS (the `aws`
        // lines are present), confirming will create *billable* cloud resources,
        // so flag that in yellow instead of the calmer local reassurance.
        if c.aws.is_empty() {
            lines.push(Line::from(
                "Nothing is created yet. Review every setting below.".dim(),
            ));
        } else {
            lines.push(Line::from(
                "Confirming will create real, billable AWS resources. Review every setting below."
                    .yellow(),
            ));
        }
        lines.push(Line::from(""));
        lines
    }

    /// Map a key to a [`ReviewMsg`]. Up/down move; Enter/e edits the focused
    /// row; c/y confirm; Esc/q cancel. Pure, so the bindings are testable.
    pub fn key_to_msg(&self, code: ratatui::crossterm::event::KeyCode) -> Option<ReviewMsg> {
        use ratatui::crossterm::event::KeyCode::*;
        match code {
            Down | Char('j') => Some(ReviewMsg::Down),
            Up | Char('k') => Some(ReviewMsg::Up),
            Enter | Char('e') => Some(ReviewMsg::Edit),
            Char('c') | Char('y') => Some(ReviewMsg::Confirm),
            Esc | Char('q') => Some(ReviewMsg::Cancel),
            _ => None,
        }
    }

    /// The sole mutation point.
    pub fn update(&mut self, msg: ReviewMsg) {
        let n = self.rows.len().max(1);
        match msg {
            ReviewMsg::Down => self.cursor = (self.cursor + 1) % n,
            ReviewMsg::Up => self.cursor = (self.cursor + n - 1) % n,
            ReviewMsg::Edit => {
                if let Some(r) = self.rows.get(self.cursor) {
                    self.outcome = Some(ReviewOutcome::Edit(r.question));
                }
            }
            ReviewMsg::Confirm => self.outcome = Some(ReviewOutcome::Confirm),
            ReviewMsg::Cancel => self.outcome = Some(ReviewOutcome::Cancel),
        }
    }

    pub fn view(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
    }
}

impl Widget for &ReviewScreen {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let block =
            Block::bordered().title(" Review the plan — edit anything before provisioning ".bold());
        let inner = block.inner(area);
        block.render(area, buf);

        let header = self.header_lines();
        let [title_a, body_a, hint_a] = Layout::vertical([
            Constraint::Length(header.len() as u16),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        Paragraph::new(header).render(title_a, buf);

        let lines: Vec<Line> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let on = i == self.cursor;
                let caret = if on { "▶ " } else { "  " };
                let label = Span::from(format!("{caret}{:<18}", r.label));
                let label = if on { label.bold() } else { label };
                let mut line = Line::from(vec![label, Span::from(r.value.clone())]);
                if on {
                    line = line.reversed();
                }
                line
            })
            .collect();
        Paragraph::new(lines).render(body_a, buf);

        Paragraph::new("↑↓ move · Enter/e edit · c confirm & provision · q/Esc/^C cancel".dim())
            .render(hint_a, buf);
    }
}

/// Drive the review screen to a [`ReviewOutcome`]. Sets up + always restores
/// the terminal. `context` renders the version/workspace/AWS-identity header.
pub fn run_review(rows: Vec<ReviewRow>, context: ReviewContext) -> std::io::Result<ReviewOutcome> {
    let mut terminal = ratatui::try_init()?;
    let result = run_review_loop(&mut terminal, ReviewScreen::with_context(rows, context));
    ratatui::restore();
    result
}

fn run_review_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mut screen: ReviewScreen,
) -> std::io::Result<ReviewOutcome> {
    use ratatui::crossterm::event::{self, Event};
    loop {
        terminal.draw(|f| screen.view(f))?;
        if let Some(outcome) = &screen.outcome {
            return Ok(outcome.clone());
        }
        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.is_press() {
                    // Ctrl-C is not SIGINT in raw mode — treat it as Cancel.
                    let msg = if is_ctrl_c(&key) {
                        Some(ReviewMsg::Cancel)
                    } else {
                        screen.key_to_msg(key.code)
                    };
                    if let Some(msg) = msg {
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
        // Seed an explicit (non-default) selection so these toggle-mechanics
        // tests start from a known empty state, independent of default_on.
        Screen::new(q, &["__none__".to_string()])
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
    fn y_answers_yes_and_n_answers_no_regardless_of_cursor() {
        // Regression: `y`/`n` must force the value, not "confirm whatever the
        // cursor is on" — previously `y` while flipped to No confirmed No.
        let s = yesno();
        assert_eq!(
            s.key_to_msg(KeyCode::Char('y')),
            Some(ScreenMsg::ConfirmYesNo(true))
        );
        assert_eq!(
            s.key_to_msg(KeyCode::Char('n')),
            Some(ScreenMsg::ConfirmYesNo(false))
        );

        // Cursor on "No", press y → answers YES.
        let mut s = yesno();
        s.update(ScreenMsg::Down); // flip to No
        assert!(!s.yes);
        s.update(ScreenMsg::ConfirmYesNo(true));
        assert_eq!(s.outcome, Some(Outcome::Answered(Answer::Bool(true))));

        // Cursor on "Yes" (default), press n → answers NO.
        let mut s = yesno();
        s.update(ScreenMsg::ConfirmYesNo(false));
        assert_eq!(s.outcome, Some(Outcome::Answered(Answer::Bool(false))));
    }

    #[test]
    fn footer_shows_step_counter_when_set() {
        use ratatui::{backend::TestBackend, Terminal};
        let q = wizard::build(QuestionId::Target, &Answers::new());
        let s = Screen::new(q, &[]).with_step(Some((1, 9)));
        let mut t = Terminal::new(TestBackend::new(90, 16)).unwrap();
        t.draw(|f| s.view(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(text.contains("Step 1 of 9"), "footer shows step counter");
        assert!(text.contains("editable on the final review"));
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

    #[test]
    fn ctrl_c_is_detected_but_a_plain_c_is_not() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let plain_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(is_ctrl_c(&ctrl_c));
        assert!(is_ctrl_c(&ctrl_d));
        // A bare `c` is the review's "confirm" — it must NOT be read as Ctrl-C.
        assert!(!is_ctrl_c(&plain_c));
    }

    fn review_text(screen: &ReviewScreen, w: u16, h: u16) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| screen.view(f)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..buf.area().height)
            .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn review_rows() -> Vec<ReviewRow> {
        wizard::review_rows(&Answers::new())
    }

    #[test]
    fn review_header_shows_version_workspace_and_aws_identity() {
        let ctx = ReviewContext {
            version: "0.1.2".into(),
            workspace: "/tmp/ws".into(),
            aws: vec![
                "AWS profile=default  region=us-east-1".into(),
                "identity arn:aws:sts::874041194807:assumed-role/Foo/x  (account 874041194807)"
                    .into(),
            ],
            aws_warning: false,
            update_hint: None,
        };
        let screen = ReviewScreen::with_context(review_rows(), ctx);
        let text = review_text(&screen, 100, 30);
        assert!(text.contains("0.1.2"), "version in header");
        assert!(text.contains("/tmp/ws"), "workspace in header");
        assert!(text.contains("874041194807"), "AWS account in header");
        assert!(text.contains("Review the plan"), "title bar");
        // The plan rows still render.
        assert!(text.contains("Where"));
        // An AWS-touching plan flags the billable consequence.
        assert!(
            text.contains("billable AWS resources"),
            "cloud plan warns about billable resources"
        );
    }

    #[test]
    fn review_without_aws_uses_calm_reassurance_not_billing_warning() {
        let ctx = ReviewContext {
            version: "0.1.2".into(),
            workspace: "/tmp/ws".into(),
            aws: Vec::new(),
            aws_warning: false,
            update_hint: None,
        };
        let screen = ReviewScreen::with_context(review_rows(), ctx);
        let text = review_text(&screen, 100, 30);
        assert!(text.contains("Nothing is created yet"));
        assert!(!text.contains("billable AWS resources"));
    }

    #[test]
    fn review_header_shows_upgrade_hint_when_present() {
        let ctx = ReviewContext {
            version: "0.1.1".into(),
            workspace: "/tmp/ws".into(),
            aws: Vec::new(),
            aws_warning: false,
            update_hint: Some("A newer ma-demo is available: v0.1.2".into()),
        };
        let screen = ReviewScreen::with_context(review_rows(), ctx);
        let text = review_text(&screen, 100, 30);
        assert!(text.contains("newer ma-demo is available"));
    }

    #[test]
    fn review_without_context_has_no_header_but_still_renders_plan() {
        let screen = ReviewScreen::new(review_rows());
        let text = review_text(&screen, 100, 24);
        assert!(text.contains("Nothing is created yet"));
        assert!(text.contains("Where"));
    }
}

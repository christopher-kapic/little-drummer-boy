//! `/plans` pane — the read-only plan browser (plan.md §4.1).
//!
//! A two-level overlay modeled on [`crate::tui::sessions_pane`]: the root
//! level lists every plan (active first — `in_progress`, then `pending`,
//! then `done`, newest within each group), and pressing Enter / `→` drills
//! into one plan's **steps**, showing each step's dependency prerequisites
//! (the DAG), per-step status, and each step's tests with their `phase` +
//! `concurrency` (e.g. an `exclusive: port:8080` badge). `←` / Esc backs
//! out. The view is **read-only in v1**: no creating, editing, deleting, or
//! executing plans — authoring is the planner's job and execution controls
//! land in a later prompt (see the `execution-controls seam` note below).
//!
//! ## Data sources
//!
//! The pane is a socket client: the plan list and per-plan detail come from
//! the daemon's `ListPlans` / `PlanDetail` RPCs (plans live in the global
//! cockpit DB, same as sessions). Daemon down → an inline error, no crash —
//! the overlay always opens. Mirrors the sessions browser's blocking-fetch
//! shape via [`crate::tui::agent_runner`].
//!
//! ## Execution-controls seam (later prompt)
//!
//! [`PlansOutcome`] is the single channel the pane uses to ask the `App` to
//! act. v1 only emits `Close`. When plan *execution* controls (start /
//! pause / status of a running plan) land, they slot in as additional
//! `PlansOutcome` variants keyed off the highlighted plan (`selected_plan`)
//! — the `App` owns the daemon round-trip exactly as it owns
//! `attach_to_session` for `/sessions`. No execution logic lives here.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::daemon::proto::{PlanStepWire, PlanSummaryWire};
use crate::tui::agent_runner;
use crate::tui::theme::{ACCENT_BLUE_INDEX, MUTED_COLOR_INDEX};

/// What the pane asks the `App` to do after a key. v1 only closes; this is
/// the integration seam for later execution controls (start/pause/status),
/// which slot in as extra variants without touching the rest of the pane.
pub enum PlansOutcome {
    /// Close the pane back to chat.
    Close,
}

/// Which level of the browser is showing.
enum View {
    /// The root list of all plans.
    List {
        plans: Vec<PlanSummaryWire>,
        cursor: usize,
        scroll: usize,
    },
    /// One plan's step DAG (drilled in from the list).
    Detail {
        plan: PlanSummaryWire,
        steps: Vec<PlanStepWire>,
        scroll: usize,
    },
}

pub struct PlansPane {
    view: View,
    /// Last-loaded error (daemon unreachable, unknown plan, …), shown inline.
    error: Option<String>,
    /// Rendered body height + content rows at last draw (scroll clamp).
    last_body_height: usize,
    last_content_rows: usize,
}

impl PlansPane {
    /// Open the browser, loading the plan list. A load failure (daemon
    /// down) is non-fatal — the pane shows an inline message rather than
    /// refusing to open, matching `/sessions` and `/skills`.
    pub fn open() -> Self {
        let (plans, error) = match agent_runner::list_plans_blocking() {
            Ok(plans) => (plans, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Self {
            view: View::List {
                plans,
                cursor: 0,
                scroll: 0,
            },
            error,
            last_body_height: 0,
            last_content_rows: 0,
        }
    }

    /// The highlighted plan on the list level, if any. The seam later
    /// execution controls read to target the right plan.
    fn selected_plan(&self) -> Option<&PlanSummaryWire> {
        match &self.view {
            View::List { plans, cursor, .. } => plans.get(*cursor),
            View::Detail { plan, .. } => Some(plan),
        }
    }

    /// Handle a key. Returns `Some(outcome)` for close; `None` keeps the
    /// pane open. Always consumed by `App` (the modal rule).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PlansOutcome> {
        match &mut self.view {
            View::List { .. } => self.handle_list_key(key),
            View::Detail { .. } => self.handle_detail_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Option<PlansOutcome> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Some(PlansOutcome::Close),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            // Drill into the highlighted plan's steps.
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.drill_in(),
            _ => {}
        }
        None
    }

    fn handle_detail_key(&mut self, key: KeyEvent) -> Option<PlansOutcome> {
        match key.code {
            // Back to the list (never closes the pane from detail).
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h') => {
                self.drill_out();
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(),
            KeyCode::PageUp => {
                for _ in 0..self.last_body_height.max(1) {
                    self.scroll_up();
                }
            }
            KeyCode::PageDown => {
                for _ in 0..self.last_body_height.max(1) {
                    self.scroll_down();
                }
            }
            KeyCode::Char('g') => {
                if let View::Detail { scroll, .. } = &mut self.view {
                    *scroll = 0;
                }
            }
            KeyCode::Char('G') => {
                let max = self.last_content_rows.saturating_sub(self.last_body_height);
                if let View::Detail { scroll, .. } = &mut self.view {
                    *scroll = max;
                }
            }
            _ => {}
        }
        None
    }

    fn move_cursor(&mut self, delta: isize) {
        let View::List {
            plans,
            cursor,
            scroll,
        } = &mut self.view
        else {
            return;
        };
        let len = plans.len();
        if len == 0 {
            return;
        }
        let prev = *cursor;
        // Wrap at both ends, consistent with every other selectable list.
        *cursor = if delta < 0 {
            crate::tui::nav::wrap_prev(prev, len)
        } else {
            crate::tui::nav::wrap_next(prev, len)
        };
        // Rough scroll-follow; the render pass does the precise clamp.
        if delta < 0 {
            if *cursor > prev {
                *scroll = scroll.saturating_add(len);
            } else {
                *scroll = scroll.saturating_sub(1);
            }
        } else if *cursor < prev {
            *scroll = 0;
        } else {
            *scroll += 1;
        }
    }

    /// Drill into the highlighted plan's steps via the `PlanDetail` RPC.
    /// A fetch failure records the error inline and stays on the list.
    fn drill_in(&mut self) {
        let Some(plan) = self.selected_plan().cloned() else {
            return;
        };
        match agent_runner::plan_detail_blocking(plan.plan_id) {
            Ok((plan, steps)) => {
                self.error = None;
                self.view = View::Detail {
                    plan,
                    steps,
                    scroll: 0,
                };
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Pop back to the list level, re-fetching it so any change is fresh.
    fn drill_out(&mut self) {
        let (plans, error) = match agent_runner::list_plans_blocking() {
            Ok(plans) => (plans, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        self.error = error;
        self.view = View::List {
            plans,
            cursor: 0,
            scroll: 0,
        };
    }

    /// Mouse-wheel / key scroll (one row), only meaningful in detail (the
    /// list scrolls by cursor movement).
    pub fn scroll_up(&mut self) {
        match &mut self.view {
            View::Detail { scroll, .. } => *scroll = scroll.saturating_sub(1),
            View::List { scroll, .. } => *scroll = scroll.saturating_sub(1),
        }
    }

    pub fn scroll_down(&mut self) {
        let max = self.last_content_rows.saturating_sub(self.last_body_height);
        match &mut self.view {
            View::Detail { scroll, .. } => *scroll = (*scroll + 1).min(max),
            View::List { scroll, .. } => *scroll = (*scroll + 1).min(max),
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = match &self.view {
            View::List { .. } => Line::from(" /plans "),
            View::Detail { plan, .. } => Line::from(format!(" /plans › {} ", plan_display(plan))),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
        let body = layout[0];
        let help_area = layout[1];

        let lines = self.body_lines(body.width as usize);
        self.last_content_rows = lines.len();
        self.last_body_height = body.height as usize;
        let max_scroll = self.last_content_rows.saturating_sub(self.last_body_height);
        let scroll = self.current_scroll().min(max_scroll);
        self.set_scroll(scroll);
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), body);

        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        frame.render_widget(Paragraph::new(self.help_line()).style(muted), help_area);
    }

    fn current_scroll(&self) -> usize {
        match &self.view {
            View::List { scroll, .. } | View::Detail { scroll, .. } => *scroll,
        }
    }

    fn set_scroll(&mut self, v: usize) {
        match &mut self.view {
            View::List { scroll, .. } | View::Detail { scroll, .. } => *scroll = v,
        }
    }

    fn help_line(&self) -> Line<'static> {
        let text = match self.view {
            View::List { .. } => "q quit  ↑/↓ move  enter/→ open plan",
            View::Detail { .. } => "←/q back  ↑/↓ scroll  g/G top/bottom",
        };
        Line::from(text.to_string())
    }

    /// Assemble every body row as owned [`Line`]s. Pure aside from reading
    /// `self`; per-row assembly lives in the free helpers so it's
    /// unit-testable without a terminal.
    fn body_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(e) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("plans unavailable: {e}"),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::default());
        }
        match &self.view {
            View::List { plans, cursor, .. } => {
                if plans.is_empty() {
                    // Only the empty-state when there was no fetch error.
                    if self.error.is_none() {
                        lines.push(empty_state_line());
                    }
                    return lines;
                }
                for (i, p) in plans.iter().enumerate() {
                    lines.extend(plan_card_lines(p, i == *cursor, width));
                    lines.push(Line::default());
                }
            }
            View::Detail { plan, steps, .. } => {
                lines.extend(detail_lines(plan, steps));
            }
        }
        lines
    }
}

// ---- pure helpers ----------------------------------------------------------

/// The empty-state line: brief, pointing the user at `/plan` (per the spec).
fn empty_state_line() -> Line<'static> {
    Line::from(Span::styled(
        "  No plans yet. Use the planner (/plan) to create one.".to_string(),
        Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
    ))
}

/// Title when set, else the slug — the plan's display handle.
fn plan_display(p: &PlanSummaryWire) -> String {
    if !p.title.trim().is_empty() {
        p.title.clone()
    } else {
        p.slug.clone()
    }
}

/// Accent color for a plan/step status string.
fn status_color(status: &str) -> Color {
    match status {
        "in_progress" => Color::Cyan,
        "pending" => Color::Yellow,
        "done" => Color::Green,
        _ => Color::Indexed(MUTED_COLOR_INDEX),
    }
}

/// Terse status indicator (token-economy short).
fn status_label(status: &str) -> &'static str {
    match status {
        "in_progress" => "● in progress",
        "pending" => "● pending",
        "done" => "✓ done",
        _ => status_unknown(),
    }
}

fn status_unknown() -> &'static str {
    "● unknown"
}

/// Assemble one plan's rendered rows for the list level: a framed text
/// block (rounded corners, matching the sessions cards) with the title +
/// status, the branch + step count, and the one-line description.
fn plan_card_lines(p: &PlanSummaryWire, selected: bool, width: usize) -> Vec<Line<'static>> {
    let inner_w = width.saturating_sub(2).max(8);
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let border_style = if selected {
        Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX))
    } else {
        muted
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner_w)),
        border_style,
    )));

    // Row 1: title + status.
    out.push(boxed_row(
        vec![
            Span::styled(
                plan_display(p),
                if selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::raw("  "),
            Span::styled(
                status_label(&p.status).to_string(),
                Style::default().fg(status_color(&p.status)),
            ),
        ],
        inner_w,
        border_style,
    ));

    // Row 2: target branch + step count.
    let branch = p
        .target_branch
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "—".to_string());
    let steps = if p.step_count == 1 {
        "1 step".to_string()
    } else {
        format!("{} steps", p.step_count)
    };
    out.push(boxed_row(
        vec![
            Span::styled(format!("→ {branch}"), muted),
            Span::raw("  "),
            Span::styled(steps, muted),
        ],
        inner_w,
        border_style,
    ));

    // Row 3 (only when set): one-line description.
    if !p.description.trim().is_empty() {
        out.push(boxed_row(
            vec![Span::styled(
                p.description.clone(),
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            )],
            inner_w,
            border_style,
        ));
    }

    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
    )));
    out
}

/// Wrap a row's spans in vertical bars, padding to `inner_w` (truncating
/// when the content overruns the card width).
fn boxed_row(content: Vec<Span<'static>>, inner_w: usize, border: Style) -> Line<'static> {
    let used: usize = content.iter().map(|s| s.content.chars().count()).sum();
    let pad = inner_w.saturating_sub(used + 1); // +1 for the leading space
    let mut spans = vec![Span::styled("│".to_string(), border), Span::raw(" ")];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled("│".to_string(), border));
    Line::from(spans)
}

/// Assemble the plan-detail body: a header (description + branches), then
/// each step in DAG order with its status, dependency prerequisites, and
/// tests (phase + concurrency badges).
fn detail_lines(plan: &PlanSummaryWire, steps: &[PlanStepWire]) -> Vec<Line<'static>> {
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let mut out: Vec<Line<'static>> = Vec::new();

    // Header line: status + step count.
    out.push(Line::from(vec![
        Span::styled(
            status_label(&plan.status).to_string(),
            Style::default().fg(status_color(&plan.status)),
        ),
        Span::raw("  "),
        Span::styled(
            format!(
                "{} step{}",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
            muted,
        ),
    ]));
    if !plan.description.trim().is_empty() {
        out.push(Line::from(Span::styled(plan.description.clone(), muted)));
    }
    let base = plan.base_branch.clone().unwrap_or_else(|| "—".to_string());
    let target = plan
        .target_branch
        .clone()
        .unwrap_or_else(|| "—".to_string());
    out.push(Line::from(Span::styled(
        format!("base {base} → target {target}"),
        muted,
    )));
    out.push(Line::default());

    if steps.is_empty() {
        out.push(Line::from(Span::styled(
            "  This plan has no steps yet.".to_string(),
            muted,
        )));
        return out;
    }

    for (i, step) in steps.iter().enumerate() {
        // Step header: ordinal + title + status.
        out.push(Line::from(vec![
            Span::styled(
                format!("{}. ", i + 1),
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            ),
            Span::styled(
                step.title.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                status_label(&step.status).to_string(),
                Style::default().fg(status_color(&step.status)),
            ),
        ]));

        // Dependency prerequisites — the DAG edges into this step.
        if step.depends_on.is_empty() {
            out.push(Line::from(Span::styled(
                "    depends on: (none — can start first)".to_string(),
                muted,
            )));
        } else {
            out.push(Line::from(Span::styled(
                format!("    depends on: {}", step.depends_on.join(", ")),
                Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX)),
            )));
        }

        // Tests with phase + concurrency badges.
        for test in &step.tests {
            out.push(Line::from(vec![
                Span::styled("    test ".to_string(), muted),
                Span::styled(test.command.clone(), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(
                    format!("[{}]", test.phase),
                    Style::default().fg(Color::Magenta),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", test.concurrency),
                    Style::default().fg(concurrency_color(&test.concurrency)),
                ),
            ]));
        }

        out.push(Line::default());
    }
    out
}

/// Exclusive tests get a warning tint (they serialize on a shared
/// resource); parallel tests are muted.
fn concurrency_color(concurrency: &str) -> Color {
    if concurrency.starts_with("exclusive") {
        Color::Yellow
    } else {
        Color::Indexed(MUTED_COLOR_INDEX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::proto::PlanTestWire;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use uuid::Uuid;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn plan(slug: &str, title: &str, status: &str, steps: i64) -> PlanSummaryWire {
        PlanSummaryWire {
            plan_id: Uuid::new_v4(),
            slug: slug.into(),
            title: title.into(),
            description: format!("desc of {slug}"),
            status: status.into(),
            base_branch: Some("main".into()),
            target_branch: Some("cockpit-plan/feature".into()),
            step_count: steps,
            created_at: 0,
        }
    }

    fn list_pane(plans: Vec<PlanSummaryWire>) -> PlansPane {
        PlansPane {
            view: View::List {
                plans,
                cursor: 0,
                scroll: 0,
            },
            error: None,
            last_body_height: 100,
            last_content_rows: 0,
        }
    }

    fn render_text(pane: &PlansPane) -> String {
        pane.body_lines(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn list_shows_title_status_branch_stepcount_description() {
        let pane = list_pane(vec![plan("p1", "Ship the thing", "in_progress", 3)]);
        let text = render_text(&pane);
        assert!(text.contains("Ship the thing"), "title shown");
        assert!(text.contains("in progress"), "status shown");
        assert!(text.contains("cockpit-plan/feature"), "target branch shown");
        assert!(text.contains("3 steps"), "step count shown");
        assert!(text.contains("desc of p1"), "one-line description shown");
        assert!(text.contains("╭") && text.contains("╰"), "rounded card");
    }

    #[test]
    fn empty_state_points_at_planner() {
        let pane = list_pane(Vec::new());
        let text = render_text(&pane);
        assert!(text.contains("No plans yet"), "empty-state message");
        assert!(text.contains("/plan"), "points the user at /plan");
    }

    #[test]
    fn fetch_error_renders_inline_and_suppresses_empty_state() {
        let mut pane = list_pane(Vec::new());
        pane.error = Some("daemon not running".into());
        let text = render_text(&pane);
        assert!(text.contains("plans unavailable"));
        assert!(text.contains("daemon not running"));
        // The empty-state pointer is suppressed when the list is empty
        // *because of* an error (we don't claim "no plans" on a failure).
        assert!(!text.contains("No plans yet"));
    }

    #[test]
    fn esc_and_q_close_from_list() {
        let mut pane = list_pane(vec![plan("p", "P", "pending", 0)]);
        assert!(matches!(
            pane.handle_key(press(KeyCode::Esc)),
            Some(PlansOutcome::Close)
        ));
        let mut pane = list_pane(vec![plan("p", "P", "pending", 0)]);
        assert!(matches!(
            pane.handle_key(press(KeyCode::Char('q'))),
            Some(PlansOutcome::Close)
        ));
    }

    #[test]
    fn cursor_wraps_at_both_ends() {
        let mut pane = list_pane(vec![
            plan("a", "A", "pending", 0),
            plan("b", "B", "pending", 0),
            plan("c", "C", "pending", 0),
        ]);
        let cursor = |p: &PlansPane| match &p.view {
            View::List { cursor, .. } => *cursor,
            _ => unreachable!(),
        };
        assert_eq!(cursor(&pane), 0);
        pane.handle_key(press(KeyCode::Up));
        assert_eq!(cursor(&pane), 2, "up from first wraps to last");
        pane.handle_key(press(KeyCode::Down));
        assert_eq!(cursor(&pane), 0, "down from last wraps to first");
        pane.handle_key(press(KeyCode::Char('j')));
        assert_eq!(cursor(&pane), 1);
        pane.handle_key(press(KeyCode::Char('k')));
        assert_eq!(cursor(&pane), 0);
    }

    fn step(title: &str, status: &str, deps: &[&str], tests: Vec<PlanTestWire>) -> PlanStepWire {
        PlanStepWire {
            step_id: Uuid::new_v4(),
            title: title.into(),
            status: status.into(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            tests,
        }
    }

    fn detail_text(plan: &PlanSummaryWire, steps: &[PlanStepWire]) -> String {
        detail_lines(plan, steps)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn detail_shows_dag_status_and_test_badges() {
        let p = plan("multi", "Multi", "in_progress", 2);
        let steps = vec![
            step("schema", "done", &[], vec![]),
            step(
                "tools",
                "pending",
                &["schema"],
                vec![
                    PlanTestWire {
                        command: "cargo test".into(),
                        phase: "post_step".into(),
                        concurrency: "parallel".into(),
                    },
                    PlanTestWire {
                        command: "./it.sh".into(),
                        phase: "branch_stable".into(),
                        concurrency: "exclusive: port:8080".into(),
                    },
                ],
            ),
        ];
        let text = detail_text(&p, &steps);
        // Steps + ordering.
        assert!(text.contains("1. schema"));
        assert!(text.contains("2. tools"));
        // Per-step status.
        assert!(text.contains("done"));
        assert!(text.contains("pending"));
        // Dependency prerequisites (the DAG).
        assert!(
            text.contains("depends on: (none"),
            "root step shows no prerequisites"
        );
        assert!(
            text.contains("depends on: schema"),
            "downstream step names its prerequisite"
        );
        // Test phase + concurrency badges.
        assert!(text.contains("cargo test"));
        assert!(text.contains("[post_step]"));
        assert!(text.contains("[parallel]"));
        assert!(text.contains("[branch_stable]"));
        assert!(
            text.contains("[exclusive: port:8080]"),
            "exclusive concurrency badge with the resource key"
        );
    }

    #[test]
    fn detail_handles_a_plan_with_no_steps() {
        let p = plan("empty", "Empty", "pending", 0);
        let text = detail_text(&p, &[]);
        assert!(text.contains("no steps yet"));
    }

    #[test]
    fn enter_drills_in_without_daemon_records_error() {
        // No daemon under test: Enter attempts the PlanDetail fetch, which
        // fails, records the error inline, and stays on the list (never
        // panics, never closes).
        let mut pane = list_pane(vec![plan("p", "P", "pending", 1)]);
        let outcome = pane.handle_key(press(KeyCode::Enter));
        assert!(outcome.is_none(), "drill-in never closes the pane");
        assert!(matches!(pane.view, View::List { .. }), "stays on the list");
        assert!(pane.error.is_some(), "fetch failure surfaced inline");
    }
}

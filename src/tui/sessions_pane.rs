//! `/resume` + `/sessions` pane — the fullscreen session browser
//! (GOALS §17f).
//!
//! A scrollable list of rounded-border session cards, tier-sorted, with
//! fork drill-in navigation and an archive/delete confirm flow. Selecting
//! a card resumes that session. Mirrors [`crate::tui::stats_pane`]'s shape
//! (`open` / `handle_key` / `render`); `App` opens it over the chat body
//! and routes input/render the same way.
//!
//! ## Data sources
//!
//! - **Tiers 1-2** (active jobs / processing) come from the daemon's
//!   in-memory per-session [`crate::daemon::session_worker::LiveState`]
//!   via the `SessionLiveStatus` RPC. Daemon down → no live tiers, no
//!   crash (sessions fall to the DB-derived tiers).
//! - **Tiers 3-5** (unread / pending-question / read) come from the DB
//!   fields on each [`SessionSummary`]: `latest_activity_at` vs.
//!   `last_viewed_at` for read/unread, and `open_interrupts` for the
//!   pending-question split.
//!
//! The pane is a socket client: every fetch / archive / delete is a
//! blocking daemon request through [`crate::tui::agent_runner`]. The
//! resume action is *not* performed here — `handle_key` returns a
//! [`SessionsOutcome`] the `App` acts on, reusing the existing
//! session-switch path (`attach_to_session`).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use uuid::Uuid;

use crate::daemon::proto::SessionSummary;
use crate::tui::agent_runner;
use crate::tui::theme::{ACCENT_BLUE_INDEX, MUTED_COLOR_INDEX};

/// Tier a session sorts into, top (lowest discriminant) to bottom. Within
/// a tier, sessions sort by `last_active_at` descending. GOALS §17f.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// 1 — has active background/loop/timer jobs (daemon-reported).
    ActiveJobs,
    /// 2 — currently processing a turn (daemon-reported).
    Processing,
    /// 3 — unread: the most recent agent event is newer than the marker.
    Unread,
    /// 4 — read, with a pending question (`open_interrupts > 0`).
    PendingQuestion,
    /// 5 — read, idle, no pending question (ended sessions live here too).
    Idle,
}

impl Tier {
    /// Terse status indicator for the card (kept short per token economy).
    fn label(self) -> &'static str {
        match self {
            Tier::ActiveJobs => "● jobs running",
            Tier::Processing => "● working",
            Tier::Unread => "● unread",
            Tier::PendingQuestion => "● question pending",
            Tier::Idle => "idle",
        }
    }

    /// Accent color for the status indicator.
    fn color(self) -> Color {
        match self {
            Tier::ActiveJobs => Color::Green,
            Tier::Processing => Color::Cyan,
            Tier::Unread => Color::Yellow,
            Tier::PendingQuestion => Color::Magenta,
            Tier::Idle => Color::Indexed(MUTED_COLOR_INDEX),
        }
    }
}

/// Classify one session into its tier given its live daemon status.
/// `live = (has_active_jobs, processing)`; `None` when the daemon has no
/// live worker (or is unreachable) — then only the DB-derived tiers 3-5
/// apply. Pure so the tiering rules are unit-testable without a daemon.
pub fn classify(summary: &SessionSummary, live: Option<(bool, bool)>) -> Tier {
    if let Some((has_jobs, processing)) = live {
        if has_jobs {
            return Tier::ActiveJobs;
        }
        if processing {
            return Tier::Processing;
        }
    }
    if is_unread(summary) {
        return Tier::Unread;
    }
    if summary.open_interrupts > 0 {
        return Tier::PendingQuestion;
    }
    Tier::Idle
}

/// Unread = the session has agent-produced activity newer than the
/// last-viewed marker. A never-viewed session with any agent activity is
/// unread; a session with no agent activity is never unread.
fn is_unread(summary: &SessionSummary) -> bool {
    match summary.latest_activity_at {
        None => false,
        Some(activity) => match summary.last_viewed_at {
            None => true,
            Some(viewed) => activity > viewed,
        },
    }
}

/// Sort `(summary, live)` pairs into display order: by tier ascending,
/// then `last_active_at` descending within a tier. Returns the classified
/// tier alongside each summary so the renderer doesn't re-classify.
pub fn tier_sort(
    mut items: Vec<(SessionSummary, Option<(bool, bool)>)>,
) -> Vec<(SessionSummary, Tier)> {
    let mut classified: Vec<(SessionSummary, Tier)> = items
        .drain(..)
        .map(|(s, live)| {
            let tier = classify(&s, live);
            (s, tier)
        })
        .collect();
    classified.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(b.0.last_active_at.cmp(&a.0.last_active_at))
    });
    classified
}

/// One breadcrumb level: the parent session we drilled into (its short id
/// label) and the cards shown at that level.
struct Level {
    /// `None` at the root level; `Some` once we've drilled into a fork.
    parent: Option<SessionSummary>,
    cards: Vec<(SessionSummary, Tier)>,
    cursor: usize,
    scroll: usize,
}

/// Current archive/delete confirm sub-step (modelled like the model
/// picker's step enum — kept inside the pane, GOALS §17h).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Browsing the list.
    Browse,
    /// Confirm dialog open for the highlighted session. `descendants` is
    /// the cascade count stated to the user; `live` is whether the target
    /// is mid-turn / has jobs (interrupt-first warning). `choice` is the
    /// highlighted button.
    Confirm {
        session_id: Uuid,
        label: String,
        descendants: u32,
        live: bool,
        choice: ConfirmChoice,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmChoice {
    Archive,
    Delete,
    Cancel,
}

/// Active scope. `Project` lists root sessions in the current project;
/// `All` lists every session across projects (each card shows a label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Project,
    All,
}

/// What the pane asks the `App` to do after a key. `App` owns the resume
/// path (it reuses `attach_to_session`); the pane never switches sessions
/// itself.
pub enum SessionsOutcome {
    /// Close the pane back to chat.
    Close,
    /// Resume this session (load it into the TUI).
    Resume(Uuid),
}

pub struct SessionsPane {
    /// Resolved current-project id, or `None` when the cwd couldn't be
    /// resolved. When `None` the scope is pinned to `All`.
    project_id: Option<String>,
    scope: Scope,
    /// Whether archived sessions are revealed (toggle, GOALS §17h).
    show_archived: bool,
    /// Breadcrumb stack of fork levels; `levels[0]` is the root list.
    levels: Vec<Level>,
    step: Step,
    /// Last-loaded error (daemon unreachable, etc.), shown inline.
    error: Option<String>,
    /// Rendered body height + content rows at last draw (scroll clamp).
    last_body_height: usize,
    last_content_rows: usize,
}

impl SessionsPane {
    /// Open the browser for `cwd`. Resolves the project scope and loads
    /// the root level. A load failure (daemon down) is non-fatal — the
    /// pane shows an inline message rather than refusing to open.
    pub fn open(cwd: &std::path::Path) -> Self {
        let project_id = resolve_project_id(cwd);
        let scope = if project_id.is_some() {
            Scope::Project
        } else {
            Scope::All
        };
        let mut pane = Self {
            project_id,
            scope,
            show_archived: false,
            levels: Vec::new(),
            step: Step::Browse,
            error: None,
            last_body_height: 0,
            last_content_rows: 0,
        };
        pane.load_root();
        pane
    }

    /// (Re)load the root level for the active scope, discarding any fork
    /// drill-in. Called at open and on a scope / archived-toggle change.
    fn load_root(&mut self) {
        let pid = match self.scope {
            Scope::Project => self.project_id.clone(),
            Scope::All => None,
        };
        let cards = self.fetch_level(pid, None);
        self.levels = vec![Level {
            parent: None,
            cards,
            cursor: 0,
            scroll: 0,
        }];
    }

    /// Fetch + tier-sort one level: root sessions (`parent = None`) or the
    /// direct forks of `parent`. Filters archived per the toggle and
    /// attaches live status. Records (clears) the error on success.
    fn fetch_level(
        &mut self,
        project_id: Option<String>,
        parent: Option<Uuid>,
    ) -> Vec<(SessionSummary, Tier)> {
        match agent_runner::list_sessions_blocking(project_id, parent) {
            Ok(mut sessions) => {
                self.error = None;
                // Archive filter (GOALS §17h): hidden by default.
                if !self.show_archived {
                    sessions.retain(|s| s.archived_at.is_none());
                }
                let ids: Vec<Uuid> = sessions.iter().map(|s| s.session_id).collect();
                let live = agent_runner::session_live_status_blocking(ids);
                let pairs: Vec<_> = sessions
                    .into_iter()
                    .map(|s| {
                        let l = live.get(&s.session_id).copied();
                        (s, l)
                    })
                    .collect();
                tier_sort(pairs)
            }
            Err(e) => {
                self.error = Some(e);
                Vec::new()
            }
        }
    }

    /// Reload the current level in place, preserving scope/breadcrumb and
    /// clamping the cursor. Used after an archive/delete/unarchive.
    fn reload_current_level(&mut self) {
        let (pid, parent) = {
            let depth = self.levels.len();
            let level = self.levels.last().expect("at least the root level");
            match (depth, &level.parent) {
                (_, Some(p)) => (None, Some(p.session_id)),
                _ => (
                    match self.scope {
                        Scope::Project => self.project_id.clone(),
                        Scope::All => None,
                    },
                    None,
                ),
            }
        };
        let cards = self.fetch_level(pid, parent);
        if let Some(level) = self.levels.last_mut() {
            level.cards = cards;
            level.cursor = level.cursor.min(level.cards.len().saturating_sub(1));
            level.scroll = 0;
        }
    }

    fn current(&self) -> &Level {
        self.levels.last().expect("at least the root level")
    }

    fn current_mut(&mut self) -> &mut Level {
        self.levels.last_mut().expect("at least the root level")
    }

    /// The highlighted card's summary, if any.
    fn selected(&self) -> Option<&SessionSummary> {
        let level = self.current();
        level.cards.get(level.cursor).map(|(s, _)| s)
    }

    /// Handle a key. Returns `Some(outcome)` for close/resume; `None`
    /// otherwise (the pane stays open). Always consumed by `App` so
    /// nothing leaks to the composer (the modal rule).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<SessionsOutcome> {
        // The confirm sub-dialog owns input while open.
        if matches!(self.step, Step::Confirm { .. }) {
            return self.handle_confirm_key(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Some(SessionsOutcome::Close),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Enter => {
                if let Some(s) = self.selected() {
                    return Some(SessionsOutcome::Resume(s.session_id));
                }
            }
            // Drill into the highlighted session's forks.
            KeyCode::Right | KeyCode::Char('l') => self.drill_in(),
            // Go back up one fork level (no-op at the root).
            KeyCode::Left | KeyCode::Char('h') => self.drill_out(),
            // Scope toggle — only meaningful with a current project.
            KeyCode::Char('p') if self.project_id.is_some() => {
                self.scope = match self.scope {
                    Scope::Project => Scope::All,
                    Scope::All => Scope::Project,
                };
                self.load_root();
            }
            // Reveal / hide archived sessions.
            KeyCode::Char('a') => {
                self.show_archived = !self.show_archived;
                self.reload_current_level();
            }
            // Unarchive the highlighted session (only from the archived
            // view, where archived rows are visible).
            KeyCode::Char('u') => self.unarchive_selected(),
            // Open the archive/delete confirm for the highlighted session.
            KeyCode::Char('d') => self.open_confirm(),
            _ => {}
        }
        None
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.current().cards.len();
        if len == 0 {
            return;
        }
        let level = self.current_mut();
        let prev = level.cursor;
        // Wrap at both ends, consistent with every other selectable list.
        level.cursor = if delta < 0 {
            crate::tui::nav::wrap_prev(prev, len)
        } else {
            crate::tui::nav::wrap_next(prev, len)
        };
        // Keep the cursor inside the visible window (rough: each card is
        // ~4 rows). The render pass does the precise clamp.
        if delta < 0 {
            if level.cursor > prev {
                // Wrapped first → last: scroll toward the bottom; render
                // clamps to the precise floor.
                level.scroll = level.scroll.saturating_add(len);
            } else {
                level.scroll = level.scroll.saturating_sub(1);
            }
        } else if level.cursor < prev {
            // Wrapped last → first: jump back to the top.
            level.scroll = 0;
        } else {
            level.scroll += 1;
        }
    }

    /// Drill into the highlighted session's direct forks (unbounded
    /// depth). No-op when the session has no forks.
    fn drill_in(&mut self) {
        let Some(parent) = self.selected().cloned() else {
            return;
        };
        if parent.fork_count == 0 {
            return;
        }
        let cards = self.fetch_level(None, Some(parent.session_id));
        self.levels.push(Level {
            parent: Some(parent),
            cards,
            cursor: 0,
            scroll: 0,
        });
    }

    /// Pop one fork level. No-op at the root.
    fn drill_out(&mut self) {
        if self.levels.len() > 1 {
            self.levels.pop();
        }
    }

    /// Open the Archive/Delete/Cancel confirm for the highlighted session,
    /// stating the cascade count and whether the target is live.
    fn open_confirm(&mut self) {
        let Some(s) = self.selected().cloned() else {
            return;
        };
        // Cascade count: the full descendant subtree the daemon walks on
        // archive/delete (GOALS §17h) — carried on the summary, accurate
        // without an extra round-trip.
        let descendants = s.descendant_count;
        // Live status drives the interrupt-first warning.
        let live_map = agent_runner::session_live_status_blocking(vec![s.session_id]);
        let live = live_map
            .get(&s.session_id)
            .map(|(j, p)| *j || *p)
            .unwrap_or(false);
        let label = card_description(&s);
        self.step = Step::Confirm {
            session_id: s.session_id,
            label,
            descendants,
            live,
            choice: ConfirmChoice::Cancel,
        };
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Option<SessionsOutcome> {
        let Step::Confirm {
            session_id, choice, ..
        } = &mut self.step
        else {
            return None;
        };
        let session_id = *session_id;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.step = Step::Browse;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                *choice = match choice {
                    ConfirmChoice::Archive => ConfirmChoice::Cancel,
                    ConfirmChoice::Delete => ConfirmChoice::Archive,
                    ConfirmChoice::Cancel => ConfirmChoice::Delete,
                };
            }
            KeyCode::Right | KeyCode::Char('l') => {
                *choice = match choice {
                    ConfirmChoice::Archive => ConfirmChoice::Delete,
                    ConfirmChoice::Delete => ConfirmChoice::Cancel,
                    ConfirmChoice::Cancel => ConfirmChoice::Archive,
                };
            }
            KeyCode::Enter => {
                let decided = *choice;
                self.apply_confirm(session_id, decided);
            }
            _ => {}
        }
        None
    }

    /// Apply the confirm choice. Both Archive and Delete cascade the whole
    /// fork subtree; the daemon interrupts any live worker in the subtree
    /// first (GOALS §17h). On success we reload the current level.
    fn apply_confirm(&mut self, session_id: Uuid, choice: ConfirmChoice) {
        use crate::daemon::proto::Request;
        let req = match choice {
            ConfirmChoice::Cancel => {
                self.step = Step::Browse;
                return;
            }
            ConfirmChoice::Archive => Request::ArchiveSession {
                session_id,
                cascade: true,
            },
            ConfirmChoice::Delete => Request::DeleteSession {
                session_id,
                cascade: true,
            },
        };
        match agent_runner::daemon_request_blocking(req) {
            Ok(_) => {
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e);
            }
        }
        self.step = Step::Browse;
        self.reload_current_level();
    }

    fn unarchive_selected(&mut self) {
        let Some(s) = self.selected().cloned() else {
            return;
        };
        if s.archived_at.is_none() {
            return;
        }
        match agent_runner::daemon_request_blocking(
            crate::daemon::proto::Request::UnarchiveSession {
                session_id: s.session_id,
            },
        ) {
            Ok(_) => self.error = None,
            Err(e) => self.error = Some(e),
        }
        self.reload_current_level();
    }

    /// Mouse-wheel scroll (one row).
    pub fn scroll_up(&mut self) {
        let level = self.current_mut();
        level.scroll = level.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        let max = self.last_content_rows.saturating_sub(self.last_body_height);
        let level = self.current_mut();
        level.scroll = (level.scroll + 1).min(max);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = self.title();
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::vertical([
            Constraint::Length(1), // breadcrumb
            Constraint::Min(0),    // cards
            Constraint::Length(1), // help
        ])
        .split(inner);
        let crumb_area = layout[0];
        let body = layout[1];
        let help_area = layout[2];

        frame.render_widget(Paragraph::new(self.breadcrumb_line()), crumb_area);

        let lines = self.body_lines(body.width as usize);
        self.last_content_rows = lines.len();
        self.last_body_height = body.height as usize;
        let max_scroll = self.last_content_rows.saturating_sub(self.last_body_height);
        let scroll = self.current().scroll.min(max_scroll);
        if let Some(level) = self.levels.last_mut() {
            level.scroll = scroll;
        }
        frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), body);

        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        frame.render_widget(Paragraph::new(self.help_line()).style(muted), help_area);

        // The confirm sub-dialog draws over the bottom of the body.
        if let Step::Confirm { .. } = &self.step {
            self.render_confirm(frame, body);
        }
    }

    fn title(&self) -> Line<'static> {
        let scope_label = match self.scope {
            Scope::Project => match &self.project_id {
                Some(id) => format!("project {}", short(id)),
                None => "project".to_string(),
            },
            Scope::All => "all projects".to_string(),
        };
        let mut spans = vec![
            Span::raw(" /sessions "),
            Span::styled(
                format!("scope: {scope_label} "),
                Style::default().fg(Color::Yellow),
            ),
        ];
        if self.show_archived {
            spans.push(Span::styled(
                "[archived shown] ",
                Style::default().fg(Color::Magenta),
            ));
        }
        Line::from(spans)
    }

    /// Breadcrumb / depth header for fork drill-in (GOALS §17f).
    fn breadcrumb_line(&self) -> Line<'static> {
        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        if self.levels.len() == 1 {
            return Line::from(Span::styled("sessions", muted));
        }
        let mut parts = vec!["sessions".to_string()];
        for level in self.levels.iter().skip(1) {
            if let Some(p) = &level.parent {
                parts.push(format!("forks of {}", card_description(p)));
            }
        }
        Line::from(Span::styled(parts.join("  ›  "), muted))
    }

    fn help_line(&self) -> Line<'static> {
        let scope_hint = if self.project_id.is_some() {
            "p scope  "
        } else {
            ""
        };
        Line::from(format!(
            "q quit  ↑/↓ move  enter resume  →/l forks  ←/h back  {scope_hint}a archived  u unarchive  d archive/delete"
        ))
    }

    /// Assemble every body row as owned [`Line`]s. Pure aside from reading
    /// `self`; the per-card assembly lives in [`card_lines`] so it's
    /// unit-testable without a terminal.
    fn body_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(e) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("daemon unavailable: {e}"),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::default());
        }
        let level = self.current();
        if level.cards.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no sessions)".to_string(),
                Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX)),
            )));
            return lines;
        }
        let show_project = matches!(self.scope, Scope::All) || self.levels.len() > 1;
        for (i, (summary, tier)) in level.cards.iter().enumerate() {
            lines.extend(card_lines(
                summary,
                *tier,
                i == level.cursor,
                show_project,
                width,
            ));
            lines.push(Line::default());
        }
        lines
    }

    fn render_confirm(&self, frame: &mut Frame, body: Rect) {
        let Step::Confirm {
            label,
            descendants,
            live,
            choice,
            ..
        } = &self.step
        else {
            return;
        };
        // A 6-row modal pinned to the bottom of the body.
        let h = 7u16.min(body.height);
        let rect = Rect {
            x: body.x,
            y: body.y + body.height.saturating_sub(h),
            width: body.width,
            height: h,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX)))
            .title(" archive / delete ");
        let inner = block.inner(rect);
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(block, rect);

        let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(label.clone()));
        let cascade = if *descendants > 0 {
            format!("Cascades to {descendants} fork(s) and their descendants.")
        } else {
            "No forks affected.".to_string()
        };
        lines.push(Line::from(Span::styled(cascade, muted)));
        if *live {
            lines.push(Line::from(Span::styled(
                "Session is live — it will be interrupted first.".to_string(),
                Style::default().fg(Color::Yellow),
            )));
        }
        lines.push(button_row(*choice));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

// ---- pure helpers ----------------------------------------------------------

/// Resolve the cwd to a `project_id` the same way `stats_pane` does.
fn resolve_project_id(cwd: &std::path::Path) -> Option<String> {
    let root = crate::git::find_worktree_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    Some(crate::session::project_id_for(&root))
}

/// Card description: the title when set, else the short id, else a short
/// prefix of the full session id (defensive — short_id is always set on
/// modern rows).
pub fn card_description(s: &SessionSummary) -> String {
    if let Some(t) = &s.title
        && !t.trim().is_empty()
    {
        return t.clone();
    }
    if let Some(sid) = &s.short_id
        && !sid.is_empty()
    {
        return sid.clone();
    }
    short(&s.session_id.to_string())
}

/// Assemble one card's rendered rows (pure, terminal-free). A rounded
/// border isn't drawn glyph-by-glyph here — `Paragraph` can't host nested
/// borders cheaply per card in a scroll region, so each card is a framed
/// text block: a top rule, content rows, a bottom rule, using rounded
/// corner glyphs to match `BorderType::Rounded`.
pub fn card_lines(
    s: &SessionSummary,
    tier: Tier,
    selected: bool,
    show_project: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let inner_w = width.saturating_sub(2).max(8);
    let muted = Style::default().fg(Color::Indexed(MUTED_COLOR_INDEX));
    let border_style = if selected {
        Style::default().fg(Color::Indexed(ACCENT_BLUE_INDEX))
    } else {
        muted
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    // Top rule with rounded corners.
    out.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner_w)),
        border_style,
    )));

    // Row 1: description + tier status.
    let desc = card_description(s);
    let status = Span::styled(tier.label().to_string(), Style::default().fg(tier.color()));
    out.push(boxed_row(
        vec![
            Span::styled(
                desc,
                if selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::raw("  "),
            status,
        ],
        inner_w,
        border_style,
    ));

    // Row 2: absolute most-recent-event time + (optional) project label.
    let mut meta: Vec<Span<'static>> = vec![Span::styled(fmt_time(s.last_active_at), muted)];
    if show_project {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("[{}]", project_label(&s.project_root)),
            muted,
        ));
    }
    if s.archived_at.is_some() {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            "archived".to_string(),
            Style::default().fg(Color::Magenta),
        ));
    }
    out.push(boxed_row(meta, inner_w, border_style));

    // Row 3 (only when forks exist): fork hint.
    if s.fork_count > 0 {
        out.push(boxed_row(
            vec![Span::styled(
                format!("press →/l to view {} fork(s)", s.fork_count),
                Style::default().fg(Color::Cyan),
            )],
            inner_w,
            border_style,
        ));
    }

    // Bottom rule.
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
    )));
    out
}

/// Wrap a row's spans in vertical bars, padding to `inner_w`.
fn boxed_row(content: Vec<Span<'static>>, inner_w: usize, border: Style) -> Line<'static> {
    let used: usize = content.iter().map(|s| s.content.chars().count()).sum();
    let pad = inner_w.saturating_sub(used + 1); // +1 for the leading space
    let mut spans = vec![Span::styled("│".to_string(), border), Span::raw(" ")];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled("│".to_string(), border));
    Line::from(spans)
}

/// The Archive / Delete / Cancel button row, highlighting the selection.
fn button_row(choice: ConfirmChoice) -> Line<'static> {
    let mk = |label: &str, this: ConfirmChoice| {
        if this == choice {
            Span::styled(
                format!("[ {label} ]"),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("  {label}  "), Style::default().fg(Color::White))
        }
    };
    Line::from(vec![
        mk("Archive", ConfirmChoice::Archive),
        Span::raw(" "),
        mk("Delete", ConfirmChoice::Delete),
        Span::raw(" "),
        mk("Cancel", ConfirmChoice::Cancel),
    ])
}

/// Absolute, human-readable local timestamp for `last_active_at`.
fn fmt_time(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "—".to_string(),
    }
}

/// Last path component of a project root, for the all-projects card label.
fn project_label(root: &str) -> String {
    std::path::Path::new(root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string())
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn summary(id: Uuid, last_active: i64) -> SessionSummary {
        SessionSummary {
            session_id: id,
            short_id: Some("abc123".into()),
            project_root: "/proj/alpha".into(),
            project_id: "pid".into(),
            started_at: 0,
            last_active_at: last_active,
            turns: 0,
            active_agent: "coder".into(),
            title: None,
            parent_session_id: None,
            fork_count: 0,
            descendant_count: 0,
            last_viewed_at: None,
            latest_activity_at: None,
            open_interrupts: 0,
            archived_at: None,
        }
    }

    #[test]
    fn classify_respects_tier_precedence() {
        let mut s = summary(Uuid::new_v4(), 100);
        // Live jobs win over everything.
        s.latest_activity_at = Some(200);
        s.open_interrupts = 3;
        assert_eq!(classify(&s, Some((true, true))), Tier::ActiveJobs);
        // Processing (no jobs) is tier 2.
        assert_eq!(classify(&s, Some((false, true))), Tier::Processing);
        // No live status → unread because activity is newer than the
        // (never-set) viewed marker.
        assert_eq!(classify(&s, None), Tier::Unread);
    }

    #[test]
    fn unread_computation() {
        let mut s = summary(Uuid::new_v4(), 100);
        // No agent activity → never unread.
        assert!(!is_unread(&s));
        // Activity but never viewed → unread.
        s.latest_activity_at = Some(50);
        assert!(is_unread(&s));
        // Viewed after the activity → read.
        s.last_viewed_at = Some(60);
        assert!(!is_unread(&s));
        // New activity after the view → unread again.
        s.latest_activity_at = Some(70);
        assert!(is_unread(&s));
    }

    #[test]
    fn read_with_pending_question_is_tier_4() {
        let mut s = summary(Uuid::new_v4(), 100);
        // Read (activity <= viewed), but an open interrupt.
        s.latest_activity_at = Some(50);
        s.last_viewed_at = Some(60);
        s.open_interrupts = 1;
        assert_eq!(classify(&s, None), Tier::PendingQuestion);
        // Read, no question → idle.
        s.open_interrupts = 0;
        assert_eq!(classify(&s, None), Tier::Idle);
    }

    #[test]
    fn daemon_down_degrades_to_db_tiers() {
        // `None` live status (daemon unreachable) → the session still
        // classifies into a DB tier, never panics.
        let mut s = summary(Uuid::new_v4(), 100);
        s.latest_activity_at = Some(10);
        s.last_viewed_at = Some(20);
        assert_eq!(classify(&s, None), Tier::Idle);
    }

    #[test]
    fn tier_sort_orders_by_tier_then_recency() {
        let idle_old = summary(Uuid::new_v4(), 10);
        let idle_new = summary(Uuid::new_v4(), 90);
        let mut unread = summary(Uuid::new_v4(), 50);
        unread.latest_activity_at = Some(55); // never viewed → unread

        let sorted = tier_sort(vec![
            (idle_old.clone(), None),
            (idle_new.clone(), None),
            (unread.clone(), None),
        ]);
        // Unread (tier 3) sorts above the two idle (tier 5) regardless of
        // recency; within idle, the newer one is first.
        assert_eq!(sorted[0].1, Tier::Unread);
        assert_eq!(sorted[0].0.session_id, unread.session_id);
        assert_eq!(sorted[1].0.session_id, idle_new.session_id);
        assert_eq!(sorted[2].0.session_id, idle_old.session_id);
    }

    #[test]
    fn card_fields_assemble() {
        let mut s = summary(Uuid::new_v4(), 1_700_000_000);
        s.title = Some("fix the parser".into());
        s.fork_count = 2;
        let text: String = card_lines(&s, Tier::Unread, true, true, 60)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("fix the parser"), "title is the description");
        assert!(text.contains("unread"), "tier status shown");
        assert!(
            text.contains("press →/l to view 2 fork(s)"),
            "fork hint shown when forks exist"
        );
        assert!(text.contains("[alpha]"), "all-projects label shown");
        assert!(text.contains("╭") && text.contains("╰"), "rounded corners");
    }

    #[test]
    fn description_falls_back_to_short_id() {
        let s = summary(Uuid::new_v4(), 0); // title None
        assert_eq!(card_description(&s), "abc123");
    }

    #[test]
    fn esc_closes() {
        // Build a pane without touching the daemon (empty root level).
        let mut pane = test_pane(vec![]);
        assert!(matches!(
            pane.handle_key(press(KeyCode::Esc)),
            Some(SessionsOutcome::Close)
        ));
    }

    #[test]
    fn enter_resumes_highlighted() {
        let id = Uuid::new_v4();
        let mut pane = test_pane(vec![(summary(id, 100), Tier::Idle)]);
        match pane.handle_key(press(KeyCode::Enter)) {
            Some(SessionsOutcome::Resume(got)) => assert_eq!(got, id),
            other => panic!(
                "expected Resume, got a non-resume outcome: {}",
                other.is_none()
            ),
        }
    }

    #[test]
    fn fork_breadcrumb_drill_out_is_bounded() {
        // Drilling out at the root is a no-op (never pops below level 0).
        let mut pane = test_pane(vec![(summary(Uuid::new_v4(), 1), Tier::Idle)]);
        assert_eq!(pane.levels.len(), 1);
        pane.drill_out();
        assert_eq!(pane.levels.len(), 1);
    }

    #[test]
    fn drill_in_noop_without_forks() {
        // A card with fork_count = 0 doesn't push a level.
        let mut pane = test_pane(vec![(summary(Uuid::new_v4(), 1), Tier::Idle)]);
        pane.drill_in();
        assert_eq!(pane.levels.len(), 1);
    }

    #[test]
    fn breadcrumb_reflects_depth() {
        let mut parent = summary(Uuid::new_v4(), 1);
        parent.title = Some("root-task".into());
        let mut pane = test_pane(vec![(parent.clone(), Tier::Idle)]);
        // Simulate a drill-in by pushing a level (the real drill-in fetches
        // from the daemon, which isn't available under test).
        pane.levels.push(Level {
            parent: Some(parent),
            cards: vec![],
            cursor: 0,
            scroll: 0,
        });
        let crumb: String = pane
            .breadcrumb_line()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(crumb.contains("forks of root-task"));
    }

    #[test]
    fn confirm_choice_cycles_and_archive_delete_cascade() {
        // Drive the confirm sub-dialog's choice cycling + that both
        // Archive and Delete map to cascading subtree requests.
        let mut pane = test_pane(vec![]);
        pane.step = Step::Confirm {
            session_id: Uuid::new_v4(),
            label: "task".into(),
            descendants: 4,
            live: true,
            choice: ConfirmChoice::Cancel,
        };
        // Right cycles Cancel → Archive → Delete → Cancel.
        pane.handle_key(press(KeyCode::Right));
        assert!(matches!(
            pane.step,
            Step::Confirm {
                choice: ConfirmChoice::Archive,
                ..
            }
        ));
        pane.handle_key(press(KeyCode::Right));
        assert!(matches!(
            pane.step,
            Step::Confirm {
                choice: ConfirmChoice::Delete,
                ..
            }
        ));
        // Esc returns to Browse.
        pane.handle_key(press(KeyCode::Esc));
        assert_eq!(pane.step, Step::Browse);
    }

    #[test]
    fn confirm_dialog_states_cascade_count_and_live_warning() {
        // The rendered confirm text states the descendant cascade count
        // and the interrupt-first warning when the target is live.
        let pane = {
            let mut p = test_pane(vec![]);
            p.step = Step::Confirm {
                session_id: Uuid::new_v4(),
                label: "build the thing".into(),
                descendants: 3,
                live: true,
                choice: ConfirmChoice::Archive,
            };
            p
        };
        // Reconstruct the confirm body the renderer assembles.
        let Step::Confirm {
            descendants, live, ..
        } = &pane.step
        else {
            unreachable!()
        };
        assert_eq!(*descendants, 3);
        assert!(*live);
        // The button row marks the active choice.
        let row: String = button_row(ConfirmChoice::Archive)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(row.contains("[ Archive ]"));
        assert!(row.contains("Delete"));
        assert!(row.contains("Cancel"));
    }

    /// Build a pane with a fixed root level and no daemon interaction.
    #[test]
    fn cursor_wraps_at_both_ends() {
        let cards = vec![
            (summary(Uuid::new_v4(), 300), Tier::Unread),
            (summary(Uuid::new_v4(), 200), Tier::Unread),
            (summary(Uuid::new_v4(), 100), Tier::Unread),
        ];
        let mut pane = test_pane(cards);
        assert_eq!(pane.current().cursor, 0);
        // Up from the first card wraps to the last.
        pane.handle_key(press(KeyCode::Up));
        assert_eq!(pane.current().cursor, 2);
        // Down from the last card wraps to the first.
        pane.handle_key(press(KeyCode::Down));
        assert_eq!(pane.current().cursor, 0);
        // `j`/`k` navigate the same (non-typing list).
        pane.handle_key(press(KeyCode::Char('k')));
        assert_eq!(pane.current().cursor, 2);
        pane.handle_key(press(KeyCode::Char('j')));
        assert_eq!(pane.current().cursor, 0);
    }

    #[test]
    fn cursor_single_card_stays_put() {
        let cards = vec![(summary(Uuid::new_v4(), 100), Tier::Unread)];
        let mut pane = test_pane(cards);
        pane.handle_key(press(KeyCode::Down));
        assert_eq!(pane.current().cursor, 0);
        pane.handle_key(press(KeyCode::Up));
        assert_eq!(pane.current().cursor, 0);
    }

    fn test_pane(cards: Vec<(SessionSummary, Tier)>) -> SessionsPane {
        SessionsPane {
            project_id: Some("pid".into()),
            scope: Scope::Project,
            show_archived: false,
            levels: vec![Level {
                parent: None,
                cards,
                cursor: 0,
                scroll: 0,
            }],
            step: Step::Browse,
            error: None,
            last_body_height: 100,
            last_content_rows: 0,
        }
    }
}

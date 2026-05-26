//! Top-level TUI state and event loop.
//!
//! Mouse capture is intentionally **not** enabled: leaving it off lets
//! the terminal/tmux handle the scroll wheel natively, so the user can
//! scroll up through chat history and the launch header even after they
//! spill into terminal scrollback. When we eventually need mouse-driven
//! interactions (clicking buttons, drag-to-select, etc.) we'll switch on
//! `EnableMouseCapture` and route `MouseEvent`s in the event loop —
//! revisit the scroll path when that happens.

use std::io::{Write, stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{Clear, ClearType, size as terminal_size};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, TerminalOptions, Viewport};

use crate::git;
use crate::tui::composer::{Composer, VimMode};
use crate::tui::settings::{self, Dialog};
use crate::welcome::{self, LaunchInfo};

const STATUS_HEIGHT: u16 = 1;
const MIN_HISTORY_HEIGHT: u16 = 1;
const MIN_INPUT_CONTENT: u16 = 1;
const MAX_INPUT_CONTENT: u16 = 6;
const INPUT_BORDER: u16 = 2;
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const EVENT_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "compact",
        description: "Compress the conversation to save context",
    },
    SlashCommand {
        name: "exit",
        description: "Quit cockpit",
    },
    SlashCommand {
        name: "model",
        description: "Switch the active model",
    },
    SlashCommand {
        name: "prune",
        description: "Drop the oldest messages",
    },
    SlashCommand {
        name: "settings",
        description: "Open the settings dialog",
    },
];

pub struct App {
    launch: LaunchInfo,
    composer: Composer,
    history: Vec<String>,
    last_git_refresh: Instant,
    /// Current pane height. Monotonically non-decreasing: when the chat
    /// or composer needs more room we grow the pane (and scroll prior
    /// terminal content up into scrollback so it stays mouse-reachable),
    /// but we never shrink it.
    pane_height: u16,
    dialog: Dialog,
}

impl App {
    pub fn new(project: Option<&Path>) -> Self {
        let mut composer = Composer::new(true);
        composer.vim_mode = VimMode::Insert;

        let mut app = Self {
            launch: welcome::load(project),
            composer,
            history: Vec::new(),
            last_git_refresh: Instant::now(),
            pane_height: 0,
            dialog: Dialog::None,
        };
        app.pane_height = app.desired_pane_height_uncapped();
        app
    }

    pub async fn run(&mut self) -> Result<()> {
        // Print the header to normal terminal output. It lives in scrollback
        // from this point on — once enough messages arrive it scrolls up
        // off the top of the terminal, recoverable with the mouse wheel.
        welcome::print_header(&self.launch);

        reserve_fixed_pane_space(self.pane_height)?;

        let (width, height) = terminal_size()?;
        let options = TerminalOptions {
            viewport: Viewport::Fixed(viewport_rect(self.pane_height, width, height)),
        };
        let mut terminal = ratatui::try_init_with_options(options)?;

        let kbd_enhanced = crossterm::execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .is_ok();

        let result = self.event_loop(&mut terminal);

        // Wipe the viewport rows before we hand the terminal back. Without
        // this, the input box / popup / status sit forever in the user's
        // scrollback under the last chat line — distracting when scrolling
        // up after exit.
        self.clear_viewport_for_exit().ok();

        if kbd_enhanced {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        ratatui::try_restore()?;
        result
    }

    fn clear_viewport_for_exit(&self) -> Result<()> {
        let (_, h) = terminal_size()?;
        let viewport_top = h.saturating_sub(self.pane_height);
        let mut out = stdout();
        for row in viewport_top..h {
            crossterm::execute!(out, cursor::MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        crossterm::execute!(out, cursor::MoveTo(0, viewport_top))?;
        out.flush()?;
        Ok(())
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.maybe_grow_pane(terminal)?;
            if self.maybe_spill_history()? {
                terminal.clear()?;
            }
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(EVENT_TICK)? {
                match event::read()? {
                    Event::Key(key) if accepts_key(&key) && self.handle_key(key) => break,
                    Event::Resize(width, height) => {
                        terminal.resize(viewport_rect(self.pane_height, width, height))?;
                    }
                    _ => {}
                }
            }

            if self.last_git_refresh.elapsed() >= GIT_REFRESH_INTERVAL {
                self.refresh_git();
            }
        }

        Ok(())
    }

    /// Grow the pane (and the terminal viewport) if more space is now
    /// needed than we've previously reserved. We scroll the terminal up
    /// by the deficit so prior output moves into scrollback rather than
    /// being clipped.
    fn maybe_grow_pane(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let (w, h) = terminal_size()?;
        let desired = self.desired_pane_height_uncapped().min(h);
        if desired > self.pane_height {
            let extra = desired - self.pane_height;
            push_terminal_content_up(extra, h)?;
            self.pane_height = desired;
            terminal.resize(viewport_rect(self.pane_height, w, h))?;
        }
        Ok(())
    }

    /// Once the pane has grown to fill the terminal but history still
    /// wants more space, pop the oldest entries off `App.history` and
    /// push them into terminal scrollback. Mouse-wheel scroll preserves
    /// them. Returns true if anything spilled (caller must clear ratatui's
    /// buffer to force a clean redraw).
    fn maybe_spill_history(&mut self) -> Result<bool> {
        let (_, h) = terminal_size()?;
        let chrome = self.input_height() + self.popup_lines() + STATUS_HEIGHT;
        let max_history = h.saturating_sub(chrome).max(MIN_HISTORY_HEIGHT);

        let total = self.total_history_lines();
        if total <= max_history {
            return Ok(false);
        }

        let to_spill = total - max_history;
        let mut spilled = 0u16;
        let mut items = Vec::new();
        while spilled < to_spill && !self.history.is_empty() {
            let entry = self.history.remove(0);
            spilled += entry_line_count(&entry);
            items.push(entry);
        }
        insert_above_viewport(self.pane_height, &items)?;
        Ok(true)
    }

    fn refresh_git(&mut self) {
        self.launch.repo_status = git::repo_status(&self.launch.cwd).ok().flatten();
        self.last_git_refresh = Instant::now();
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            return true;
        }

        if self.dialog.is_active() {
            if self.dialog.handle_key(key) {
                // TODO: when the settings pages actually mutate config,
                // reload and re-apply to the running session here.
                self.dialog = Dialog::None;
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                if self.slash_query().is_some() {
                    self.composer.buffer.clear();
                    self.composer.cursor = 0;
                    false
                } else {
                    true
                }
            }
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.insert_char('\n');
                    false
                } else {
                    self.complete_or_submit()
                }
            }
            KeyCode::Backspace => {
                self.delete_left();
                false
            }
            KeyCode::Delete => {
                self.delete_right();
                false
            }
            KeyCode::Left => {
                self.move_left();
                false
            }
            KeyCode::Right => {
                self.move_right();
                false
            }
            KeyCode::Up => {
                self.move_up();
                false
            }
            KeyCode::Down => {
                self.move_down();
                false
            }
            KeyCode::Home => {
                self.move_line_start();
                false
            }
            KeyCode::End => {
                self.move_line_end();
                false
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(ch);
                false
            }
            _ => false,
        }
    }

    fn complete_or_submit(&mut self) -> bool {
        if let Some(query) = self.slash_query() {
            if let Some(cmd) = slash_matches(query).first() {
                return self.execute_slash(**cmd);
            }
            return false;
        }
        self.submit_input()
    }

    fn submit_input(&mut self) -> bool {
        let submitted = self.composer.buffer.trim().to_string();
        if submitted.is_empty() {
            return false;
        }

        let prefix_width = welcome::INPUT_PREFIX.chars().count();
        let indent: String = " ".repeat(prefix_width);
        for (i, line) in submitted.split('\n').enumerate() {
            let prefix = if i == 0 {
                welcome::INPUT_PREFIX
            } else {
                indent.as_str()
            };
            self.history.push(format!("{prefix}{line}"));
        }
        self.history.push(format!(
            "{}: input captured; provider loop is not wired yet.",
            self.launch.agent_name
        ));
        self.composer.buffer.clear();
        self.composer.cursor = 0;
        false
    }

    fn execute_slash(&mut self, cmd: SlashCommand) -> bool {
        self.composer.buffer.clear();
        self.composer.cursor = 0;
        let msg = match cmd.name {
            "exit" => return true,
            "settings" => {
                self.dialog = Dialog::open(&self.launch.cwd);
                return false;
            }
            "compact" => "/compact: stub — context compaction not wired yet.",
            "prune" => "/prune: stub — history pruning not wired yet.",
            "model" => "/model: stub — model picker not wired yet.",
            _ => return false,
        };
        self.history.push(msg.to_string());
        false
    }

    fn slash_query(&self) -> Option<&str> {
        let rest = self.composer.buffer.strip_prefix('/')?;
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    fn popup_lines(&self) -> u16 {
        match self.slash_query() {
            Some(q) => slash_matches(q).len().max(1) as u16,
            None => 0,
        }
    }

    fn buffer_line_count(&self) -> u16 {
        if self.composer.buffer.is_empty() {
            1
        } else {
            self.composer.buffer.split('\n').count() as u16
        }
    }

    fn input_content_height(&self) -> u16 {
        self.buffer_line_count()
            .clamp(MIN_INPUT_CONTENT, MAX_INPUT_CONTENT)
    }

    fn input_height(&self) -> u16 {
        self.input_content_height() + INPUT_BORDER
    }

    fn total_history_lines(&self) -> u16 {
        self.history.iter().map(|s| entry_line_count(s)).sum()
    }

    /// Pane height the current state would prefer (before any cap). The
    /// pane shrinks to this only on growth — see `maybe_grow_pane`.
    fn desired_pane_height_uncapped(&self) -> u16 {
        if self.dialog.is_active() {
            return settings::DIALOG_HEIGHT;
        }
        let chrome = self.input_height() + self.popup_lines() + STATUS_HEIGHT;
        chrome + self.total_history_lines().max(MIN_HISTORY_HEIGHT)
    }

    fn render(&self, frame: &mut ratatui::Frame) {
        if self.dialog.is_active() {
            self.dialog.render(frame, frame.area());
            // No cursor in dialog mode — ratatui hides it when we skip
            // set_cursor_position during this draw.
            return;
        }

        let popup_h = self.popup_lines();
        let layout = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(self.input_height()),
            Constraint::Length(popup_h),
            Constraint::Length(STATUS_HEIGHT),
        ])
        .split(frame.area());

        self.render_history(frame, layout[0]);
        let cursor_pos = self.render_input(frame, layout[1]);
        if popup_h > 0 {
            self.render_popup(frame, layout[2]);
        }
        self.render_status(frame, layout[3]);

        frame.set_cursor_position(cursor_pos);
    }

    fn render_history(&self, frame: &mut ratatui::Frame, area: Rect) {
        let area_h = area.height as usize;
        let mut all: Vec<Line<'static>> = Vec::new();
        for entry in &self.history {
            for l in entry.split('\n') {
                all.push(Line::from(l.to_string()));
            }
        }
        // Bottom-align: newest content sits just above the input box,
        // blank padding above when sparse.
        let visible: Vec<Line<'static>> = if all.len() < area_h {
            let pad = area_h - all.len();
            let mut v: Vec<Line<'static>> = (0..pad).map(|_| Line::default()).collect();
            v.extend(all);
            v
        } else {
            let drop = all.len() - area_h;
            all.split_off(drop)
        };
        frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);
    }

    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) -> Position {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White));
        let input_inner = input_block.inner(area);

        let prefix_width = welcome::INPUT_PREFIX.chars().count();
        let indent: String = " ".repeat(prefix_width);
        let buf_lines: Vec<&str> = if self.composer.buffer.is_empty() {
            vec![""]
        } else {
            self.composer.buffer.split('\n').collect()
        };
        let lines: Vec<Line<'static>> = buf_lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 {
                    welcome::INPUT_PREFIX
                } else {
                    indent.as_str()
                };
                Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::White)),
                    Span::styled((*l).to_string(), Style::default().fg(Color::White)),
                ])
            })
            .collect();

        let before = &self.composer.buffer[..self.composer.cursor];
        let cursor_line = before.matches('\n').count() as u16;
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let cursor_col = before[line_start..].chars().count() as u16;

        let visible_rows = input_inner.height;
        let scroll_y = cursor_line.saturating_sub(visible_rows.saturating_sub(1));
        let para = Paragraph::new(lines).block(input_block).scroll((scroll_y, 0));
        frame.render_widget(para, area);

        Position::new(
            input_inner.x + prefix_width as u16 + cursor_col,
            input_inner.y + cursor_line.saturating_sub(scroll_y),
        )
    }

    fn render_popup(&self, frame: &mut ratatui::Frame, area: Rect) {
        let query = self.slash_query().unwrap_or("");
        let matches = slash_matches(query);
        let muted = Style::default().fg(Color::Indexed(welcome::MUTED_COLOR_INDEX));

        let lines: Vec<Line<'static>> = if matches.is_empty() {
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("no matching command", Style::default().fg(Color::Red)),
            ])]
        } else {
            let name_w = matches.iter().map(|c| c.name.len()).max().unwrap_or(0);
            matches
                .iter()
                .enumerate()
                .map(|(i, cmd)| {
                    let marker = if i == 0 { "▸ " } else { "  " };
                    let name_padded = format!("/{:<width$}", cmd.name, width = name_w);
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(name_padded, Style::default().fg(Color::Yellow)),
                        Span::raw("  "),
                        Span::styled(cmd.description.to_string(), muted),
                    ])
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let status_spans = welcome::status_line_spans(&self.launch);
        let status_width: u16 = status_spans
            .iter()
            .map(|s| s.width() as u16)
            .sum::<u16>()
            .min(area.width);
        let bottom = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(status_width),
        ])
        .split(area);
        frame.render_widget(Paragraph::new(self.launch.agent_name.as_str()), bottom[0]);
        frame.render_widget(Paragraph::new(Line::from(status_spans)), bottom[1]);
    }

    fn insert_char(&mut self, ch: char) {
        self.composer.buffer.insert(self.composer.cursor, ch);
        self.composer.cursor += ch.len_utf8();
    }

    fn delete_left(&mut self) {
        if self.composer.cursor == 0 {
            return;
        }
        let previous = self.composer.buffer[..self.composer.cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.composer.buffer.drain(previous..self.composer.cursor);
        self.composer.cursor = previous;
    }

    fn delete_right(&mut self) {
        if self.composer.cursor >= self.composer.buffer.len() {
            return;
        }
        let next_len = self.composer.buffer[self.composer.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.composer
            .buffer
            .drain(self.composer.cursor..self.composer.cursor + next_len);
    }

    fn move_left(&mut self) {
        if self.composer.cursor == 0 {
            return;
        }
        self.composer.cursor = self.composer.buffer[..self.composer.cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    fn move_right(&mut self) {
        if self.composer.cursor >= self.composer.buffer.len() {
            return;
        }
        if let Some(next) = self.composer.buffer[self.composer.cursor..].chars().next() {
            self.composer.cursor += next.len_utf8();
        }
    }

    fn move_up(&mut self) {
        let before = &self.composer.buffer[..self.composer.cursor];
        let Some(prev_nl) = before.rfind('\n') else {
            return;
        };
        let curr_line_start = prev_nl + 1;
        let col = before[curr_line_start..].chars().count();
        let prev_line_end = prev_nl;
        let prev_line_start = self.composer.buffer[..prev_line_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line = &self.composer.buffer[prev_line_start..prev_line_end];
        let target_chars = col.min(prev_line.chars().count());
        let target_byte = prev_line
            .char_indices()
            .nth(target_chars)
            .map(|(i, _)| i)
            .unwrap_or(prev_line.len());
        self.composer.cursor = prev_line_start + target_byte;
    }

    fn move_down(&mut self) {
        let buf = &self.composer.buffer;
        let cursor = self.composer.cursor;
        let line_start = buf[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = buf[line_start..cursor].chars().count();
        let Some(rel_nl) = buf[cursor..].find('\n') else {
            return;
        };
        let next_line_start = cursor + rel_nl + 1;
        let next_line_end = buf[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(buf.len());
        let next_line = &buf[next_line_start..next_line_end];
        let target_chars = col.min(next_line.chars().count());
        let target_byte = next_line
            .char_indices()
            .nth(target_chars)
            .map(|(i, _)| i)
            .unwrap_or(next_line.len());
        self.composer.cursor = next_line_start + target_byte;
    }

    fn move_line_start(&mut self) {
        let line_start = self.composer.buffer[..self.composer.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.composer.cursor = line_start;
    }

    fn move_line_end(&mut self) {
        let buf = &self.composer.buffer;
        let line_end = buf[self.composer.cursor..]
            .find('\n')
            .map(|i| self.composer.cursor + i)
            .unwrap_or(buf.len());
        self.composer.cursor = line_end;
    }
}

fn entry_line_count(entry: &str) -> u16 {
    (entry.split('\n').count() as u16).max(1)
}

fn slash_matches(query: &str) -> Vec<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(query))
        .collect()
}

fn viewport_rect(pane_height: u16, width: u16, height: u16) -> Rect {
    let h = pane_height.min(height.max(1));
    Rect::new(0, height.saturating_sub(h), width.max(1), h)
}

fn reserve_fixed_pane_space(height: u16) -> Result<()> {
    let mut out = stdout();
    for _ in 0..height {
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

/// Scroll the terminal up by `extra` rows by walking the cursor to the
/// bottom row and emitting linefeeds. In raw mode `\n` is plain LF, so
/// each one at the last row makes the terminal scroll: prior output
/// moves into scrollback (recoverable with the mouse wheel) and `extra`
/// blank rows open up at the bottom for the enlarged viewport.
fn push_terminal_content_up(extra: u16, term_h: u16) -> Result<()> {
    if extra == 0 {
        return Ok(());
    }
    let mut out = stdout();
    crossterm::execute!(out, cursor::MoveTo(0, term_h.saturating_sub(1)))?;
    for _ in 0..extra {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// Push `lines` into terminal scrollback just above the viewport.
///
/// Approach: write the lines at the top of the viewport (overwriting
/// the top rows of whatever is currently rendered there), then scroll
/// the terminal up by `lines.len()` rows. The just-written lines slide
/// up into the area above the viewport — visible if pane_height < term_h,
/// or pushed into actual terminal scrollback if pane_height == term_h.
/// Either way the mouse wheel can scroll back to them.
///
/// After calling this, the caller must invoke `terminal.clear()` so
/// ratatui forces a full redraw — otherwise its diff-based renderer
/// will not realize the terminal state has changed underneath it.
fn insert_above_viewport(pane_height: u16, lines: &[String]) -> Result<()> {
    let n = lines.len() as u16;
    if n == 0 {
        return Ok(());
    }
    let (_, h) = terminal_size()?;
    let viewport_top = h.saturating_sub(pane_height);
    let mut out = stdout();

    crossterm::execute!(out, cursor::MoveTo(0, viewport_top))?;
    for (i, line) in lines.iter().enumerate() {
        out.write_all(line.as_bytes())?;
        crossterm::execute!(out, Clear(ClearType::UntilNewLine))?;
        if i + 1 < lines.len() {
            out.write_all(b"\r\n")?;
        }
    }

    crossterm::execute!(out, cursor::MoveTo(0, h.saturating_sub(1)))?;
    for _ in 0..n {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn accepts_key(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

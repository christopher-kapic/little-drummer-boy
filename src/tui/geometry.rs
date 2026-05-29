//! Pane geometry — one place to compute section heights and split a frame.
//!
//! The TUI viewport is a fixed-height pane anchored to the bottom of the
//! terminal. Its layout is one of:
//!
//! - chat:   `[ body (history)  |  input  |  popup  |  status ]`
//! - dialog: `[ body (dialog)                                  |  status ]`
//!
//! `PaneGeometry::compute` produces the section heights for a given app
//! state; `layout` then carves a `Rect` into the named sub-rects.

use ratatui::layout::{Constraint, Layout, Rect};

pub const STATUS_HEIGHT: u16 = 1;
pub const MIN_HISTORY_HEIGHT: u16 = 1;

#[derive(Debug, Clone, Copy)]
pub struct PaneGeometry {
    /// Input box height (content + border). Zero when a dialog is open.
    pub input: u16,
    /// "Agent is working" status indicator row above the queue strip.
    /// Zero unless the agent is busy past the startup grace. One row
    /// when shown.
    pub indicator: u16,
    /// Queued-messages strip above the input. Zero when nothing is
    /// queued. Includes its top border AND its shared bottom border
    /// (the row that visually doubles as the input's top edge).
    pub queue: u16,
    /// Slash-popup / vim-hint height. Zero when there's no slash query
    /// or a dialog is open.
    pub popup: u16,
    /// Status row height. Always `STATUS_HEIGHT`; named so that callers
    /// don't need to reach for the constant separately.
    pub status: u16,
    /// Dialog height. Zero when no dialog is open.
    pub dialog: u16,
    /// Compact bottom-anchored overlay height (the answering/question
    /// dialog, GOALS §3b). Unlike `dialog` (a fullscreen modal that hides
    /// history), this sits at the bottom above the status row and lets
    /// history show above it. Zero when no compact overlay is open.
    pub compact: u16,
    /// History rows wanted by the current scrollback. The pane will grow
    /// to fit up to the terminal height; beyond that, old entries spill
    /// into terminal scrollback.
    pub history: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct PaneRects {
    /// Where history renders (chat mode) or the dialog overlays
    /// (dialog mode).
    pub body: Rect,
    /// Status-indicator row above the queue strip. Zero-area unless the
    /// working indicator is showing.
    pub indicator: Rect,
    /// Queued-messages strip above the input. Zero-area when the queue
    /// is empty or a dialog is open.
    pub queue: Rect,
    /// Input box rect. Zero-area when a dialog is open.
    pub input: Rect,
    /// Slash popup rect. Zero-area when there's no slash query or a
    /// dialog is open.
    pub popup: Rect,
    /// Compact bottom-anchored overlay rect (answering dialog). Zero-area
    /// unless a compact overlay is open. Sits below `body` (history) and
    /// above `status`.
    pub compact: Rect,
    /// Status row — always rendered, including under a dialog.
    pub status: Rect,
}

impl PaneGeometry {
    /// Build the geometry for an app frame.
    ///
    /// `input_height` and `popup_height` are passed in (rather than
    /// computed here) so the only inputs this module needs are integers —
    /// no dependency on the App or Composer types.
    pub fn compute(
        input_height: u16,
        indicator_height: u16,
        queue_height: u16,
        popup_height: u16,
        history_lines: u16,
        dialog_height: u16,
        compact_height: u16,
    ) -> Self {
        // The compact answering overlay takes precedence over the input
        // row + slash popup (it owns input while open) but keeps history
        // visible above it, bottom-anchored. A fullscreen `dialog` still
        // wins outright.
        if dialog_height == 0 && compact_height > 0 {
            return Self {
                input: 0,
                indicator: 0,
                queue: 0,
                popup: 0,
                status: STATUS_HEIGHT,
                dialog: 0,
                compact: compact_height,
                history: history_lines.max(MIN_HISTORY_HEIGHT),
            };
        }
        if dialog_height > 0 {
            Self {
                input: 0,
                indicator: 0,
                queue: 0,
                popup: 0,
                status: STATUS_HEIGHT,
                dialog: dialog_height,
                compact: 0,
                history: history_lines.max(MIN_HISTORY_HEIGHT),
            }
        } else {
            // Queue (when present) owns its top border, N message
            // rows, and a shared bottom row that doubles as the
            // input's top edge. So input drops its own top border
            // when the queue is up — net: one fewer row goes to
            // input.
            let input = if queue_height > 0 {
                input_height.saturating_sub(1)
            } else {
                input_height
            };
            Self {
                input,
                indicator: indicator_height,
                queue: queue_height,
                popup: popup_height,
                status: STATUS_HEIGHT,
                dialog: 0,
                compact: 0,
                history: history_lines.max(MIN_HISTORY_HEIGHT),
            }
        }
    }

    /// Pane height the current state would prefer if we weren't constrained
    /// by the terminal or by the monotonic-grow policy. Sum of all sections
    /// + however much history wants to show.
    pub fn desired_pane_height(&self) -> u16 {
        if self.dialog > 0 {
            self.dialog + self.status
        } else {
            self.history
                + self.indicator
                + self.queue
                + self.input
                + self.popup
                + self.compact
                + self.status
        }
    }

    /// Sum of every section above `body`. Used by `maybe_spill_history` to
    /// figure out how many rows are available for history.
    pub fn chrome_height(&self) -> u16 {
        if self.dialog > 0 {
            self.status
        } else {
            self.indicator + self.queue + self.input + self.popup + self.compact + self.status
        }
    }

    /// Split `area` into the named sub-rects.
    pub fn layout(&self, area: Rect) -> PaneRects {
        if self.dialog > 0 {
            let parts =
                Layout::vertical([Constraint::Min(0), Constraint::Length(self.status)]).split(area);
            PaneRects {
                body: parts[0],
                indicator: Rect::new(0, 0, 0, 0),
                queue: Rect::new(0, 0, 0, 0),
                input: Rect::new(0, 0, 0, 0),
                popup: Rect::new(0, 0, 0, 0),
                compact: Rect::new(0, 0, 0, 0),
                status: parts[1],
            }
        } else {
            let parts = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(self.indicator),
                Constraint::Length(self.queue),
                Constraint::Length(self.input),
                Constraint::Length(self.popup),
                Constraint::Length(self.compact),
                Constraint::Length(self.status),
            ])
            .split(area);
            PaneRects {
                body: parts[0],
                indicator: parts[1],
                queue: parts[2],
                input: parts[3],
                popup: parts[4],
                compact: parts[5],
                status: parts[6],
            }
        }
    }
}

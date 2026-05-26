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
    /// Queued-messages strip above the input. Zero-area when the queue
    /// is empty or a dialog is open.
    pub queue: Rect,
    /// Input box rect. Zero-area when a dialog is open.
    pub input: Rect,
    /// Slash popup rect. Zero-area when there's no slash query or a
    /// dialog is open.
    pub popup: Rect,
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
        queue_height: u16,
        popup_height: u16,
        history_lines: u16,
        dialog_height: u16,
    ) -> Self {
        if dialog_height > 0 {
            Self {
                input: 0,
                queue: 0,
                popup: 0,
                status: STATUS_HEIGHT,
                dialog: dialog_height,
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
                queue: queue_height,
                popup: popup_height,
                status: STATUS_HEIGHT,
                dialog: 0,
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
            self.history + self.queue + self.input + self.popup + self.status
        }
    }

    /// Sum of every section above `body`. Used by `maybe_spill_history` to
    /// figure out how many rows are available for history.
    pub fn chrome_height(&self) -> u16 {
        if self.dialog > 0 {
            self.status
        } else {
            self.queue + self.input + self.popup + self.status
        }
    }

    /// Split `area` into the named sub-rects.
    pub fn layout(&self, area: Rect) -> PaneRects {
        if self.dialog > 0 {
            let parts =
                Layout::vertical([Constraint::Min(0), Constraint::Length(self.status)]).split(area);
            PaneRects {
                body: parts[0],
                queue: Rect::new(0, 0, 0, 0),
                input: Rect::new(0, 0, 0, 0),
                popup: Rect::new(0, 0, 0, 0),
                status: parts[1],
            }
        } else {
            let parts = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(self.queue),
                Constraint::Length(self.input),
                Constraint::Length(self.popup),
                Constraint::Length(self.status),
            ])
            .split(area);
            PaneRects {
                body: parts[0],
                queue: parts[1],
                input: parts[2],
                popup: parts[3],
                status: parts[4],
            }
        }
    }
}

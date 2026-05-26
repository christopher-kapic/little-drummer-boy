//! Markdown → `Vec<Line<'static>>` emitter for the chat pane.
//!
//! Uses `pulldown-cmark` for parsing and walks the event stream to
//! build styled ratatui spans. Scope is deliberately narrow — we
//! support what LLMs actually emit in chat: bold, italic, inline code,
//! fenced code blocks, headings (h1–h3), bullet + ordered lists, and
//! block quotes. No tables, no images, no link rendering beyond
//! showing the label (we keep the `[text](url)` URL inline in muted
//! grey so the user can still copy it).
//!
//! Soft wrapping is the *caller's* job — the chrome already runs lines
//! through `wrap_with_reserved_first_line` so the output here is
//! emitted at logical line boundaries only.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const CODE_FG: Color = Color::Indexed(229); // soft yellow
const CODE_BG: Color = Color::Indexed(236); // near-black grey
const HEADING_FG: Color = Color::Indexed(81); // light cyan
const QUOTE_FG: Color = Color::Indexed(244); // mid grey
const LINK_FG: Color = Color::Indexed(75); // sky blue
const URL_FG: Color = Color::Indexed(244); // mid-grey, parenthetical

/// Parse `src` as Markdown and return one ratatui line per logical
/// rendered row. Empty input renders as a single empty line so the
/// caller's render path stays predictable.
pub fn render(src: &str) -> Vec<Line<'static>> {
    if src.is_empty() {
        return vec![Line::default()];
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(src, opts);
    let mut emitter = Emitter::default();
    for event in parser {
        emitter.handle(event);
    }
    emitter.finish()
}

#[derive(Default)]
struct Emitter {
    lines: Vec<Line<'static>>,
    /// Spans accumulating into the current logical row.
    current: Vec<Span<'static>>,
    /// Stack of style modifiers from open inline tags (bold/italic/etc).
    style_stack: Vec<Style>,
    /// True while inside a fenced/indented code block.
    in_code_block: bool,
    /// True while inside a block quote — we'll prefix each emitted line
    /// with a quote bar.
    in_block_quote: bool,
    /// List nesting state. For each open list, hold the (kind, next-index)
    /// where `kind` is None for bullets and `Some(n)` for ordered lists.
    list_stack: Vec<ListState>,
}

#[derive(Clone, Copy)]
struct ListState {
    ordered_index: Option<u64>,
}

impl Emitter {
    fn handle(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => self.text(s.into_string()),
            Event::Code(s) => self.inline_code(s.into_string()),
            Event::SoftBreak => self.text(" ".to_string()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => self.horizontal_rule(),
            Event::Html(s) | Event::InlineHtml(s) => self.text(s.into_string()),
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line();
                let hashes = "#".repeat(heading_depth(level));
                self.current.push(Span::styled(
                    format!("{hashes} "),
                    Style::default().fg(HEADING_FG).add_modifier(Modifier::BOLD),
                ));
                self.push_style(
                    Style::default().fg(HEADING_FG).add_modifier(Modifier::BOLD),
                );
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.in_block_quote = true;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind
                    && !lang.is_empty()
                {
                    self.lines.push(Line::from(Span::styled(
                        format!("```{lang}"),
                        Style::default().fg(CODE_FG).add_modifier(Modifier::DIM),
                    )));
                }
            }
            Tag::List(start) => {
                self.flush_line();
                self.list_stack.push(ListState { ordered_index: start });
            }
            Tag::Item => {
                self.flush_line();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(state) => match state.ordered_index {
                        Some(n) => {
                            state.ordered_index = Some(n + 1);
                            format!("{n}. ")
                        }
                        None => "• ".to_string(),
                    },
                    None => "• ".to_string(),
                };
                self.current.push(Span::raw(format!("{indent}{marker}")));
            }
            Tag::Emphasis => self.push_style(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { .. } => {
                self.push_style(Style::default().fg(LINK_FG).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Image { .. } => self.push_style(Style::default().fg(QUOTE_FG)),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line_then_blank(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.flush_line_then_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.in_block_quote = false;
                self.flush_line_then_blank();
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "```".to_string(),
                    Style::default().fg(CODE_FG).add_modifier(Modifier::DIM),
                )));
                self.lines.push(Line::default());
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.flush_line_then_blank();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
            }
            _ => {}
        }
    }

    fn text(&mut self, s: String) {
        if self.in_code_block {
            for raw in s.split_inclusive('\n') {
                let trimmed_nl = raw.strip_suffix('\n');
                let chunk = trimmed_nl.unwrap_or(raw).to_string();
                if !chunk.is_empty() {
                    self.current.push(Span::styled(
                        chunk,
                        Style::default().fg(CODE_FG).bg(CODE_BG),
                    ));
                }
                if trimmed_nl.is_some() {
                    self.flush_line();
                }
            }
            return;
        }
        let style = self.current_style();
        // Split on hard newlines (rare in inline content; paragraphs use
        // SoftBreak / HardBreak events) so a stray `\n` in raw HTML
        // doesn't end up inside a span.
        let mut first = true;
        for piece in s.split('\n') {
            if !first {
                self.flush_line();
            }
            if !piece.is_empty() {
                self.current.push(Span::styled(piece.to_string(), style));
            }
            first = false;
        }
    }

    fn inline_code(&mut self, s: String) {
        self.current.push(Span::styled(
            s,
            Style::default().fg(CODE_FG).bg(CODE_BG),
        ));
    }

    fn horizontal_rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(40),
            Style::default().fg(QUOTE_FG),
        )));
        self.lines.push(Line::default());
    }

    fn push_style(&mut self, style: Style) {
        let merged = self.current_style().patch(style);
        self.style_stack.push(merged);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        let line = if self.in_block_quote {
            let mut with_bar: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 1);
            with_bar.push(Span::styled(
                "│ ".to_string(),
                Style::default().fg(QUOTE_FG),
            ));
            with_bar.extend(spans);
            Line::from(with_bar)
        } else {
            Line::from(spans)
        };
        self.lines.push(line);
    }

    fn flush_line_then_blank(&mut self) {
        self.flush_line();
        if !matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // Trim trailing blank lines — the chat pane already insets a
        // gap row between entries, so dangling blanks here just widen
        // the gap.
        while matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.lines
    }
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Best-effort line-count estimate for sizing the chat pane geometry.
/// Faster than rendering and discarding; close-enough for the spill
/// math in `App::total_history_lines`.
pub fn estimate_lines(src: &str) -> usize {
    if src.is_empty() {
        return 1;
    }
    // One row per markdown logical line, plus a blank between paragraphs
    // / blocks. Cheap upper bound.
    let nl_count = src.matches('\n').count();
    let blank_blocks = src.matches("\n\n").count();
    (nl_count + 1).saturating_add(blank_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_strings(src: &str) -> Vec<String> {
        render(src)
            .into_iter()
            .map(|l| {
                l.spans
                    .into_iter()
                    .map(|s| s.content.into_owned())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn plain_text_round_trips() {
        assert_eq!(render_to_strings("hello world"), vec!["hello world"]);
    }

    #[test]
    fn bold_and_italic_text_keep_visible_content() {
        let s = render_to_strings("**bold** and *italic* and `code`");
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("bold"));
        assert!(s[0].contains("italic"));
        assert!(s[0].contains("code"));
    }

    #[test]
    fn fenced_code_block_includes_fences() {
        let s = render_to_strings("```rust\nfn main() {}\n```");
        assert!(s.iter().any(|l| l.starts_with("```rust")));
        assert!(s.iter().any(|l| l == "```"));
        assert!(s.iter().any(|l| l.contains("fn main()")));
    }

    #[test]
    fn bullet_list_marks_each_item() {
        let s = render_to_strings("- one\n- two\n- three");
        let bullets: Vec<&String> = s.iter().filter(|l| l.contains('•')).collect();
        assert_eq!(bullets.len(), 3);
    }

    #[test]
    fn ordered_list_numbers_items() {
        let s = render_to_strings("1. first\n2. second");
        assert!(s.iter().any(|l| l.starts_with("1. ")));
        assert!(s.iter().any(|l| l.starts_with("2. ")));
    }

    #[test]
    fn heading_prefixed_with_hashes() {
        let s = render_to_strings("# Hello");
        assert!(s.iter().any(|l| l.starts_with("# ")));
    }

    #[test]
    fn block_quote_prefixed_with_bar() {
        let s = render_to_strings("> quoted text");
        assert!(s.iter().any(|l| l.contains('│') && l.contains("quoted")));
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        assert_eq!(render("").len(), 1);
    }
}

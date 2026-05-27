//! System-clipboard helpers (plan.md T8.e).
//!
//! Two entry points:
//!
//! - [`copy_plain`] — plain-text only. Prefers OSC52 (works through SSH)
//!   and falls back to the local OS clipboard via `arboard` if the OSC52
//!   write fails (e.g. terminal doesn't honor the escape).
//! - [`copy_rich`] — multi-format (HTML + plain alt). Uses `arboard`
//!   only, because OSC52 is single-format. Returns `Err(Unsupported)`
//!   when the session is over SSH so the caller can show a toast and
//!   fall back to `copy_plain`.
//!
//! SSH detection is `$SSH_CONNECTION` / `$SSH_TTY` — OpenSSH sets these
//! on the remote side. Inside tmux on a local machine they're unset
//! so we still pick the local-clipboard path.

use std::io::{Write, stdout};

use base64::Engine;

/// Why a copy attempt didn't reach the system clipboard.
#[derive(Debug)]
pub enum CopyError {
    /// Rich-text copy was attempted over SSH, where no protocol can
    /// forward multi-format clipboard data. Caller should fall back to
    /// plain text and surface a toast.
    UnsupportedOverSsh,
    /// Underlying clipboard backend failed (no clipboard service,
    /// permission denied, etc.).
    Backend(String),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedOverSsh => write!(f, "rich-text copy unavailable over SSH"),
            Self::Backend(s) => write!(f, "clipboard backend error: {s}"),
        }
    }
}

impl std::error::Error for CopyError {}

/// Copy plain text to the system clipboard.
///
/// Tries OSC52 first (terminal escape, works through SSH and tmux with
/// `set-clipboard on`). Falls back to the local OS clipboard via
/// `arboard` if OSC52 isn't acknowledged by the terminal (we can't
/// actually detect that — we just attempt OSC52 and additionally try
/// arboard locally so at least one path lands).
pub fn copy_plain(text: &str) -> Result<(), CopyError> {
    let mut last_err = None;

    // OSC52 always — it's a fire-and-forget terminal escape, costs
    // nothing, and is the only path that crosses SSH.
    if let Err(e) = osc52_set_clipboard(text) {
        last_err = Some(e);
    }

    // Locally, also try arboard so the user gets the multi-format
    // clipboard slot populated (some apps prefer it over the OSC52
    // path). Skip on SSH.
    if !is_ssh()
        && let Err(e) = arboard_set_text(text)
    {
        // OSC52 might still have succeeded; don't overwrite that.
        if last_err.is_none() {
            last_err = Some(e);
        }
    }

    // If both backends failed, surface the most recent error. If at
    // least one succeeded, return Ok.
    match last_err {
        Some(e) if is_ssh() => Err(e),
        _ => Ok(()),
    }
}

/// Copy rich text (HTML + plain alt) to the system clipboard.
///
/// Goes through `arboard` only — OSC52 cannot carry multi-format. Over
/// SSH there's no clipboard pathway, so this returns
/// [`CopyError::UnsupportedOverSsh`] and the caller falls back to
/// [`copy_plain`].
pub fn copy_rich(plain: &str, html: &str) -> Result<(), CopyError> {
    if is_ssh() {
        return Err(CopyError::UnsupportedOverSsh);
    }
    arboard_set_html(html, plain)
}

/// True when the current process appears to be running over SSH —
/// `$SSH_CONNECTION` or `$SSH_TTY` is set. Used by the rich-text
/// copy path to fall back to OSC52 plain, and by the context-menu
/// builder to drop "Copy as rich text" from the offered list (it
/// can't reach the local clipboard over SSH anyway).
pub fn is_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

fn osc52_set_clipboard(text: &str) -> Result<(), CopyError> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // BEL-terminated form. `c` selects the system clipboard buffer
    // (vs `p` primary or numeric cut-buffers). tmux requires
    // `set -g set-clipboard on` to forward this through.
    let mut out = stdout();
    write!(out, "\x1b]52;c;{encoded}\x07").map_err(|e| CopyError::Backend(e.to_string()))?;
    out.flush().map_err(|e| CopyError::Backend(e.to_string()))?;
    Ok(())
}

fn arboard_set_text(text: &str) -> Result<(), CopyError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| CopyError::Backend(e.to_string()))?;
    cb.set_text(text.to_string())
        .map_err(|e| CopyError::Backend(e.to_string()))?;
    Ok(())
}

fn arboard_set_html(html: &str, plain: &str) -> Result<(), CopyError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| CopyError::Backend(e.to_string()))?;
    cb.set_html(html.to_string(), Some(plain.to_string()))
        .map_err(|e| CopyError::Backend(e.to_string()))?;
    Ok(())
}

/// Convert a markdown source string to a self-contained HTML fragment
/// suitable for the system clipboard's HTML slot. Used by the
/// rich-text copy keybind (plan.md T8.g).
pub fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);
    let parser = Parser::new_ext(markdown, opts);
    let mut buf = String::with_capacity(markdown.len() * 2);
    html::push_html(&mut buf, parser);
    buf
}

/// Render a markdown source string to plain text — drops the
/// formatting markers (`**`, `_`, backticks, ATX `#`, etc.) and
/// keeps readable structure (paragraph breaks, list items as
/// "- item", code block contents on their own lines). Used by the
/// "Copy as plain text" context-menu action.
pub fn markdown_to_plain(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(markdown, opts);
    let mut out = String::with_capacity(markdown.len());
    // Track list nesting to render bullets/numbered prefixes.
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut at_block_start = true;
    let mut in_code_block = false;
    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => {
                ensure_paragraph_break(&mut out);
                at_block_start = true;
            }
            Event::End(TagEnd::Paragraph) => {
                out.push('\n');
                at_block_start = true;
            }
            Event::Start(Tag::Heading { .. }) => {
                ensure_paragraph_break(&mut out);
                // No `#` prefix; the next text + a trailing blank
                // line gives the heading enough visual weight on its
                // own in a plain-text paste.
            }
            Event::End(TagEnd::Heading(_)) => {
                out.push_str("\n\n");
                at_block_start = true;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                ensure_paragraph_break(&mut out);
                out.push_str("> ");
                at_block_start = false;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                out.push('\n');
                at_block_start = true;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                ensure_paragraph_break(&mut out);
                in_code_block = true;
                at_block_start = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                out.push('\n');
                at_block_start = true;
            }
            Event::Start(Tag::List(start)) => {
                ensure_paragraph_break(&mut out);
                list_stack.push(start);
                at_block_start = true;
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() {
                    out.push('\n');
                }
                at_block_start = true;
            }
            Event::Start(Tag::Item) => {
                if !at_block_start {
                    out.push('\n');
                }
                let depth = list_stack.len().saturating_sub(1);
                for _ in 0..depth {
                    out.push_str("  ");
                }
                if let Some(top) = list_stack.last_mut() {
                    match top {
                        Some(n) => {
                            out.push_str(&format!("{n}. "));
                            *n += 1;
                        }
                        None => out.push_str("- "),
                    }
                }
                at_block_start = false;
            }
            Event::End(TagEnd::Item) => {
                at_block_start = true;
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => {}
            Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => {}
            Event::Start(Tag::Link { .. }) => {}
            Event::End(TagEnd::Link) => {}
            Event::Start(Tag::Image { .. }) => {}
            Event::End(TagEnd::Image) => {}
            Event::Text(s) => {
                out.push_str(&s);
                at_block_start = false;
            }
            Event::Code(s) => {
                // Inline code stays as the bare text — no backticks.
                out.push_str(&s);
                at_block_start = false;
            }
            Event::SoftBreak => {
                if in_code_block {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                at_block_start = false;
            }
            Event::HardBreak => {
                out.push('\n');
                at_block_start = false;
            }
            Event::Rule => {
                ensure_paragraph_break(&mut out);
                out.push_str("---\n\n");
                at_block_start = true;
            }
            Event::Html(s) | Event::InlineHtml(s) => {
                out.push_str(&s);
                at_block_start = false;
            }
            _ => {}
        }
    }
    // Collapse trailing whitespace + newlines so the pasted result
    // doesn't end with a sea of blank lines.
    while out.ends_with(['\n', ' ']) {
        out.pop();
    }
    out
}

/// Ensure the buffer ends with a paragraph break (`\n\n`) before
/// appending a new block. No-op when the buffer is empty or already
/// terminates that way.
fn ensure_paragraph_break(out: &mut String) {
    if out.is_empty() {
        return;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
}

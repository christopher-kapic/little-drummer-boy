//! Startup pixel banner (GOALS §1g).
//!
//! Renders a small P-51 Mustang in 256-color ANSI half-blocks. Source
//! data — color palette + 12×36 cell grid — was authored as the shell
//! script `p51-6.sh` in the repo root; this module is the Rust port.
//!
//! Each output row covers two input rows; each output column covers
//! two input columns. The four cells inside one 2×2 group decide one
//! half-block glyph + (fg, bg) pair via the same logic the shell
//! script uses (see [`draw_cell`]). Result: a 6-row × 18-col rendered
//! banner.
//!
//! Suppression (per the GOALS §1g rules) is handled at the
//! [`render_lines`] boundary — callers either get the lines or `None`
//! and skip the banner entirely.

use std::io::IsTerminal;

use crossterm::terminal;

/// Plane grid as 12 rows of 36 single-char cells. `.` = transparent;
/// `a`-`h` keys into [`PALETTE`].
const PLANE: [&str; 12] = [
    "......hhhh..........................",
    ".......hhhdd.....................hh.",
    ".h......dddeee..................hhh.",
    ".d.......eeedgggc...........ee.hhhh.",
    ".e.hhhhhhhhccccccchhhhhhhhhhhhhhhhhh",
    "fbaeaaaaahhhhhhhhhhhhddddhhhhheeeed.",
    ".e.ddddddeeeeeeeeeeeeedddd..........",
    ".d..........eeeeeeeedd..............",
    ".h...............ddddddd............",
    "....................dddhhh..........",
    "......................hhhhh.........",
    "....................................",
];

/// ANSI 256-color palette, indexed by `'a'..='h'` − `'a'`. Mirrors the
/// `color_for` case statement in `p51-6.sh`.
const PALETTE: [u8; 8] = [0, 3, 6, 7, 8, 11, 14, 15];

const PLANE_WIDTH: usize = 36;
const PLANE_HEIGHT: usize = 12;
/// Rendered banner width in terminal columns (one half-block glyph per
/// 2×2 cell group).
pub const RENDERED_WIDTH: usize = PLANE_WIDTH / 2;
/// Rendered banner height in terminal rows.
pub const RENDERED_HEIGHT: usize = PLANE_HEIGHT / 2;
/// Two-space left indent applied when rendering, matching the existing
/// `welcome.rs` chrome spacing.
const LEFT_INDENT: usize = 2;
/// Window must be at least this wide for the banner to render. Less
/// generous than the doc's "~36" — the rendered art is only 18 cells +
/// 2-space indent, so 20 is the actual minimum.
const MIN_TERMINAL_WIDTH: u16 = (RENDERED_WIDTH + LEFT_INDENT) as u16;

const RESET: &str = "\x1b[0m";

/// Render the banner into 6 ANSI-styled lines, or return `None` if the
/// environment doesn't support it. Suppression rules (any one of these
/// returns `None`):
///
/// - `enabled = false`. Set by `tui.banner.enabled` in config.
/// - `NO_COLOR` env var is set (per the no-color.org convention).
/// - `COCKPIT_ROOSTER=1` is set (the rooster splash preempts;
///   `miscellaneous.md` §9b).
/// - stdout isn't a TTY (piped, redirected, non-interactive).
/// - Terminal window is narrower than [`MIN_TERMINAL_WIDTH`].
pub fn render_lines(enabled: bool) -> Option<Vec<String>> {
    if !enabled {
        return None;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    if std::env::var_os("COCKPIT_ROOSTER").is_some() {
        return None;
    }
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if !terminal_wide_enough() {
        return None;
    }
    Some(render_unconditional())
}

fn terminal_wide_enough() -> bool {
    match terminal::size() {
        Ok((cols, _)) => cols >= MIN_TERMINAL_WIDTH,
        // Couldn't probe — assume there's room. Renders into stdout
        // with the worst case being a slightly-clipped banner, which
        // is preferable to suppressing on a working terminal whose
        // size we just couldn't read.
        Err(_) => true,
    }
}

/// Render the banner regardless of suppression rules. Useful for tests
/// and for callers (e.g. `/banner` debug commands later) that want the
/// art unconditionally.
pub fn render_unconditional() -> Vec<String> {
    let indent = " ".repeat(LEFT_INDENT);
    let mut out = Vec::with_capacity(RENDERED_HEIGHT);
    for y in (0..PLANE_HEIGHT).step_by(2) {
        let top = PLANE[y].as_bytes();
        let bot = PLANE[y + 1].as_bytes();
        let mut line = indent.clone();
        for x in (0..PLANE_WIDTH).step_by(2) {
            line.push_str(&draw_cell(
                top[x] as char,
                top[x + 1] as char,
                bot[x] as char,
                bot[x + 1] as char,
            ));
        }
        out.push(line);
    }
    out
}

/// Render one 2×2 cell group. Mirrors `draw_cell` in `p51-6.sh`:
///
/// 1. Find at most two distinct non-`.` colors in the four positions.
/// 2. The first (call it A) becomes the foreground; the second (B,
///    if present) becomes the background.
/// 3. The four boolean "is this cell A?" bits index into a fixed
///    glyph table (16 entries, since the all-zero case is handled
///    separately as a single space).
fn draw_cell(ul: char, ur: char, ll: char, lr: char) -> String {
    let mut unique = [None; 4];
    let mut count = 0;
    for &c in &[ul, ur, ll, lr] {
        if c == '.' {
            continue;
        }
        if unique.iter().take(count).any(|x| *x == Some(c)) {
            continue;
        }
        unique[count] = Some(c);
        count += 1;
    }

    if count == 0 {
        return " ".to_string();
    }

    let a = unique[0].expect("count >= 1");
    let bits = [(ul == a), (ur == a), (ll == a), (lr == a)];
    let glyph = glyph_for_pattern(bits);
    let fg = color_for(a);

    if let Some(b) = unique[1] {
        let bg = color_for(b);
        format!("\x1b[38;5;{fg};48;5;{bg}m{glyph}{RESET}")
    } else {
        format!("\x1b[38;5;{fg}m{glyph}{RESET}")
    }
}

fn color_for(c: char) -> u8 {
    let idx = (c as u8).wrapping_sub(b'a') as usize;
    *PALETTE.get(idx).unwrap_or(&15)
}

/// Map the 4-bit "is this position A?" pattern to a Unicode block
/// glyph. The mapping comes from `p51-6.sh`; the all-zero case is
/// pre-filtered by [`draw_cell`] (returns a space).
fn glyph_for_pattern(bits: [bool; 4]) -> &'static str {
    match bits {
        [true, true, true, true] => "█",
        [true, true, true, false] => "▛",
        [true, true, false, true] => "▜",
        [true, false, true, true] => "▙",
        [false, true, true, true] => "▟",
        [true, true, false, false] => "▀",
        [false, false, true, true] => "▄",
        [true, false, true, false] => "▌",
        [false, true, false, true] => "▐",
        [true, false, false, true] => "▚",
        [false, true, true, false] => "▞",
        [true, false, false, false] => "▘",
        [false, true, false, false] => "▝",
        [false, false, true, false] => "▖",
        [false, false, false, true] => "▗",
        [false, false, false, false] => " ", // unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_six_lines() {
        let lines = render_unconditional();
        assert_eq!(lines.len(), RENDERED_HEIGHT);
    }

    #[test]
    fn each_line_starts_with_two_space_indent() {
        for line in render_unconditional() {
            assert!(line.starts_with("  "), "missing indent in {line:?}");
        }
    }

    #[test]
    fn palette_size_matches_alphabet() {
        // a..h is 8 entries; the palette length must match so
        // color_for() never lands in the fallback branch on valid
        // inputs.
        assert_eq!(PALETTE.len(), 8);
    }

    #[test]
    fn plane_grid_is_uniform() {
        assert_eq!(PLANE.len(), PLANE_HEIGHT);
        for (i, row) in PLANE.iter().enumerate() {
            assert_eq!(row.chars().count(), PLANE_WIDTH, "row {i} width mismatch");
            for c in row.chars() {
                assert!(
                    c == '.' || matches!(c, 'a'..='h'),
                    "row {i} has unknown char `{c}`"
                );
            }
        }
    }

    #[test]
    fn no_color_env_suppresses() {
        // SAFETY: NO_COLOR is read-only-ish in tests but we set/unset
        // around the assertion. unsafe-block satisfies the
        // edition-2024 set_var/remove_var contract.
        let prev = std::env::var_os("NO_COLOR");
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let result = render_lines(true);
        match prev {
            Some(v) => unsafe { std::env::set_var("NO_COLOR", v) },
            None => unsafe { std::env::remove_var("NO_COLOR") },
        }
        assert!(result.is_none());
    }

    #[test]
    fn rooster_env_preempts() {
        let prev = std::env::var_os("COCKPIT_ROOSTER");
        unsafe { std::env::set_var("COCKPIT_ROOSTER", "1") };
        let result = render_lines(true);
        match prev {
            Some(v) => unsafe { std::env::set_var("COCKPIT_ROOSTER", v) },
            None => unsafe { std::env::remove_var("COCKPIT_ROOSTER") },
        }
        assert!(result.is_none());
    }

    #[test]
    fn disabled_flag_suppresses() {
        assert!(render_lines(false).is_none());
    }

    /// Visual smoke test. Run with `--nocapture` to see the banner.
    #[test]
    fn dump_for_visual_inspection() {
        eprintln!();
        for line in render_unconditional() {
            eprintln!("{line}");
        }
    }
}

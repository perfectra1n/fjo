//! Styling, plus the ANSI-aware width primitives every layout decision depends on.
//!
//! Two things live here that look unrelated but are not:
//!
//! * **Styling** ([`paint`], [`autocolor`], [`hyperlink`]). Escapes are produced only when
//!   [`Term::color`] is set, rather than always being produced and stripped later by
//!   `anstream`. That is a deliberate belt: TSV output must be byte-clean even if a caller
//!   forgets to wrap its writer, and a stray SGR escape inside a TSV field is invisible in a
//!   diff but breaks `cut -f2` for the user.
//! * **Width** ([`display_width`], [`truncate_visible`], [`strip_ansi`]). Once cells can
//!   contain escapes — and they can, because `{{color "green" .state}}` inside `tablerow`
//!   produces one — every width computation must skip them. `unicode_width` on a styled
//!   string counts `\x1b[32m` as 5 columns and every subsequent column drifts right.

use unicode_width::UnicodeWidthChar;

use super::tty::Term;

/// The header row: dim + underlined, which is `gh`'s look. Not bold — bold headers over
/// space-padded columns read as a heading rather than a table.
pub fn header_style() -> anstyle::Style {
    anstyle::Style::new().dimmed().underline()
}

/// Colors and attributes accepted by the `color` template function, in the order the error
/// message lists them.
pub const STYLE_NAMES: &[&str] = &[
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "gray",
    "grey",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
    "bold",
    "dim",
    "italic",
    "underline",
];

/// Resolve a style spec such as `green`, `bold`, or `bold+green`.
///
/// Multiple attributes may be combined with `+` or whitespace so a template can say
/// `{{color "bold+red" .state}}` without nesting two calls.
pub fn style_by_name(spec: &str) -> Option<anstyle::Style> {
    use anstyle::AnsiColor::*;
    let mut style = anstyle::Style::new();
    let mut any = false;
    for part in spec.split(['+', ' ', ',']).filter(|p| !p.is_empty()) {
        any = true;
        let lower = part.to_ascii_lowercase();
        let color = match lower.as_str() {
            "black" => Some(Black),
            "red" => Some(Red),
            "green" => Some(Green),
            "yellow" => Some(Yellow),
            "blue" => Some(Blue),
            "magenta" => Some(Magenta),
            "cyan" => Some(Cyan),
            "white" => Some(White),
            // `gray` is `gh`'s name for bright black, which is what actually renders as gray
            // on a dark background. Both spellings are accepted because both are in the wild.
            "gray" | "grey" | "bright-black" | "brightblack" => Some(BrightBlack),
            "bright-red" | "brightred" => Some(BrightRed),
            "bright-green" | "brightgreen" => Some(BrightGreen),
            "bright-yellow" | "brightyellow" => Some(BrightYellow),
            "bright-blue" | "brightblue" => Some(BrightBlue),
            "bright-magenta" | "brightmagenta" => Some(BrightMagenta),
            "bright-cyan" | "brightcyan" => Some(BrightCyan),
            "bright-white" | "brightwhite" => Some(BrightWhite),
            _ => None,
        };
        if let Some(c) = color {
            style = style.fg_color(Some(c.into()));
            continue;
        }
        style = match lower.as_str() {
            "bold" => style.bold(),
            "dim" | "dimmed" | "faint" => style.dimmed(),
            "italic" => style.italic(),
            "underline" | "underlined" => style.underline(),
            _ => return None,
        };
    }
    any.then_some(style)
}

/// The state → color map.
///
/// `None` means "no opinion", and the caller must then print the text **unchanged**. That
/// case is load-bearing: `gitea-model`'s open enums surface a server's unrecognized state
/// as `Unknown("needs_rebase")`, and a map that swallowed unknown states would print an
/// empty cell instead of the value we do not understand.
pub fn autocolor_style(state: &str) -> Option<anstyle::Style> {
    use anstyle::AnsiColor::*;
    let s = state.trim().to_ascii_lowercase();
    let color = match s.as_str() {
        "open" | "success" => Green,
        "closed" | "failure" => Red,
        "merged" => Magenta,
        "pending" | "running" => Yellow,
        "draft" | "wip" | "skipped" | "cancelled" | "canceled" => {
            return Some(anstyle::Style::new().dimmed());
        }
        _ => return None,
    };
    Some(anstyle::Style::new().fg_color(Some(color.into())))
}

/// Wrap `text` in `style`, or return it untouched when color is off.
pub fn paint(term: &Term, style: anstyle::Style, text: &str) -> String {
    if !term.color || style == anstyle::Style::new() || text.is_empty() {
        return text.to_string();
    }
    format!("{}{}{}", style.render(), text, style.render_reset())
}

/// Color a state string by the [`autocolor_style`] map, passing unrecognized values through.
pub fn autocolor(term: &Term, text: &str) -> String {
    match autocolor_style(text) {
        Some(style) => paint(term, style, text),
        None => text.to_string(),
    }
}

/// An OSC 8 hyperlink, or just `text` when the terminal is not known to support it.
///
/// Falling back to bare text (rather than `text (url)`) matches `gh`: the URL is usually
/// already visible in another column, and appending it would break the column widths that
/// were computed from the styled string.
pub fn hyperlink(term: &Term, url: &str, text: &str) -> String {
    if !term.hyperlinks || url.is_empty() {
        return text.to_string();
    }
    // ESC ] 8 ; params ; URI ST  <text>  ESC ] 8 ; ; ST
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Byte length of the ANSI escape sequence starting at `start`, if there is one.
///
/// Handles the two forms that actually appear in our output: CSI (`ESC [ … final`) for SGR
/// styling, and OSC (`ESC ] … BEL`-or-`ST`) for hyperlinks. An OSC hyperlink embeds a URL
/// containing `;` and `/`, so it cannot be scanned with the CSI rule — getting this wrong
/// makes every hyperlinked cell count its URL as visible width.
fn escape_len(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(start) != Some(&0x1b) {
        return None;
    }
    match b.get(start + 1) {
        Some(b'[') => {
            let mut i = start + 2;
            while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                i += 1;
            }
            Some((i + 1).min(b.len()) - start)
        }
        Some(b']') => {
            let mut i = start + 2;
            while i < b.len() {
                if b[i] == 0x07 {
                    return Some(i + 1 - start);
                }
                if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                    return Some(i + 2 - start);
                }
                i += 1;
            }
            Some(b.len() - start)
        }
        // A lone ESC, or ESC followed by one intermediate byte (e.g. `ESC \`, the string
        // terminator). Consume it so it is not mistaken for printable text.
        Some(_) => Some(2),
        None => Some(1),
    }
}

/// Visible width of `s` in terminal columns, ignoring ANSI escapes.
///
/// Widths are summed per character rather than per string. That is deliberate: the same
/// per-character arithmetic is used by [`truncate_visible`], and the property that actually
/// keeps columns aligned is that padding and truncation agree with each other. Per-character
/// summing over-counts a ZWJ emoji sequence (a "family" emoji counts as 4 rather than 2), so
/// the failure mode is a column one or two spaces wider than necessary — never a column that
/// overflows and shifts everything after it.
pub fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut i = 0;
    let b = s.len();
    while i < b {
        if let Some(len) = escape_len(s, i) {
            i += len;
            continue;
        }
        let c = s[i..].chars().next().expect("index is on a char boundary");
        w += c.width().unwrap_or(0);
        i += c.len_utf8();
    }
    w
}

/// `s` with all ANSI escapes removed. Used for TSV cells, where an escape would corrupt a
/// field, and for error messages that quote user data.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = escape_len(s, i) {
            i += len;
            continue;
        }
        let c = s[i..].chars().next().expect("index is on a char boundary");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// The ellipsis used for truncation. One column wide, unlike `...`, which costs three of the
/// columns you were trying to save.
pub const ELLIPSIS: char = '…';

/// Truncate `s` to at most `max` visible columns, appending `…` when anything was dropped.
///
/// Escape sequences are copied through and never counted, and a reset is appended when the
/// truncation happened inside a styled run — otherwise the style would bleed into the next
/// column and colorize the rest of the row.
pub fn truncate_visible(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1; // room for the ellipsis
    let mut out = String::with_capacity(s.len());
    let mut w = 0;
    let mut i = 0;
    let mut styled = false;
    while i < s.len() {
        if let Some(len) = escape_len(s, i) {
            styled = true;
            out.push_str(&s[i..i + len]);
            i += len;
            continue;
        }
        let c = s[i..].chars().next().expect("index is on a char boundary");
        let cw = c.width().unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
        i += c.len_utf8();
    }
    out.push(ELLIPSIS);
    if styled {
        out.push_str("\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::tty::MapEnv;

    fn colored() -> Term {
        Term::detect_with(&MapEnv::new().with("CLICOLOR_FORCE", "1"), true, Some(80))
    }

    /// Bug this prevents: measuring a styled cell with plain `unicode_width`, which counts
    /// `\x1b[32m` as five columns and pushes every column to its right out of alignment.
    #[test]
    fn width_ignores_sgr_escapes() {
        let styled = paint(&colored(), style_by_name("green").unwrap(), "open");
        assert!(styled.len() > 4, "the test string must actually carry escapes");
        assert_eq!(display_width(&styled), 4);
        assert_eq!(strip_ansi(&styled), "open");
    }

    /// Bug this prevents: scanning an OSC 8 hyperlink with the CSI rule. The URL contains
    /// `;` and `/` but no CSI final byte in the right place, so a CSI-only scanner counts
    /// most of the URL as visible text.
    #[test]
    fn width_ignores_osc8_hyperlinks() {
        let term = Term { tty: true, width: 80, color: true, hyperlinks: true };
        let link = hyperlink(&term, "https://example.org/a;b/c", "gea");
        assert_eq!(display_width(&link), 3);
        assert_eq!(strip_ansi(&link), "gea");
    }

    /// Bug this prevents: counting CJK and emoji as one column each, so a table of Chinese
    /// titles is visibly ragged even though the byte/char arithmetic looked right.
    #[test]
    fn cjk_and_emoji_are_double_width() {
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("ascii"), 5);
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(display_width("a🚀日"), 1 + 2 + 2);
    }

    /// Bug this prevents: truncating by `char` count, which splits a double-width character
    /// budget in half and produces a cell one column too wide.
    #[test]
    fn truncation_respects_double_width() {
        // Six columns of CJK truncated to 5 => two chars (4 cols) plus the ellipsis.
        assert_eq!(truncate_visible("日本語", 5), "日本…");
        assert!(display_width(&truncate_visible("日本語", 5)) <= 5);
        // Never truncate what already fits.
        assert_eq!(truncate_visible("日本語", 6), "日本語");
        assert_eq!(truncate_visible("abcdef", 4), "abc…");
    }

    /// Bug this prevents: truncating inside a styled run without a reset, so the color
    /// bleeds across the two-space gutter and paints the next column.
    #[test]
    fn truncation_closes_an_open_style() {
        let styled = paint(&colored(), style_by_name("red").unwrap(), "abcdefgh");
        let cut = truncate_visible(&styled, 4);
        assert!(cut.ends_with("\x1b[0m"), "got {cut:?}");
        assert_eq!(display_width(&cut), 4);
    }

    /// Bug this prevents: an unrecognized state (an open-enum `Unknown`) being dropped or
    /// replaced by a placeholder instead of printed verbatim.
    #[test]
    fn autocolor_passes_unknown_states_through() {
        let term = colored();
        assert_eq!(autocolor(&term, "needs_rebase"), "needs_rebase");
        assert_eq!(autocolor(&term, ""), "");
        assert!(autocolor(&term, "open").contains("open"));
        assert_ne!(autocolor(&term, "open"), "open", "a known state should be styled");
    }

    /// Bug this prevents: only matching the lowercase spellings, so Gitea's `WIP` prefix
    /// and a capitalized `Open` lose their color.
    #[test]
    fn autocolor_is_case_insensitive() {
        assert!(autocolor_style("WIP").is_some());
        assert!(autocolor_style("Open").is_some());
        assert!(autocolor_style(" merged ").is_some());
    }

    /// Bug this prevents: emitting escapes when color is disabled, which would end up inside
    /// TSV fields.
    #[test]
    fn paint_is_a_no_op_without_color() {
        let plain = Term::piped();
        assert_eq!(paint(&plain, style_by_name("green").unwrap(), "open"), "open");
        assert_eq!(autocolor(&plain, "open"), "open");
        assert_eq!(hyperlink(&plain, "https://x", "t"), "t");
    }

    #[test]
    fn style_names_all_resolve() {
        for name in STYLE_NAMES {
            assert!(style_by_name(name).is_some(), "{name}");
        }
        assert!(style_by_name("bold+green").is_some());
        assert!(style_by_name("chartreuse").is_none());
        assert!(style_by_name("").is_none());
    }
}

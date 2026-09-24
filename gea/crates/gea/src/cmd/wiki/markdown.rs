//! A small terminal renderer for markdown.
//!
//! # Why not a markdown crate, and why not the server
//!
//! Two tempting alternatives, both wrong here:
//!
//! * **`POST /markdown`** renders to *HTML*. A terminal cannot use HTML, and asking the server
//!   to render a document we already have in hand adds a round trip to `wiki view`.
//! * **A CommonMark crate** would parse the document correctly and then leave the hard part —
//!   turning a tree into styled, width-aware terminal output — entirely undone, for a dependency
//!   whose full generality nothing here needs.
//!
//! So this is a **line-oriented, deliberately shallow** renderer: headings, fenced code, lists,
//! block quotes, rules, and inline `code`/emphasis/links. It never rewraps and never reorders, so
//! whatever it does not understand survives verbatim — a document that renders imperfectly is an
//! inconvenience, but a document whose text is *lost* would be a data-integrity bug.
//!
//! **It runs only on a terminal.** Piped output is the raw markdown, byte for byte, because
//! `gea wiki view Home > Home.md` has to produce the file that was committed. The command
//! layer enforces that; this module is not reached at all when output is not a TTY.

use anstyle::{AnsiColor, Style};

use crate::output::color::paint;
use crate::output::{Term, display_width};

/// Render markdown for a terminal.
pub fn render(source: &str, term: &Term) -> String {
    let mut out = String::with_capacity(source.len() + source.len() / 8);
    let mut in_fence = false;
    let mut fence_marker = String::new();

    for line in source.lines() {
        let trimmed = line.trim_end();

        // Fenced code first: nothing inside a fence is markup, and treating a `# comment` in a
        // shell snippet as a heading is the single most visible way to get this wrong.
        if let Some(marker) = fence_open(trimmed) {
            if in_fence {
                if trimmed.trim_start().starts_with(&fence_marker) {
                    in_fence = false;
                    continue;
                }
            } else {
                in_fence = true;
                fence_marker = marker;
                continue;
            }
        }
        if in_fence {
            out.push_str(&paint(term, code_style(), &format!("  {trimmed}")));
            out.push('\n');
            continue;
        }

        if let Some(rule) = horizontal_rule(trimmed, term) {
            out.push_str(&rule);
            out.push('\n');
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            out.push_str(&paint(term, heading_style(level), &inline(text, term)));
            out.push('\n');
            continue;
        }
        if let Some((indent, marker, rest)) = list_item(trimmed) {
            out.push_str(&format!(
                "{indent}{} {}",
                paint(term, bullet_style(), &marker),
                inline(rest, term)
            ));
            out.push('\n');
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            out.push_str(&paint(term, quote_style(), &format!("│ {}", rest.trim_start())));
            out.push('\n');
            continue;
        }
        out.push_str(&inline(trimmed, term));
        out.push('\n');
    }
    out
}

fn heading_style(level: usize) -> Style {
    // h1 and h2 are the document's structure and earn underlining; deeper levels are bold only,
    // because a page with six underlined lines has no structure left to see.
    if level <= 2 { Style::new().bold().underline() } else { Style::new().bold() }
}

fn code_style() -> Style {
    Style::new().fg_color(Some(AnsiColor::Cyan.into()))
}

fn bullet_style() -> Style {
    Style::new().fg_color(Some(AnsiColor::Yellow.into()))
}

fn quote_style() -> Style {
    Style::new().dimmed()
}

fn link_style() -> Style {
    Style::new().fg_color(Some(AnsiColor::Blue.into())).underline()
}

/// ` ``` ` or `~~~`, with an optional info string.
fn fence_open(line: &str) -> Option<String> {
    let t = line.trim_start();
    for marker in ["```", "~~~"] {
        if t.starts_with(marker) {
            return Some(marker.to_owned());
        }
    }
    None
}

/// `# Heading` → `(1, "Heading")`. A `#` with no space after it is not a heading in CommonMark,
/// and is very often a shell comment or a `#42` issue reference at the start of a line.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    let text = rest.strip_prefix(' ')?;
    Some((hashes, text.trim()))
}

/// `- item`, `* item`, `+ item`, `1. item`, preserving the indent so nesting survives.
fn list_item(line: &str) -> Option<(String, String, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = " ".repeat(indent_len);
    let t = line.trim_start();
    for marker in ['-', '*', '+'] {
        if let Some(rest) = t.strip_prefix(marker)
            && let Some(rest) = rest.strip_prefix(' ')
        {
            return Some((indent, "•".to_owned(), rest));
        }
    }
    // An ordered item keeps its own number: renumbering someone's list would change meaning.
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = t[digits..].strip_prefix(". ")
    {
        return Some((indent, format!("{}.", &t[..digits]), rest));
    }
    None
}

/// `---`, `***`, `___` — three or more of one character, nothing else on the line.
fn horizontal_rule(line: &str, term: &Term) -> Option<String> {
    let t = line.trim();
    if t.len() < 3 {
        return None;
    }
    let first = t.chars().next()?;
    if !matches!(first, '-' | '*' | '_') || !t.chars().all(|c| c == first) {
        return None;
    }
    // Two columns short of the width so the rule does not wrap in a terminal that counts the
    // last column differently (and so it does not touch a scrollbar).
    Some(paint(term, quote_style(), &"─".repeat(term.width.saturating_sub(2).max(3))))
}

/// Inline spans: `` `code` ``, `**strong**`, `*emphasis*`, `_emphasis_`, `[text](url)`.
///
/// One pass, no nesting, and anything unmatched is emitted verbatim — an unclosed `**` must not
/// swallow the rest of the paragraph.
fn inline(line: &str, term: &Term) -> String {
    if !term.color && !term.hyperlinks {
        // Nothing to do, and stripping the markers without styling would *lose* information:
        // `**important**` would become `important` with no emphasis at all.
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len());
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i];
        if c == '`'
            && let Some(end) = find(&bytes, i + 1, '`')
        {
            let text: String = bytes[i + 1..end].iter().collect();
            out.push_str(&paint(term, code_style(), &text));
            i = end + 1;
            continue;
        }
        if c == '['
            && let Some((text, url, next)) = link(&bytes, i)
        {
            out.push_str(&crate::output::color::hyperlink(
                term,
                &url,
                &paint(term, link_style(), &text),
            ));
            i = next;
            continue;
        }
        if c == '*'
            && i + 1 < bytes.len()
            && bytes[i + 1] == '*'
            && let Some(end) = find_pair(&bytes, i + 2)
        {
            let text: String = bytes[i + 2..end].iter().collect();
            out.push_str(&paint(term, Style::new().bold(), &inline(&text, term)));
            i = end + 2;
            continue;
        }
        if (c == '*' || c == '_')
            && let Some(end) = find(&bytes, i + 1, c)
            // An empty span is not emphasis: `**` alone, or `__` between two words.
            && end > i + 1
        {
            // An `_` inside a word is not a delimiter either; see `looks_like_identifier`.
            if c == '_' && looks_like_identifier(&bytes, i, end) {
                out.push(c);
                i += 1;
                continue;
            }
            let text: String = bytes[i + 1..end].iter().collect();
            out.push_str(&paint(term, Style::new().italic(), &text));
            i = end + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|i| chars[*i] == needle)
}

/// The index of a closing `**`.
fn find_pair(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|i| chars[*i] == '*' && chars[*i + 1] == '*')
}

/// `[text](url)` starting at `at`, and the index just past it.
fn link(chars: &[char], at: usize) -> Option<(String, String, usize)> {
    let close = find(chars, at + 1, ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find(chars, close + 2, ')')?;
    let text: String = chars[at + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((text, url, end + 1))
}

/// True when an `_` is inside a word — `some_field_name` — rather than delimiting emphasis.
///
/// Bug this guards: field names are snake_case everywhere in this project, so a page documenting
/// `content_base64` would otherwise render "base" in italics and drop two underscores.
fn looks_like_identifier(chars: &[char], open: usize, close: usize) -> bool {
    let before = open.checked_sub(1).and_then(|i| chars.get(i));
    let after = chars.get(close + 1);
    before.is_some_and(|c| c.is_alphanumeric()) || after.is_some_and(|c| c.is_alphanumeric())
}

/// A page title, underlined to the width of the title itself.
pub fn title(text: &str, term: &Term) -> String {
    let bar = "─".repeat(display_width(text).min(term.width));
    format!("{}\n{}\n", paint(term, Style::new().bold(), text), paint(term, quote_style(), &bar))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A colourless TTY, so goldens are readable: the *structure* is what is under test, and
    /// escape sequences in an assertion are unreviewable.
    fn plain() -> Term {
        Term::tty(40)
    }

    #[test]
    fn headings_lose_their_hashes_and_lists_gain_a_bullet() {
        let out = render("# Title\n\n- one\n- two\n", &plain());
        assert_eq!(out, "Title\n\n• one\n• two\n");
    }

    #[test]
    fn ordered_lists_keep_their_own_numbering() {
        // Renumbering would change what the author wrote; `3.` in a fragment stays `3.`.
        assert_eq!(render("3. third\n4. fourth\n", &plain()), "3. third\n4. fourth\n");
    }

    /// Bug this prevents: treating a `#` comment inside a fenced code block as a heading, so a
    /// shell snippet loses its comment markers and reads as a section title.
    #[test]
    fn nothing_inside_a_fence_is_treated_as_markup() {
        let src = "```sh\n# not a heading\n- not a list\n```\nafter\n";
        assert_eq!(render(src, &plain()), "  # not a heading\n  - not a list\nafter\n");
    }

    /// Bug this prevents: `#42` or `#!/bin/sh` at the start of a line rendering as a heading.
    /// CommonMark requires a space after the hashes, and both of those are common in wikis.
    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        assert_eq!(render("#42 is fixed\n", &plain()), "#42 is fixed\n");
        assert_eq!(render("#!/bin/sh\n", &plain()), "#!/bin/sh\n");
    }

    #[test]
    fn a_horizontal_rule_spans_the_terminal() {
        let out = render("---\n", &plain());
        assert_eq!(out.trim_end().chars().count(), 38);
        assert!(out.starts_with('─'));
    }

    /// Bug this prevents: an unclosed `**` swallowing the rest of the paragraph, so text
    /// disappears from the rendered page. Anything unmatched must survive verbatim.
    #[test]
    fn unmatched_markers_are_emitted_verbatim() {
        let term = Term { color: true, ..Term::tty(40) };
        let out = render("a **b and c\n", &term);
        assert!(out.contains("**b and c"), "{out:?}");
        let out = render("a `b\n", &term);
        assert!(out.contains("`b"), "{out:?}");
    }

    /// Bug this prevents: `content_base64` rendering as "content" + italic "base" + "64", which
    /// both mangles the text and drops the underscores a reader needs to copy the field name.
    #[test]
    fn snake_case_identifiers_keep_their_underscores() {
        let term = Term { color: true, ..Term::tty(40) };
        let out = render("the content_base64 field\n", &term);
        assert!(out.contains("content_base64"), "{out:?}");
    }

    /// With colour off there is nothing to render *into*, so the markers stay: stripping them
    /// would silently discard emphasis rather than represent it.
    #[test]
    fn a_colourless_terminal_keeps_inline_markers() {
        assert_eq!(render("a **strong** word\n", &plain()), "a **strong** word\n");
    }
}

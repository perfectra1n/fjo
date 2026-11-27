//! Documentation cleanup.
//!
//! Gitea's descriptions are written for the Swagger UI, so they arrive HTML-escaped and
//! sprinkled with `<code>`, `<br>` and `<a href=…>`. Emitting them unchanged produces doc
//! comments that render as literal `&lt;` in `cargo doc` and clap help that shows raw markup.
//!
//! Cleaning happens **here, in lowering**, not in the emitters. Four emitters each doing their
//! own unescaping would be four chances to disagree about the same sentence, and the IR is
//! supposed to be the contract.
//!
//! ## Why two forms of the same text
//!
//! [`Doc`] carries the long text twice, and the duplication is deliberate:
//!
//! - **rustdoc** reads `[foo]` as an intra-doc link. A description mentioning `[owner]` or
//!   `[1, 2]` produces a `broken_intra_doc_links` warning, and with `RUSTDOCFLAGS=-D warnings`
//!   in CI that is a failed build somewhere inside 9k lines of generated models. So the rustdoc
//!   form escapes `[` and `]`.
//! - **clap** prints its `about` verbatim. Give it the escaped form and the user sees
//!   `\[owner\]` in `--help`, which looks like a bug in our CLI.
//!
//! One escaped form for docs, one plain form for help text. Deriving one from the other in an
//! emitter would put string munging back in exactly the layer that must not have any.

use serde::Serialize;

/// Column at which the long form is wrapped. 96 leaves room for `/// ` plus indentation inside
/// an `impl` block and still fits a 100-column limit.
pub const WRAP: usize = 96;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Doc {
    /// First sentence, plain text. clap `about`, and the summary column of
    /// `gea raw search`.
    pub short: String,
    /// Full text, plain, wrapped. clap `long_about`.
    pub long: Vec<String>,
    /// Full text with `[` and `]` escaped, wrapped. Emitted as `#[doc = …]`.
    pub rustdoc: Vec<String>,
}

impl Doc {
    pub fn is_empty(&self) -> bool {
        self.short.is_empty() && self.long.is_empty()
    }

    /// Builds a `Doc` from a summary and a longer description, either of which may be absent.
    ///
    /// Gitea puts the useful one-liner in `summary` and only 13 of 506 operations have a
    /// `description` at all, so `summary` is the primary source and `description` extends it.
    pub fn from_parts(summary: Option<&str>, description: Option<&str>) -> Doc {
        let summary = clean(summary.unwrap_or_default());
        let description = clean(description.unwrap_or_default());

        let full = match (summary.is_empty(), description.is_empty()) {
            (true, true) => return Doc::default(),
            (false, true) => summary.clone(),
            (true, false) => description.clone(),
            // Avoid the common case where `description` merely repeats `summary`.
            (false, false) if description.starts_with(&summary) => description.clone(),
            (false, false) => format!("{summary} {description}"),
        };

        let short =
            if summary.is_empty() { first_sentence(&full) } else { first_sentence(&summary) };

        Doc { short, long: wrap(&full, WRAP), rustdoc: wrap(&escape_brackets(&full), WRAP) }
    }

    /// A `Doc` from a single description, e.g. a model field or a parameter.
    pub fn from_text(text: Option<&str>) -> Doc {
        Doc::from_parts(None, text)
    }
}

/// Unescapes entities, strips the handful of HTML tags Gitea uses, and collapses whitespace.
pub fn clean(raw: &str) -> String {
    let mut s = strip_tags(raw);
    s = unescape_entities(&s);
    collapse_whitespace(&s)
}

/// The named entities Gitea's descriptions actually contain, plus a numeric decoder.
///
/// Deliberately not a general HTML decoder: an unknown `&word;` is far more likely to be
/// literal prose ("Q&A") than an entity we failed to handle, and mangling prose is worse than
/// leaving an entity intact. Numeric entities are different — `&#x7b;` is never prose — so
/// those are decoded generically. Gitea needs that: the Actions token description contains
/// `&#x7b;&#x7b; gitea.token &#x7d;&#x7d;`, its own template braces escaped.
fn unescape_entities(s: &str) -> String {
    const ENTITIES: [(&str, &str); 6] = [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        // `&amp;` is last on purpose: doing it first would turn `&amp;lt;` into `<` rather
        // than the literal `&lt;` the author wrote.
        ("&amp;", "&"),
    ];
    let mut out = numeric_entities(s);
    for (from, to) in ENTITIES {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    out
}

/// Decodes `&#NN;` and `&#xHH;`. Anything that does not parse is left exactly as written.
fn numeric_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let decoded = after.find(';').and_then(|end| {
            let digits = &after[..end];
            let cp = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(cp).map(|c| (c, end + 3))
        });
        match decoded {
            Some((c, consumed)) => {
                out.push(c);
                rest = &rest[at + consumed..];
            }
            None => {
                out.push_str("&#");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Removes the tags Gitea emits, keeping their content.
///
/// `<br>` becomes a space rather than a newline: the long form is re-wrapped anyway, and a
/// hard newline in the middle of a clap `about` breaks its layout.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Only strip things that look like the tags we know about. A bare `<` in prose
            // ("value < 10") must survive, and an over-eager stripper would eat the rest of
            // the sentence looking for a closing `>`.
            if let Some(end) = s[i..].find('>') {
                let tag = &s[i + 1..i + end];
                let name = tag
                    .trim_start_matches('/')
                    .split([' ', '\t', '\n', '/'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                const KNOWN: [&str; 8] = ["code", "br", "a", "p", "b", "i", "em", "strong"];
                if KNOWN.contains(&name.as_str()) {
                    // `<br>` and `</p>` join words that would otherwise run together.
                    if matches!(name.as_str(), "br" | "p") {
                        out.push(' ');
                    }
                    i += end + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Escapes `[` and `]` so rustdoc does not read them as an intra-doc link.
fn escape_brackets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '[' || c == ']' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The first sentence, for clap's `about`.
///
/// A sentence ends at `. `, `! ` or `? `, or at the end of the string. The `. ` (rather than
/// bare `.`) requirement is what keeps `e.g.`, `v1.2` and `main.rs` from cutting a summary in
/// half — Gitea descriptions are full of all three.
pub fn first_sentence(s: &str) -> String {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if !matches!(b, b'.' | b'!' | b'?') {
            continue;
        }
        match bytes.get(i + 1) {
            None => return s[..i].trim_end().to_owned(),
            Some(b' ') | Some(b'\n') | Some(b'\t') => {
                // `e.g. foo`: a single letter before the dot is an abbreviation, not an end.
                let looks_abbreviated = i >= 2 && bytes[i - 2] == b'.';
                if !looks_abbreviated {
                    return s[..i].to_owned();
                }
            }
            _ => {}
        }
    }
    s.trim_end_matches(['.', ' ']).to_owned()
}

/// Greedy word wrap. Words longer than the limit (URLs, mostly) get their own line rather than
/// being broken, because a broken URL is not clickable and not copy-pasteable.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_entities_are_unescaped() {
        assert_eq!(clean("a &lt;b&gt; c"), "a <b> c");
        assert_eq!(clean("&quot;quoted&quot; and &#34;this&#34;"), "\"quoted\" and \"this\"");
        assert_eq!(clean("it&#39;s"), "it's");
    }

    #[test]
    fn numeric_entities_are_decoded() {
        // Gitea's Actions token description escapes its own template braces this way, and
        // leaving `&#x7b;` in a doc comment is both ugly and confusing.
        assert_eq!(
            clean("Bearer $&#x7b;&#x7b; gitea.token &#x7d;&#x7d;"),
            "Bearer ${{ gitea.token }}"
        );
        assert_eq!(clean("it&#39;s &#34;quoted&#34;"), "it's \"quoted\"");
    }

    #[test]
    fn an_unparseable_numeric_entity_is_left_alone() {
        assert_eq!(clean("&#notanumber; and &#"), "&#notanumber; and &#");
    }

    #[test]
    fn amp_is_unescaped_last() {
        // The bug this prevents: replacing `&amp;` first turns the literal text `&amp;lt;`
        // into `<`, silently changing what the author wrote.
        assert_eq!(clean("&amp;lt;"), "&lt;");
        assert_eq!(clean("Q&amp;A"), "Q&A");
    }

    #[test]
    fn known_tags_are_stripped_and_their_content_kept() {
        assert_eq!(clean("pass <code>--force</code> to override"), "pass --force to override");
        assert_eq!(clean("one<br>two"), "one two");
        assert_eq!(clean("see <a href=\"https://x\">the docs</a>"), "see the docs");
        assert_eq!(clean("<p>a paragraph</p>"), "a paragraph");
    }

    #[test]
    fn a_bare_less_than_in_prose_survives() {
        // An over-eager stripper searching for the next `>` would eat "10 and the rest".
        assert_eq!(clean("value < 10 and the rest"), "value < 10 and the rest");
        assert_eq!(clean("a <notatag> b"), "a <notatag> b");
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(clean("a\n   b\t\tc  "), "a b c");
    }

    #[test]
    fn brackets_are_escaped_for_rustdoc_but_not_for_clap() {
        // rustdoc reads `[owner]` as an intra-doc link, which with RUSTDOCFLAGS=-D warnings
        // fails the build somewhere inside 9k generated lines. clap, on the other hand, would
        // print the backslash to the user.
        let d = Doc::from_text(Some("replaces [owner] in the path"));
        assert_eq!(d.long, ["replaces [owner] in the path"]);
        assert_eq!(d.rustdoc, ["replaces \\[owner\\] in the path"]);
        assert_eq!(d.short, "replaces [owner] in the path");
    }

    #[test]
    fn first_sentence_splits_the_short_form() {
        let d = Doc::from_parts(
            Some("Create a pull request. The base and head may be in different repositories."),
            None,
        );
        assert_eq!(d.short, "Create a pull request");
        assert_eq!(d.long.len(), 1);
        assert!(d.long[0].contains("different repositories"));
    }

    #[test]
    fn a_trailing_period_is_dropped_from_the_short_form() {
        // clap `about` strings are not sentences in gh's style, and a stray period on half of
        // 506 commands looks like an inconsistency rather than a choice.
        assert_eq!(
            Doc::from_text(Some("List a repository's branches.")).short,
            "List a repository's branches"
        );
    }

    #[test]
    fn abbreviations_do_not_end_a_sentence() {
        // The bug this prevents: `e.g. a tag` truncating to `e` for half the parameter help
        // strings in the API.
        assert_eq!(
            first_sentence("A ref, e.g. a branch or tag. More text."),
            "A ref, e.g. a branch or tag"
        );
        assert_eq!(
            first_sentence("Path such as src/main.rs here"),
            "Path such as src/main.rs here"
        );
    }

    #[test]
    fn description_repeating_summary_is_not_duplicated() {
        let d = Doc::from_parts(Some("Get a repository"), Some("Get a repository by owner/name"));
        assert_eq!(d.long, ["Get a repository by owner/name"]);
    }

    #[test]
    fn long_text_wraps_at_the_limit() {
        let text = "word ".repeat(40);
        let d = Doc::from_text(Some(&text));
        assert!(d.long.len() > 1);
        for line in &d.long {
            assert!(line.chars().count() <= WRAP, "line too long: {line:?}");
        }
    }

    #[test]
    fn an_overlong_word_gets_its_own_line_rather_than_being_broken() {
        // Breaking a URL makes it neither clickable nor copy-pasteable.
        let url = "https://".to_owned() + &"x".repeat(120);
        let lines = wrap(&format!("see {url} for details"), WRAP);
        assert!(lines.iter().any(|l| l == &url), "{lines:?}");
    }

    #[test]
    fn empty_input_yields_an_empty_doc() {
        assert!(Doc::from_parts(None, None).is_empty());
        assert!(Doc::from_text(Some("   ")).is_empty());
    }
}

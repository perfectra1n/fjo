//! Making git's own words safe to print.
//!
//! Every message this module produces quotes something git was given or said, and both can
//! carry a credential. A remote URL is the obvious one — `https://user:token@forge/o/r.git`
//! is a perfectly ordinary thing to find in `remote.origin.url`, and it appears verbatim in
//! git's argv *and* in git's error output ("fatal: could not read from
//! 'https://user:token@forge/o/r.git'"). An error that relays that untouched publishes the
//! token the moment someone pastes it into a bug report.
//!
//! The redaction itself is [`crate::http::redact`]'s, unchanged: one implementation, so a
//! fix there fixes both paths. What this module adds is finding the URLs inside free-form
//! text, since `redact::url` expects to be handed one URL rather than four lines of prose
//! with three URLs in them.

use std::borrow::Cow;
use std::ffi::OsStr;

use crate::http::redact;

/// Redact every `scheme://` URL embedded in free text, leaving the rest byte-for-byte alone.
///
/// scp-like remotes (`git@host:owner/repo.git`) are deliberately not touched: that syntax has
/// no place to put a password, so there is nothing to hide and rewriting it would only corrupt
/// a message.
pub fn text(s: &str) -> Cow<'_, str> {
    if !s.contains("://") {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut rewrote = false;

    while let Some(rel) = s[cursor..].find("://") {
        let sep = cursor + rel;
        // `.max(cursor)` because two URLs can abut with no delimiter between them; without it
        // the second token's start would rewind into text already emitted.
        let start = token_start(s, sep).max(cursor);
        let end = token_end(s, sep);
        out.push_str(&s[cursor..start]);
        let token = &s[start..end];
        match redact::url(token) {
            Cow::Borrowed(t) => out.push_str(t),
            Cow::Owned(t) => {
                out.push_str(&t);
                rewrote = true;
            }
        }
        cursor = end;
    }
    out.push_str(&s[cursor..]);

    if rewrote { Cow::Owned(out) } else { Cow::Borrowed(s) }
}

/// Render an argv for an error message, with every URL in it redacted.
///
/// Not shell-quoted: this is for a human reading a message, and quoting it would suggest the
/// line can be pasted into a shell when nothing here was ever run through one.
pub fn argv<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| {
            let lossy = a.as_ref().to_string_lossy();
            text(&lossy).into_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where the token containing a `://` at `at` starts.
fn token_start(s: &str, at: usize) -> usize {
    let mut start = 0;
    for (i, c) in s[..at].char_indices() {
        if is_left_delim(c) {
            start = i + c.len_utf8();
        }
    }
    start
}

/// Where that token ends.
fn token_end(s: &str, at: usize) -> usize {
    s[at..].char_indices().find(|(_, c)| is_right_delim(*c)).map_or(s.len(), |(i, _)| at + i)
}

fn is_left_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '(' | '[' | '=')
}

fn is_right_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | '>' | ')' | ']')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug: `gea repo clone` failing and reporting
    /// `git clone https://me:ghp_realtoken@forge/o/r.git failed: …` — which the user then
    /// pastes into an issue.
    #[test]
    fn a_password_in_a_clone_url_never_survives_an_error_message() {
        let out = text("fatal: could not read from 'https://me:s3cr3ttoken@forge/o/r.git'");
        assert!(!out.contains("s3cr3ttoken"), "{out}");
        assert!(out.contains("me:<redacted>@forge"), "{out}");
        // Everything around the URL is untouched, including the quotes.
        assert!(out.starts_with("fatal: could not read from '"), "{out}");
        assert!(out.ends_with("'"), "{out}");
    }

    #[test]
    fn several_urls_in_one_message_are_all_redacted() {
        let out = text(
            "remote: https://a:secretA@forge/x\nremote: https://b:secretB@forge/y\n\
             done https://forge/z",
        );
        assert!(!out.contains("secretA"), "{out}");
        assert!(!out.contains("secretB"), "{out}");
        assert!(out.contains("https://forge/z"), "{out}");
        assert_eq!(out.lines().count(), 3);
    }

    /// A message with nothing to hide must come back borrowed, not rebuilt: a needless
    /// allocation here is on every successful command's error-free path.
    #[test]
    fn clean_text_is_borrowed() {
        assert!(matches!(text("everything up-to-date"), Cow::Borrowed(_)));
        assert!(matches!(text("pushed to https://forge/o/r.git"), Cow::Borrowed(_)));
    }

    /// scp-like syntax cannot carry a password, and mangling it would make the message wrong.
    #[test]
    fn scp_like_remotes_are_left_alone() {
        let s = "fatal: 'git@forge:owner/repo.git' does not appear to be a git repository";
        assert!(matches!(text(s), Cow::Borrowed(_)));
    }

    #[test]
    fn argv_redacts_each_argument() {
        let line = argv(&["clone", "https://me:tok3nvalue@forge/o/r.git", "dir"]);
        assert_eq!(line, "clone https://me:<redacted>@forge/o/r.git dir");
    }

    /// `?token=` is still accepted by Gitea, so a URL carrying one is just as leaky.
    #[test]
    fn a_query_token_is_scrubbed_too() {
        let out = text("cloning https://forge/o/r.git?token=abcdef0123456789 now");
        assert!(!out.contains("abcdef0123456789"), "{out}");
    }
}

//! Percent-encoding for path parameters and query values.
//!
//! Encoding is **per-parameter**, not per-path, and that distinction is the whole reason this
//! module exists. Most Gitea path parameters are single segments (`{owner}`, `{repo}`,
//! `{index}`) where a `/` in the value must become `%2F` or it silently changes which route
//! matches. But a handful of parameters (`filepath`, `treePath`, `ref`, `path`, `filename`)
//! carry a *whole path*, and encoding their `/` turns `GET .../contents/src/main.rs` into a
//! request for a file literally named `src/main.rs` in the root — which 404s for every
//! nested file in every repository.
//!
//! Both directions of the mistake are real, so both are unit-tested below: `src/main.rs` must
//! survive [`path_like`] intact, and an owner literally named `a/b` must come out of [`seg`]
//! as `a%2Fb`.
//!
//! We hand-roll the encoder rather than take a `percent-encoding` dependency: the rule is
//! twelve lines, and the set of characters we preserve is a decision worth having in front of
//! us rather than behind a crate feature flag.

use std::borrow::Cow;

/// RFC 3986 *unreserved*: `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
///
/// We deliberately preserve **only** this set and encode every other byte, including the
/// sub-delimiters (`!$&'()*+,;=`) and `:@` that a path segment is technically allowed to
/// contain. Encoding them is always safe — the server percent-decodes before matching route
/// parameters — whereas *not* encoding them depends on how each reverse proxy in front of the
/// instance normalises a URL. Correct-and-ugly beats pretty-and-conditional.
const fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Shared engine. `extra` names bytes to pass through in addition to the unreserved set.
fn encode<'a>(s: &'a str, extra: &[u8]) -> Cow<'a, str> {
    let keep = |b: u8| is_unreserved(b) || extra.contains(&b);
    if s.bytes().all(keep) {
        // The overwhelmingly common case: nothing to do, and no allocation.
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if keep(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    Cow::Owned(out)
}

/// Encode a value that occupies exactly one path segment: `{owner}`, `{repo}`, `{index}`, …
///
/// `/` is encoded. If a value containing `/` reached the URL unencoded it would add a path
/// segment and match a *different* route — at best a 404, at worst a request against the
/// wrong object.
pub fn seg(s: &str) -> Cow<'_, str> {
    encode(s, b"")
}

/// Encode a value that is itself a path: `filepath`, `treePath`, `ref`, `path`, `filename`.
///
/// `/` is preserved because it is structural to the value. Everything else is encoded, so a
/// filename with a space or a `#` still survives.
pub fn path_like(s: &str) -> Cow<'_, str> {
    encode(s, b"/")
}

/// Encode a query-string key or value.
///
/// Space becomes `%20`, never `+`. `+` only means space in `application/x-www-form-urlencoded`,
/// and a Go server reading `r.URL.Query()` does apply that rule — which means a literal `+`
/// in a search term (`c++`) would silently become a space if we did not encode it.
pub fn query(s: &str) -> Cow<'_, str> {
    encode(s, b"")
}

/// Decode a query-string key or value the way the server will read it.
///
/// The inverse of [`query`], and it exists for one job: a query string a *user* typed has to be
/// re-emitted through [`query_string`] without double-encoding. `?q=a%20b` must reach the
/// instance as `q=a%20b`; feeding the raw `a%20b` back through [`query`] yields `a%2520b`, a
/// literal search for the six characters `a%20b`.
///
/// `+` decodes to a space, because that is what a Go server's `r.URL.Query()` does. Preserving
/// it as a literal `+` would keep *our* bytes intact at the cost of changing the value the
/// instance actually sees. Re-encoding then spells that space `%20`, which Go reads back as a
/// space: the round trip preserves the meaning rather than the spelling.
///
/// A `%` that does not begin a valid escape stays a literal `%` — what browsers do, and what
/// whoever typed `?q=100%` meant; it re-encodes to `%25`, so the instance receives the percent
/// sign they asked for. [`None`] is returned only for escapes that decode to bytes which are not
/// UTF-8 and therefore cannot be carried in the `String` a query list holds.
pub fn decode_query(s: &str) -> Option<Cow<'_, str>> {
    if !s.contains(['%', '+']) {
        // The overwhelmingly common case: nothing to decode, and no allocation.
        return Some(Cow::Borrowed(s));
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (unhex(b[i + 1]), unhex(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().map(Cow::Owned)
}

const fn unhex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Encode a `application/x-www-form-urlencoded` body value, where `+` for space is the
/// convention. Used only for [`super::Body::Form`].
pub fn form(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b if is_unreserved(b) => out.push(b as char),
            b => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Join an ordered query list into a query string, without the leading `?`.
///
/// The list is ordered, not a map, because repeated keys are legal in this API (`labels=bug&
/// labels=ci`). Collapsing them into a map would silently drop all but one.
pub fn query_string<'a, I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str> + 'a,
    V: AsRef<str> + 'a,
{
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&query(k.as_ref()));
        out.push('=');
        out.push_str(&query(v.as_ref()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `get-contents` 404: encoding `/` in a path-like parameter asks for a file named
    /// `src/main.rs` in the repository root instead of `main.rs` inside `src/`.
    #[test]
    fn path_like_preserves_slashes_so_nested_files_resolve() {
        assert_eq!(path_like("src/main.rs"), "src/main.rs");
        assert_eq!(path_like("a/b/c/d.txt"), "a/b/c/d.txt");
        // Still encodes everything else, so a space in a filename survives.
        assert_eq!(path_like("docs/my notes.md"), "docs/my%20notes.md");
    }

    /// The mirror-image bug: a single-segment parameter whose value contains `/` would add a
    /// path segment and match a different route entirely.
    #[test]
    fn seg_encodes_slashes_so_a_weird_owner_cannot_change_the_route() {
        assert_eq!(seg("a/b"), "a%2Fb");
        assert_eq!(seg("perf3ct"), "perf3ct");
        assert_eq!(seg("my.repo-1_x~"), "my.repo-1_x~");
    }

    #[test]
    fn seg_encodes_reserved_and_non_ascii() {
        assert_eq!(seg("a b"), "a%20b");
        assert_eq!(seg("a#b?c"), "a%23b%3Fc");
        assert_eq!(seg("a:b@c"), "a%3Ab%40c");
        assert_eq!(seg("é"), "%C3%A9");
        assert_eq!(seg("100%"), "100%25");
    }

    /// A `+` in a query value must arrive as a literal `+`, or searching for `c++` silently
    /// searches for `c  `.
    #[test]
    fn query_encodes_plus_rather_than_treating_it_as_space() {
        assert_eq!(query("c++"), "c%2B%2B");
        assert_eq!(query("a b"), "a%20b");
    }

    #[test]
    fn form_uses_plus_for_space() {
        assert_eq!(form("hello world"), "hello+world");
        assert_eq!(form("c++"), "c%2B%2B");
    }

    /// Repeated keys must both survive: `labels` is legal more than once.
    #[test]
    fn query_string_keeps_repeated_keys_in_order() {
        let q = query_string([("labels", "bug"), ("labels", "ci"), ("state", "open")]);
        assert_eq!(q, "labels=bug&labels=ci&state=open");
    }

    /// The double-encoding bug: a query a user typed is decoded before it re-enters the
    /// structured list, so `?q=a%20b` leaves through [`query_string`] as `q=a%20b` and not as
    /// `q=a%2520b` — a literal search for the six characters `a%20b`.
    #[test]
    fn a_user_typed_escape_survives_a_decode_and_re_encode_round_trip() {
        for typed in ["a%20b", "c%2B%2B", "plain", "%C3%A9", "a%2Fb", "100%25"] {
            let decoded = decode_query(typed).expect("valid UTF-8");
            assert_eq!(query(&decoded), typed, "round trip of {typed}");
        }
    }

    /// `+` means space to a Go server, so decoding it as a literal `+` would preserve our bytes
    /// while changing the value the instance searches for. The spelling moves to `%20`; the
    /// meaning does not move at all.
    #[test]
    fn plus_decodes_to_space_because_that_is_what_the_server_reads() {
        assert_eq!(decode_query("a+b").unwrap(), "a b");
        assert_eq!(query(&decode_query("a+b").unwrap()), "a%20b");
        // And a `+` the user escaped is a real `+`, which must survive as one.
        assert_eq!(decode_query("c%2B%2B").unwrap(), "c++");
    }

    /// A stray `%` is what someone typing `?q=100%` meant, not an error. It re-encodes to
    /// `%25`, so the instance receives the percent sign rather than a truncated term.
    #[test]
    fn a_stray_percent_is_a_literal_percent() {
        assert_eq!(decode_query("100%").unwrap(), "100%");
        assert_eq!(decode_query("%zz").unwrap(), "%zz");
        assert_eq!(query(&decode_query("100%").unwrap()), "100%25");
    }

    /// An escape that decodes to non-UTF-8 has no `String` representation, so it is reported
    /// rather than silently replaced — a lossy `U+FFFD` would re-encode to three bytes the user
    /// never typed.
    #[test]
    fn a_non_utf8_escape_is_reported_rather_than_mangled() {
        assert!(decode_query("%FF").is_none());
        assert!(decode_query("ok%C3%28").is_none());
    }

    #[test]
    fn empty_input_is_borrowed_and_empty() {
        assert!(matches!(seg(""), Cow::Borrowed("")));
        assert!(matches!(path_like("plain"), Cow::Borrowed("plain")));
    }
}

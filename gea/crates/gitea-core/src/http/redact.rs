//! Scrubbing credentials out of anything we might print.
//!
//! Every debug/trace path in this crate goes through here, because the failure mode is
//! catastrophic and silent: a user pastes `gea --debug` output into a bug report and has now
//! published a token with write access to their repositories. There is no way to un-publish
//! it.
//!
//! Three things carry credentials, and all three are handled: the `Authorization`, `Sudo`, and
//! `X-GITEA-OTP` headers; the `?token=` / `?access_token=` query parameters that Gitea
//! still accepts for compatibility; and `https://user:pass@host` credentials embedded in a
//! URL. `Sudo` is included even though it is a username rather than a secret — it names a
//! third party the operator impersonated, which does not belong in a pasted log either.

use std::borrow::Cow;

/// What replaces a redacted value. Deliberately not `***` — a distinctive marker is easy to
/// grep for in a test that asserts nothing leaked.
pub const MASK: &str = "<redacted>";

/// Header names whose values must never be printed. Compared ASCII-case-insensitively, since
/// HTTP header names are case-insensitive and `http::HeaderName` lowercases while a
/// hand-assembled `Vec<(String, String)>` may not.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "sudo",
    "x-gitea-otp",
    "x-gitea-otp",
    "cookie",
    "set-cookie",
];

/// Query parameter names whose values must never be printed.
///
/// The last four are the OAuth2 flow's. `code` is the non-obvious one and the reason this list
/// is not just "things called token": an authorization code is single-use, but until it is spent
/// it is a bearer credential that anyone can exchange for a real token, and `--debug` prints the
/// URL of the very request that spends it. `client_secret` is here defensively — gea is a public
/// client and never sends one — because a user pointing `--client-id` at a confidential
/// application would otherwise put it in a pasted log.
pub const SENSITIVE_QUERY: &[&str] = &[
    "token",
    "access_token",
    "private_token",
    "code",
    "refresh_token",
    "client_secret",
    "code_verifier",
];

pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

pub fn is_sensitive_query(name: &str) -> bool {
    SENSITIVE_QUERY.iter().any(|q| q.eq_ignore_ascii_case(name))
}

/// Redact a header value if the header is sensitive, otherwise pass it through.
///
/// The auth *scheme* is preserved (`token <redacted>`, `Basic <redacted>`) because it is
/// diagnostically load-bearing and not itself a secret: the single most common auth bug
/// against this API is sending a bare token with no `token ` prefix, and a log line that says
/// `Authorization: <redacted>` cannot tell you whether that happened.
pub fn header_value<'a>(name: &str, value: &'a str) -> Cow<'a, str> {
    if !is_sensitive_header(name) {
        return Cow::Borrowed(value);
    }
    match value.split_once(' ') {
        Some((scheme, _)) if !scheme.is_empty() => Cow::Owned(format!("{scheme} {MASK}")),
        _ => Cow::Borrowed(MASK),
    }
}

/// Redact an iterator of headers, preserving order.
pub fn headers<'a, I>(hs: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    hs.into_iter().map(|(n, v)| (n.to_owned(), header_value(n, v).into_owned())).collect()
}

/// Redact a `http::HeaderMap`, preserving order and repeated headers.
///
/// Values that are not valid UTF-8 render as `<non-utf8>` rather than being lossily decoded:
/// a lossy decode of a binary value could in principle expose bytes we meant to hide.
pub fn header_map(map: &http::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(n, v)| {
            let name = n.as_str();
            let value = v.to_str().map_or(Cow::Borrowed("<non-utf8>"), |s| header_value(name, s));
            (name.to_owned(), value.into_owned())
        })
        .collect()
}

/// Redact a URL: embedded userinfo credentials and any sensitive query parameter.
///
/// Parsing is done by hand on the string rather than through a URL type, because this must
/// also work on malformed input — an error message printing the URL that failed to parse is
/// exactly the case where a leak would happen.
pub fn url(u: &str) -> Cow<'_, str> {
    let has_userinfo = userinfo_span(u).is_some();
    let has_secret_query = u
        .split_once('?')
        .is_some_and(|(_, q)| q.split('&').any(|p| is_sensitive_query(param_name(p))));
    if !has_userinfo && !has_secret_query {
        return Cow::Borrowed(u);
    }

    let mut out = String::with_capacity(u.len());
    let (before_query, query) = match u.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (u, None),
    };

    match userinfo_span(before_query) {
        // Keep the username, drop the password: the username is useful context and the
        // password is not.
        Some((start, end)) => {
            out.push_str(&before_query[..start]);
            let userinfo = &before_query[start..end];
            match userinfo.split_once(':') {
                Some((user, _)) => {
                    out.push_str(user);
                    out.push(':');
                    out.push_str(MASK);
                }
                None => out.push_str(userinfo),
            }
            out.push_str(&before_query[end..]);
        }
        None => out.push_str(before_query),
    }

    if let Some(q) = query {
        out.push('?');
        for (i, pair) in q.split('&').enumerate() {
            if i > 0 {
                out.push('&');
            }
            let name = param_name(pair);
            if is_sensitive_query(name) {
                out.push_str(name);
                out.push('=');
                out.push_str(MASK);
            } else {
                out.push_str(pair);
            }
        }
    }
    Cow::Owned(out)
}

fn param_name(pair: &str) -> &str {
    pair.split_once('=').map_or(pair, |(n, _)| n)
}

/// Byte span of the `userinfo` portion of an authority, if present.
///
/// The `@` must come before the first `/` of the path, or `https://host/a@b` would be read as
/// having credentials.
fn userinfo_span(u: &str) -> Option<(usize, usize)> {
    let scheme_end = u.find("://").map(|i| i + 3)?;
    let rest = &u[scheme_end..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let at = rest[..authority_end].find('@')?;
    Some((scheme_end, scheme_end + at))
}

/// Last-resort scrub of free text that may embed a known secret.
///
/// Used for messages that came from somewhere we do not control (a transport error string can
/// quote the URL it was given). Pass the secrets we hold; each is replaced wherever it
/// appears. Empty and very short secrets are ignored, because replacing every occurrence of a
/// 3-character string would mangle the message without protecting anything.
pub fn text<'a>(s: &'a str, secrets: &[&str]) -> Cow<'a, str> {
    let mut out: Cow<'a, str> = Cow::Borrowed(s);
    for secret in secrets.iter().filter(|s| s.len() >= 8) {
        if out.contains(secret) {
            out = Cow::Owned(out.replace(secret, MASK));
        }
    }
    // A URL inside the message may still carry `?token=`.
    match url(&out) {
        Cow::Borrowed(_) => out,
        Cow::Owned(scrubbed) => Cow::Owned(scrubbed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug: `gea --debug` printing the full `Authorization` header, which is then pasted
    /// verbatim into a public issue.
    #[test]
    fn authorization_value_never_survives() {
        let v = header_value("Authorization", "token abcdef0123456789");
        assert_eq!(v, "token <redacted>");
        assert!(!v.contains("abcdef"));
    }

    /// Header names are case-insensitive on the wire, so a hand-built `AUTHORIZATION` must be
    /// caught too.
    #[test]
    fn header_matching_ignores_case() {
        assert_eq!(header_value("AUTHORIZATION", "Basic Zm9vOmJhcg=="), "Basic <redacted>");
        assert_eq!(header_value("X-Gitea-OTP", "123456"), MASK);
        assert_eq!(header_value("SUDO", "root"), MASK);
    }

    #[test]
    fn non_sensitive_headers_pass_through_unchanged() {
        assert_eq!(header_value("Accept", "application/json"), "application/json");
        assert_eq!(
            header_value("Link", "<https://x/?page=2>; rel=\"next\""),
            "<https://x/?page=2>; rel=\"next\""
        );
    }

    /// Gitea still accepts `?token=`; a URL logged with one is just as leaky as a header.
    #[test]
    fn query_token_is_scrubbed_but_other_params_are_kept() {
        let out = url("https://git.example.org/api/v1/user?token=abcdef0123456789&page=2");
        assert_eq!(out, "https://git.example.org/api/v1/user?token=<redacted>&page=2");
        assert_eq!(url("https://x/y?access_token=s3cret"), "https://x/y?access_token=<redacted>");
    }

    #[test]
    fn embedded_password_is_scrubbed_username_is_kept() {
        let out = url("https://alice:hunter2@git.example.org/api/v1/user");
        assert_eq!(out, "https://alice:<redacted>@git.example.org/api/v1/user");
    }

    /// `@` after the authority is part of the path, not credentials — redacting there would
    /// corrupt a legitimate URL.
    #[test]
    fn at_sign_in_path_is_not_mistaken_for_credentials() {
        let u = "https://git.example.org/api/v1/repos/o/r/contents/mail@example.txt";
        assert!(matches!(url(u), Cow::Borrowed(_)), "should not have rewritten {u}");
    }

    #[test]
    fn clean_urls_are_borrowed_not_reallocated() {
        assert!(matches!(url("https://git.example.org/api/v1/user?page=2"), Cow::Borrowed(_)));
    }

    #[test]
    fn text_replaces_a_known_secret_anywhere_it_appears() {
        let s = "connection refused while sending abcdef0123456789 to host";
        assert_eq!(
            text(s, &["abcdef0123456789"]),
            "connection refused while sending <redacted> to host"
        );
        // Too short to replace safely; would mangle unrelated text.
        assert_eq!(text("abc happened", &["abc"]), "abc happened");
    }
}

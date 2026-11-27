//! Turning a git remote URL into `(host, subpath, owner, name)`.
//!
//! Every row of the test table at the bottom of this file is a bug someone actually hits, so
//! the parser is written against that table rather than against a URL grammar. The two rules
//! that are easy to get wrong:
//!
//! **1. Match the host before splitting `owner/name`.** The tempting shortcut is "take the
//! last two path segments". It happens to work for `https://example.org/gitea/owner/repo`
//! — and that is exactly the problem: it works by accident, and then breaks on a trailing
//! slash, on a bare `https://host/owner`, and on any future nested-group layout. Doing a
//! longest-prefix match against the *configured* hosts first means the subpath is removed
//! because we know it is a subpath, not because of where it happens to sit in the string.
//!
//! **2. `:` in an scp-like URL is not a port.** `git@host:owner/repo.git` has no scheme and
//! no port; the colon separates host from path. But people do write
//! `git@host:2222/owner/repo.git`, so the disambiguation is "all digits up to the next `/`
//! is a port, anything else is a path". Note that `git` itself would read `2222/owner/repo`
//! as a path; we deviate deliberately, because a numerically-named owner is far less likely
//! than someone mixing up the two SSH URL syntaxes.
//!
//! ## Known limitation, deferred on purpose
//!
//! `~/.ssh/config` `Host` aliases are **not** resolved. `git@work-forge:o/r` yields the host
//! `work-forge`, which matches no configured host, and the resulting
//! [`crate::ErrorKind::RemoteHostUnknown`] says so and points at `gea repo set-default`,
//! which bypasses URL parsing entirely. Reading `ssh_config` properly means implementing
//! `Match`, `Include`, and token expansion; shelling out to `ssh -G` costs a process spawn on
//! every invocation. Neither is worth it before someone asks.

use crate::config::{HostKey, hosts};
use crate::types::RepoSlug;

/// A remote URL, decomposed. Credentials are dropped during parsing and never stored, so no
/// field of this struct can leak a token into a log line or a `--debug` dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    /// Lowercased scheme, or `None` for the scp-like form.
    pub scheme: Option<String>,
    /// Lowercased hostname. Bracketed for IPv6 literals (`[::1]`).
    pub host: String,
    /// The port exactly as written, before any default-port or SSH filtering.
    pub port: Option<u16>,
    /// The path with no leading or trailing slash and no trailing `.git`. Still includes any
    /// subpath prefix, because only the configured host list can tell a prefix from an owner.
    pub path: String,
}

impl RemoteUrl {
    /// Whether this URL reaches the instance over SSH (or the bare `git://` protocol).
    ///
    /// The distinction exists solely for the port: an SSH port is unrelated to the port the
    /// API listens on, so `ssh://git@host:2222/o/r` and `https://host/o/r` are the same
    /// instance. Treating 2222 as part of the host identity would leave the SSH remote
    /// matching nothing.
    pub fn is_ssh_like(&self) -> bool {
        matches!(self.scheme.as_deref(), None | Some("ssh") | Some("git") | Some("git+ssh"))
    }

    /// The host identity: `host[:port]`, with the port kept only when it distinguishes one
    /// instance from another — that is, for HTTP(S) on a non-default port.
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) if !self.is_ssh_like() && p != 80 && p != 443 => {
                format!("{}:{p}", self.host)
            }
            _ => self.host.clone(),
        }
    }

    fn matches(&self, key: &HostKey) -> bool {
        if self.is_ssh_like() {
            // Compare hostnames only: neither side's port is meaningful here.
            key.host() == self.host
        } else {
            key.authority() == self.authority()
        }
    }
}

/// The outcome of matching a remote URL against the configured hosts.
///
/// Four cases rather than an `Option`, because the caller needs to tell them apart: a URL we
/// could not parse is skipped silently, an unconfigured host becomes
/// [`crate::ErrorKind::RemoteHostUnknown`] naming that host, and a matched host with a
/// non-repository path is a different message again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    Matched {
        host: &'a HostKey,
        slug: RepoSlug,
    },
    /// Parsed, but no configured host matches. `host` is what we saw, ready for an error.
    UnknownHost {
        host: String,
    },
    /// A configured host matched, but the remaining path is not exactly `owner/name`.
    NotARepoPath {
        host: &'a HostKey,
        path: String,
    },
    /// Not a URL this parser understands — a local path, a `file://` URL, junk.
    Unparseable,
}

/// Parses a remote URL. Returns `None` for anything that is not a forge URL: a local path, a
/// `file://` clone, an empty string.
///
/// Local paths are rejected rather than best-effort parsed because a bare directory has no
/// host, and inventing one would produce a confident, wrong answer.
pub fn parse(input: &str) -> Option<RemoteUrl> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    // Local paths and explicit file URLs have no host to speak of.
    if t.starts_with('/') || t.starts_with('.') || t.starts_with('~') {
        return None;
    }

    let (scheme, rest) = match t.split_once("://") {
        Some((s, r)) => (Some(s.to_ascii_lowercase()), r),
        None => (None, t),
    };
    if matches!(scheme.as_deref(), Some("file")) {
        return None;
    }

    let (host, port, path) = match scheme {
        Some(_) => {
            // scheme://[user[:pass]@]host[:port]/path
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let authority = strip_userinfo(authority);
            let (host, port) = split_host_port(authority)?;
            (host, port, path.to_owned())
        }
        None => {
            // scp-like: [user@]host:[port/]path
            let after_user = strip_userinfo(rest);
            let (host_part, tail) = split_scp(after_user)?;
            let (host, host_port) = split_host_port(host_part)?;
            // `host:port:path` is not a thing; a port in the authority position of an
            // scp-like URL means the input was malformed.
            if host_port.is_some() {
                return None;
            }
            let (port, path) = split_leading_port(tail);
            (host, port, path.to_owned())
        }
    };

    if host.is_empty() || host.contains(' ') {
        return None;
    }

    Some(RemoteUrl { scheme, host: host.to_ascii_lowercase(), port, path: clean_path(&path) })
}

/// Matches a remote URL against the configured hosts, longest subpath prefix first, and only
/// then splits the remainder into exactly `owner/name`.
pub fn resolve<'a>(input: &str, known: &'a [HostKey]) -> Resolution<'a> {
    let Some(url) = parse(input) else {
        return Resolution::Unparseable;
    };

    let segments = hosts::split_segments(&url.path);
    // Authority filtering is the only thing this does differently from `Hosts::match_prefix`:
    // an SSH remote's port is not the API port, so `RemoteUrl::matches` compares hostnames
    // for SSH and `host:port` for HTTP(S). The prefix walk itself is shared, so the two
    // callers cannot drift apart.
    let candidates = known.iter().filter(|k| url.matches(k));

    let Some((host, skip)) = hosts::longest_subpath_prefix(candidates, &segments) else {
        return Resolution::UnknownHost { host: url.authority() };
    };

    match &segments[skip..] {
        [owner, name] => Resolution::Matched { host, slug: RepoSlug::new(*owner, *name) },
        rest => Resolution::NotARepoPath { host, path: rest.join("/") },
    }
}

/// Drops `user[:password]@`. Userinfo cannot contain an unescaped `@`, so the *last* `@` is
/// the separator; splitting on the first would corrupt an authority for no benefit.
fn strip_userinfo(authority: &str) -> &str {
    authority.rsplit_once('@').map_or(authority, |(_creds, rest)| rest)
}

/// Splits `host[:port]`, keeping IPv6 literals bracketed.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &authority[..end + 2];
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        return Some((host, port));
    }
    match authority.split_once(':') {
        None => Some((authority, None)),
        Some((h, "")) => Some((h, None)),
        Some((h, p)) => Some((h, Some(p.parse().ok()?))),
    }
}

/// Splits the scp-like `host:tail` at the colon that separates host from path.
fn split_scp(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let tail = rest[end + 1..].strip_prefix(':')?;
        return Some((&s[..end + 2], tail));
    }
    let (host, tail) = s.split_once(':')?;
    Some((host, tail))
}

/// The scp-like port disambiguation: leading digits followed by `/` are a port, anything
/// else is the start of the path. `:owner/repo` versus `:2222/owner/repo`.
fn split_leading_port(tail: &str) -> (Option<u16>, &str) {
    let Some((head, rest)) = tail.split_once('/') else {
        return (None, tail);
    };
    if !head.is_empty()
        && head.bytes().all(|b| b.is_ascii_digit())
        && let Ok(p) = head.parse::<u16>()
    {
        return (Some(p), rest);
    }
    (None, tail)
}

/// Normalizes a path: no leading or trailing slashes, no trailing `.git`.
///
/// Order matters — `trim_end_matches('/')` before stripping `.git`, so
/// `https://host/o/r.git/` works. Trailing slashes come from copy-pasting out of a browser
/// address bar and are the reason "last two segments" parsers produce an empty repo name.
fn clean_path(path: &str) -> String {
    let p = path.trim_matches('/');
    p.strip_suffix(".git").unwrap_or(p).trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> HostKey {
        HostKey::parse(s).unwrap()
    }

    /// Every row here is a real bug: a form that a naive parser gets wrong, and that a user
    /// then reports as "gea says my repo is not on a configured host".
    #[test]
    fn every_remote_url_form() {
        struct Row {
            input: &'static str,
            /// The configured host this remote must match.
            configured: &'static str,
            /// Expected host key, subpath, owner, name after resolution.
            host: &'static str,
            subpath: &'static str,
            owner: &'static str,
            name: &'static str,
            note: &'static str,
        }

        let rows = [
            Row {
                input: "https://git.example.org/owner/repo.git",
                configured: "git.example.org",
                host: "git.example.org",
                subpath: "",
                owner: "owner",
                name: "repo",
                note: "trailing .git is stripped",
            },
            Row {
                input: "http://git.example.org:3000/owner/repo",
                configured: "http://git.example.org:3000",
                host: "git.example.org:3000",
                subpath: "",
                owner: "owner",
                name: "repo",
                note: "a non-default HTTP port is part of the host identity",
            },
            Row {
                input: "ssh://git@git.example.org:2222/owner/repo.git",
                configured: "https://git.example.org",
                host: "git.example.org",
                subpath: "",
                owner: "owner",
                name: "repo",
                note: "the SSH port is ignored: it is not the API port",
            },
            Row {
                input: "git@git.example.org:owner/repo.git",
                configured: "git.example.org",
                host: "git.example.org",
                subpath: "",
                owner: "owner",
                name: "repo",
                note: "scp-like: no scheme, and ':' separates host from path, not a port",
            },
            Row {
                input: "git@git.example.org:2222/owner/repo.git",
                configured: "git.example.org",
                host: "git.example.org",
                subpath: "",
                owner: "owner",
                name: "repo",
                note: "scp-like WITH a port: all digits up to the next '/'",
            },
            Row {
                input: "https://example.org/gitea/owner/repo.git",
                configured: "https://example.org/gitea",
                host: "example.org/gitea",
                subpath: "gitea",
                owner: "owner",
                name: "repo",
                note: "subpath install: ROOT_URL carries a path prefix",
            },
            Row {
                input: "https://user:tok@git.example.org/o/r.git",
                configured: "git.example.org",
                host: "git.example.org",
                subpath: "",
                owner: "o",
                name: "r",
                note: "credentials are stripped and never retained",
            },
        ];

        for r in rows {
            let known = [key(r.configured)];
            match resolve(r.input, &known) {
                Resolution::Matched { host, slug } => {
                    assert_eq!(host.as_str(), r.host, "host for {:?} ({})", r.input, r.note);
                    assert_eq!(host.subpath(), r.subpath, "subpath for {:?}", r.input);
                    assert_eq!(slug.owner, r.owner, "owner for {:?}", r.input);
                    assert_eq!(slug.name, r.name, "name for {:?}", r.input);
                }
                other => panic!("{:?} ({}) did not resolve: {other:?}", r.input, r.note),
            }
            // No form may retain credentials, in any field, ever.
            let parsed = parse(r.input).unwrap();
            assert!(!format!("{parsed:?}").contains("tok"), "credentials kept: {parsed:?}");
        }
    }

    #[test]
    fn scp_like_colon_is_not_a_port() {
        // The single most common misparse: reading `owner` as a port number and failing.
        let u = parse("git@git.example.org:owner/repo.git").unwrap();
        assert_eq!(u.host, "git.example.org");
        assert_eq!(u.port, None);
        assert_eq!(u.path, "owner/repo");
        assert!(u.is_ssh_like());
    }

    #[test]
    fn scp_like_port_is_recognised_by_digits_then_slash() {
        let u = parse("git@git.example.org:2222/owner/repo.git").unwrap();
        assert_eq!(u.port, Some(2222));
        assert_eq!(u.path, "owner/repo");
        // ...and the port does not reach the host identity, because SSH != API.
        assert_eq!(u.authority(), "git.example.org");
    }

    #[test]
    fn ssh_port_does_not_break_matching_but_http_port_does() {
        let known = [key("git.example.org")];
        // An SSH remote on a nonstandard port still matches the plain configured host.
        assert!(matches!(
            resolve("ssh://git@git.example.org:2222/o/r", &known),
            Resolution::Matched { .. }
        ));
        // An HTTP remote on a nonstandard port is a *different* instance and must not.
        assert!(matches!(
            resolve("http://git.example.org:3000/o/r", &known),
            Resolution::UnknownHost { .. }
        ));
    }

    #[test]
    fn longest_prefix_before_owner_name_split() {
        // With both hosts configured, `gitea` must be read as the subpath and not as the
        // owner — the "last two segments" rule gets this right by accident and then gets
        // `https://example.org/gitea/owner` wrong.
        let known = [key("https://example.org"), key("https://example.org/gitea")];
        match resolve("https://example.org/gitea/owner/repo.git", &known) {
            Resolution::Matched { host, slug } => {
                assert_eq!(host.as_str(), "example.org/gitea");
                assert_eq!(slug.to_string(), "owner/repo");
            }
            other => panic!("{other:?}"),
        }
        // And a root-install repo on the same authority still resolves to the root host.
        match resolve("https://example.org/owner/repo", &known) {
            Resolution::Matched { host, slug } => {
                assert_eq!(host.as_str(), "example.org");
                assert_eq!(slug.to_string(), "owner/repo");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn trailing_slashes_and_git_suffix() {
        for input in [
            "https://git.example.org/owner/repo/",
            "https://git.example.org/owner/repo.git",
            "https://git.example.org/owner/repo.git/",
        ] {
            let u = parse(input).unwrap();
            assert_eq!(u.path, "owner/repo", "input {input:?}");
        }
    }

    #[test]
    fn matched_host_with_a_non_repo_path() {
        let known = [key("git.example.org")];
        // One segment is not a repository, and neither are three.
        assert!(matches!(
            resolve("https://git.example.org/owner", &known),
            Resolution::NotARepoPath { .. }
        ));
        assert!(matches!(
            resolve("https://git.example.org/a/b/c", &known),
            Resolution::NotARepoPath { .. }
        ));
    }

    #[test]
    fn local_paths_and_junk_are_unparseable_not_wrong() {
        // Skipping these is the point: a local remote is not a forge and must not produce a
        // confident, wrong host.
        for input in ["/srv/git/repo.git", "../sibling", "~/repos/x", "file:///srv/git/x", ""] {
            assert_eq!(parse(input), None, "input {input:?}");
        }
    }

    #[test]
    fn ssh_alias_is_a_known_limitation() {
        // Documented and deferred: ~/.ssh/config Host aliases are not resolved. The value of
        // this test is that the failure is the *specific* one whose message mentions SSH
        // aliases, not a silent misparse.
        let known = [key("git.example.org")];
        match resolve("git@work-forge:o/r.git", &known) {
            Resolution::UnknownHost { host } => assert_eq!(host, "work-forge"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ipv6_literals() {
        let u = parse("http://[::1]:3000/o/r.git").unwrap();
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.port, Some(3000));
        assert_eq!(u.authority(), "[::1]:3000");

        let u = parse("git@[::1]:o/r.git").unwrap();
        assert_eq!(u.host, "[::1]");
        assert_eq!(u.path, "o/r");
    }

    #[test]
    fn default_ports_do_not_change_identity() {
        let known = [key("git.example.org")];
        for input in ["https://git.example.org:443/o/r", "http://git.example.org:80/o/r"] {
            assert!(
                matches!(resolve(input, &known), Resolution::Matched { .. }),
                "input {input:?}"
            );
        }
    }
}

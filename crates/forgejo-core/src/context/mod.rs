//! Working out which repository, host, and login a command is about.
//!
//! The resolution order below is fixed, and every miss records an [`Attempt`] so that a
//! failure can *show its work*. "could not determine which repository to use" is a useless
//! message; "I checked -R, then $FJO_REPO, then git config, then 3 remotes, and here is what
//! each one said" is a message someone can act on.
//!
//! 1. `-R/--repo` — a [`RepoRef`], so `owner/name`, `host/owner/name`, or a URL. No git
//!    repository required, which is what makes `fjo pr list -R other/repo` work from `$HOME`.
//! 2. `$FJO_REPO`, then `$FORGEJO_REPO`.
//! 3. Not inside a git work tree → [`ErrorKind::RepoNotResolved`] carrying the attempts.
//! 4. git config `remote.<name>.fjo-resolved`, written by `fjo repo set-default`.
//! 5. Remote-name scoring: `upstream` 3, `forgejo` 2, `codeberg` 2, `origin` 1, everything
//!    else 0. Highest wins; a tie at the top is [`ErrorKind::AmbiguousRemote`].
//! 6. Remotes exist but none names a configured host →
//!    [`ErrorKind::RemoteHostUnknown`].
//!
//! # The host is decided here, and only here
//!
//! [`RepoContext::host`] is not decoration: it is the host the request is *sent to*, and the
//! caller must not resolve one of its own. `hosts.toml`'s `active` is the **last** resort, below
//! the checkout's own remotes, because a clone of `code.example/them/proj` is a statement about
//! which server the command is about — far more specific than a global default that was last
//! set by whichever `fjo auth switch` ran most recently.
//!
//! Getting that order wrong is not a cosmetic bug. Resolving the *slug* from the remote and the
//! *host* from `active` sends `them/proj` to a server nobody named; the visible outcome is a
//! 404, and the invisible one — when that server happens to hold a repository of the same name
//! — is a confident, green answer about the wrong repository on the wrong instance.
//!
//! So the host precedence is:
//!
//! 1. `--host`, `$FJO_HOST`, `$FORGEJO_HOST` — the explicit instruments, and they win
//!    everywhere, including over a remote that names another host.
//! 2. A host named inside `-R`/`$FJO_REPO` (`host/owner/name` or a URL). Disagreeing with 1 is
//!    [`ErrorKind::Usage`] rather than a silent preference.
//! 3. The git remotes, via [`host_from_remotes`] — `fjo-resolved` first, then remote-name
//!    scoring, the same walk and the same order this module uses for the repository.
//! 4. `active`, or the sole configured host.

// See the note in `crate::config`: `crate::Error` exceeds clippy's 128-byte threshold because
// `RequestCtx` is stored inline, so this fires on every fallible function here. The fix
// belongs in `error/mod.rs`.
#![allow(clippy::result_large_err)]

pub mod git;
pub mod remote_url;

use crate::config::{Env, HostKey, Hosts};
use crate::error::{Attempt, Error, ErrorKind, RemoteCandidate, Result};
use crate::types::{RepoRef, RepoSlug};

pub use git::{FakeGit, GitCli, GitCtx, Remote};
pub use remote_url::{RemoteUrl, Resolution};

/// The git config key suffix `fjo repo set-default` writes.
///
/// Namespaced per remote (`remote.origin.fjo-resolved`) exactly as `gh` does with
/// `gh-resolved`, so both tools can coexist in one clone without fighting.
pub const RESOLVED_SUFFIX: &str = "fjo-resolved";

/// The `fjo-resolved` value meaning "this remote is the repository".
pub const RESOLVED_BASE: &str = "base";

/// The `fjo-resolved` value meaning "the user said none of these remotes".
pub const RESOLVED_NONE: &str = "NONE";

/// `remote.<name>.fjo-resolved`.
pub fn resolved_key(remote: &str) -> String {
    format!("remote.{remote}.{RESOLVED_SUFFIX}")
}

/// How much we prefer a remote by name.
///
/// `upstream` outranks `origin` so that in a fork checkout — the normal contributor setup —
/// `fjo pr create` and `fjo issue list` default to the upstream project rather than to the
/// contributor's own fork, which is where the pull request needs to go and where the issues
/// live. `forgejo` and `codeberg` sit between them because a user who names a remote after
/// the forge is naming the canonical one.
pub fn remote_score(name: &str) -> u8 {
    match name {
        "upstream" => 3,
        "forgejo" | "codeberg" => 2,
        "origin" => 1,
        _ => 0,
    }
}

/// The order remotes are considered in: highest [`remote_score`] first, then alphabetically.
///
/// Shared by [`resolve_repo`] and [`host_from_remotes`] precisely so the host and the
/// repository cannot be decided by two different remotes. Alphabetical second means a
/// `--debug` transcript is reproducible rather than dependent on git's output order.
fn by_preference(a: &Remote, b: &Remote) -> std::cmp::Ordering {
    remote_score(&b.name).cmp(&remote_score(&a.name)).then_with(|| a.name.cmp(&b.name))
}

/// The host this checkout is on, or `None` when its remotes say nothing usable.
///
/// "Nothing usable" covers every shape that is not an answer: outside a work tree, no remotes,
/// remotes that are local paths, remotes on a host no `hosts.toml` entry matches, and a remote
/// the user opted out of with `fjo repo set-default --none`. All of them return `Ok(None)`
/// rather than an error, because every caller has a documented fallback (`active`) and because
/// failing here would break `fjo api user` inside an unrelated clone — a GitHub checkout is not
/// a reason for fjo to stop working.
///
/// This reads the tree in the same order as [`resolve_repo`] and honours the same
/// `fjo-resolved` override, so the two always name the same host.
pub fn host_from_remotes(hosts: &Hosts, git: &dyn GitCtx) -> Result<Option<HostKey>> {
    if git.git_dir()?.is_none() {
        return Ok(None);
    }
    let mut remotes = git.remotes()?;
    remotes.sort_by(by_preference);
    let keys = hosts.keys();

    // `fjo repo set-default` is the documented escape hatch, so it has to be able to move the
    // host as well as the repository.
    let configured = git.config_get_regexp(&format!(r"^remote\..*\.{RESOLVED_SUFFIX}$"))?;
    for remote in &remotes {
        let key = resolved_key(&remote.name);
        let Some((_, value)) = configured.iter().find(|(k, _)| *k == key) else {
            continue;
        };
        match value.trim() {
            // The user said "none of these remotes". Inferring a host from one of them anyway
            // would be re-asking the question they already answered.
            RESOLVED_NONE => return Ok(None),
            RESOLVED_BASE => {
                for url in remote.urls() {
                    if let Resolution::Matched { host, .. } = remote_url::resolve(url, &keys) {
                        return Ok(Some(host.clone()));
                    }
                }
            }
            // `host/owner/name` names the host outright. A bare `owner/name` names only the
            // repository, so it is no answer to *this* question and the walk continues — which
            // is also what keeps this function from recursing back into `host_from_pref`.
            other => {
                if let Ok(r) = other.parse::<RepoRef>()
                    && let Some(h) = &r.host
                    && let Ok(key) = HostKey::parse(h)
                    && hosts.contains(&key)
                {
                    return Ok(Some(key));
                }
            }
        }
    }

    for remote in &remotes {
        for url in remote.urls() {
            if let Resolution::Matched { host, .. } = remote_url::resolve(url, &keys) {
                return Ok(Some(host.clone()));
            }
        }
    }
    Ok(None)
}

/// Which rule produced a [`RepoContext`]. Surfaced by `--debug` and by
/// `fjo repo set-default --view`, both of which exist to answer "why is fjo talking to
/// *that* repository?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSource {
    /// `-R/--repo`.
    Flag,
    /// `$FJO_REPO` or `$FORGEJO_REPO`.
    Env { var: &'static str },
    /// git config `remote.<remote>.fjo-resolved = <value>`.
    GitConfig { remote: String, value: String },
    /// Remote-name scoring picked this remote.
    Remote { remote: String, score: u8 },
}

impl std::fmt::Display for RepoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag => f.write_str("-R/--repo"),
            Self::Env { var } => write!(f, "${var}"),
            Self::GitConfig { remote, value } => {
                write!(f, "git config remote.{remote}.{RESOLVED_SUFFIX}={value}")
            }
            Self::Remote { remote, score } => write!(f, "remote {remote:?} (score {score})"),
        }
    }
}

/// The resolved target of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoContext {
    pub host: HostKey,
    /// `None` when the host has no login yet. Deliberately not an error here: a missing
    /// credential is the HTTP layer's problem to report, with its own remedy, and failing
    /// during *repository* resolution would print the wrong advice.
    pub login: Option<String>,
    pub slug: RepoSlug,
    pub source: RepoSource,
}

/// Command-line inputs to resolution. All optional; everything else comes from the
/// environment, git, and `hosts.toml`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveOptions<'a> {
    pub repo: Option<&'a RepoRef>,
    pub host: Option<&'a str>,
    pub login: Option<&'a str>,
}

/// Requires a git work tree, for commands that genuinely cannot work without one
/// (`fjo repo set-default`, `fjo pr checkout`).
///
/// [`resolve_repo`] deliberately does *not* use this: it reports
/// [`ErrorKind::RepoNotResolved`] instead, because that variant carries the list of things it
/// tried, and "not a git repository" is only the last of them.
pub fn require_git_repo(git: &dyn GitCtx) -> Result<std::path::PathBuf> {
    git.git_dir()?.ok_or_else(|| Error::new(ErrorKind::NotAGitRepo))
}

/// Resolves the repository a command is about. See the module docs for the order.
pub fn resolve_repo(
    opts: &ResolveOptions<'_>,
    hosts: &Hosts,
    git: &dyn GitCtx,
    env: &dyn Env,
) -> Result<RepoContext> {
    let mut tried: Vec<Attempt> = Vec::new();

    // The host the user asked for, and where they asked for it — the label is needed so a
    // conflict can name both sides.
    let host_pref: Option<(String, &'static str)> = opts
        .host
        .map(|h| (h.to_owned(), "--host"))
        .or_else(|| env.get("FJO_HOST").map(|v| (v, "$FJO_HOST")))
        .or_else(|| env.get("FORGEJO_HOST").map(|v| (v, "$FORGEJO_HOST")));

    let login_pref: Option<String> = opts
        .login
        .map(str::to_owned)
        .or_else(|| env.get("FJO_USER"))
        .or_else(|| env.get("FORGEJO_USER"));

    // ---------------------------------------------------------------- 1. -R/--repo
    if let Some(r) = opts.repo {
        let host = match (&r.host, &host_pref) {
            (Some(from_flag), Some((from_host, label))) => {
                let a = HostKey::parse(from_flag)?;
                let b = HostKey::parse(from_host)?;
                if a != b {
                    // Silently preferring one would send the request to a host the user did
                    // not name, which for a destructive command is unacceptable.
                    return Err(Error::new(ErrorKind::Usage(format!(
                        "-R names host {a}, but {label} says {b}; they must agree, so drop \
                         one of them"
                    ))));
                }
                a
            }
            (Some(from_flag), None) => HostKey::parse(from_flag)?,
            // Both arms go through `host_from_pref`, so `-R owner/name` inside a checkout picks
            // up that checkout's host instead of `active`.
            (None, _) => host_from_pref(hosts, &host_pref, git, env)?,
        };
        return finish(hosts, host, login_pref.as_deref(), r.slug.clone(), RepoSource::Flag);
    }
    tried.push(Attempt::new("-R/--repo", "not given"));

    // ------------------------------------------------------- 2. $FJO_REPO, $FORGEJO_REPO
    for (var, label) in [("FJO_REPO", "$FJO_REPO"), ("FORGEJO_REPO", "$FORGEJO_REPO")] {
        let Some(value) = env.get(var) else {
            tried.push(Attempt::new(label, "not set"));
            continue;
        };
        let r: RepoRef = value.parse().map_err(|e| {
            Error::new(ErrorKind::Usage(format!("${var} is not a repository: {e}")))
        })?;
        let host = match &r.host {
            Some(h) => HostKey::parse(h)?,
            None => host_from_pref(hosts, &host_pref, git, env)?,
        };
        let var: &'static str = if var == "FJO_REPO" { "FJO_REPO" } else { "FORGEJO_REPO" };
        return finish(hosts, host, login_pref.as_deref(), r.slug, RepoSource::Env { var });
    }

    // -------------------------------------------------------------- 3. a git work tree?
    if git.git_dir()?.is_none() {
        tried.push(Attempt::new(
            "git work tree",
            "not inside one, so there are no remotes to inspect",
        ));
        return Err(Error::new(ErrorKind::RepoNotResolved { tried }));
    }

    let remotes = git.remotes()?;
    // Highest score first, then alphabetically, so the order this walks in is deterministic
    // and a `--debug` transcript is reproducible.
    let mut ordered: Vec<&Remote> = remotes.iter().collect();
    ordered.sort_by(|a, b| by_preference(a, b));

    // ------------------------------------------- 4. remote.<name>.fjo-resolved
    let configured = git.config_get_regexp(&format!(r"^remote\..*\.{RESOLVED_SUFFIX}$"))?;
    for remote in &ordered {
        let key = resolved_key(&remote.name);
        let Some((_, value)) = configured.iter().find(|(k, _)| *k == key) else {
            continue;
        };
        let value = value.trim();

        if value == RESOLVED_NONE {
            // The user explicitly said "none of these remotes". Stop, rather than falling
            // through to scoring and re-asking the question they already answered.
            tried.push(Attempt::new(
                "git config fjo-resolved",
                format!(
                    "{key} is {RESOLVED_NONE}: you opted out for this remote. Pass \
                     -R owner/name, or run `fjo repo set-default` to choose one"
                ),
            ));
            return Err(Error::new(ErrorKind::RepoNotResolved { tried }));
        }

        if value == RESOLVED_BASE {
            // This remote *is* the repository, so its URL is authoritative.
            let keys = hosts.keys();
            let mut seen_host: Option<String> = None;
            for url in remote.urls() {
                match remote_url::resolve(url, &keys) {
                    Resolution::Matched { host, slug } => {
                        return finish(
                            hosts,
                            // `--host` still wins: one `Client` speaks to one host, and that
                            // host is whatever the user named explicitly.
                            explicit_or(hosts, &host_pref, host)?,
                            login_pref.as_deref(),
                            slug,
                            RepoSource::GitConfig {
                                remote: remote.name.clone(),
                                value: value.to_owned(),
                            },
                        );
                    }
                    Resolution::UnknownHost { host } => {
                        seen_host.get_or_insert(host);
                    }
                    Resolution::NotARepoPath { .. } | Resolution::Unparseable => continue,
                }
            }
            if let Some(host) = seen_host {
                return Err(Error::new(ErrorKind::RemoteHostUnknown {
                    remote: remote.name.clone(),
                    host,
                }));
            }
            tried.push(Attempt::new(
                "git config fjo-resolved",
                format!(
                    "{key} is `{RESOLVED_BASE}`, but {}'s URL is not a repository URL",
                    remote.name
                ),
            ));
            continue;
        }

        // `owner/name` or `host/owner/name`, used verbatim — no URL parsing at all, which is
        // exactly why `set-default` is the documented escape hatch for SSH aliases and for
        // any URL shape this parser gets wrong.
        let r: RepoRef = value.parse().map_err(|e| {
            Error::new(ErrorKind::Usage(format!(
                "git config {key} is {value:?}, which is not `{RESOLVED_BASE}`, \
                 `{RESOLVED_NONE}`, `owner/name`, or `host/owner/name`: {e}"
            )))
        })?;
        let host = match &r.host {
            Some(h) => HostKey::parse(h)?,
            None => host_from_pref(hosts, &host_pref, git, env)?,
        };
        return finish(
            hosts,
            host,
            login_pref.as_deref(),
            r.slug,
            RepoSource::GitConfig { remote: remote.name.clone(), value: value.to_owned() },
        );
    }
    tried.push(Attempt::new("git config fjo-resolved", "not set on any remote"));

    // ------------------------------------------------------ 5. and 6. remote-name scoring
    let keys = hosts.keys();
    let mut matched: Vec<(u8, &Remote, &HostKey, RepoSlug)> = Vec::new();
    let mut unknown: Option<(String, String)> = None;

    for remote in &ordered {
        for url in remote.urls() {
            match remote_url::resolve(url, &keys) {
                Resolution::Matched { host, slug } => {
                    matched.push((remote_score(&remote.name), remote, host, slug));
                    break;
                }
                Resolution::UnknownHost { host } => {
                    // Remember the first (highest-scoring) unconfigured host, so the error
                    // names the remote the user most likely meant.
                    if unknown.is_none() {
                        unknown = Some((remote.name.clone(), host));
                    }
                }
                // Skip rather than fail: a local-path remote, or a `file://` mirror, is not
                // an error, it is just not a forge.
                Resolution::NotARepoPath { .. } | Resolution::Unparseable => {}
            }
        }
    }

    if matched.is_empty() {
        if let Some((remote, host)) = unknown {
            return Err(Error::new(ErrorKind::RemoteHostUnknown { remote, host }));
        }
        tried.push(Attempt::new(
            "git remotes",
            if remotes.is_empty() {
                "this repository has no remotes".to_owned()
            } else {
                format!(
                    "none of {} named a repository on a known host",
                    remotes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
                )
            },
        ));
        return Err(Error::new(ErrorKind::RepoNotResolved { tried }));
    }

    let top = matched.iter().map(|(s, ..)| *s).max().unwrap_or(0);
    let at_top: Vec<&(u8, &Remote, &HostKey, RepoSlug)> =
        matched.iter().filter(|(s, ..)| *s == top).collect();

    if at_top.len() > 1 {
        // Two equally-plausible remotes. Guessing would silently operate on the wrong
        // repository, so name them all and let `fjo repo set-default` settle it.
        return Err(Error::new(ErrorKind::AmbiguousRemote {
            candidates: at_top
                .iter()
                .map(|(_, remote, host, slug)| RemoteCandidate {
                    remote: remote.name.clone(),
                    host: host.to_string(),
                    slug: slug.to_string(),
                })
                .collect(),
        }));
    }

    let (score, remote, host, slug) = at_top[0];
    finish(
        hosts,
        explicit_or(hosts, &host_pref, host)?,
        login_pref.as_deref(),
        slug.clone(),
        RepoSource::Remote { remote: remote.name.clone(), score: *score },
    )
}

/// An explicit `--host`/env preference if there is one, otherwise the host a remote's URL named.
///
/// Without this, `fjo pr list --host other.example` inside a clone of `code.example/them/proj`
/// would leave [`RepoContext::host`] saying `code.example` while the client talked to
/// `other.example` — the two disagreeing about the same command is the whole defect this module
/// exists to prevent, and a field that is right only sometimes is worse than no field at all.
fn explicit_or(
    hosts: &Hosts,
    pref: &Option<(String, &'static str)>,
    matched: &HostKey,
) -> Result<HostKey> {
    match pref {
        Some((h, _)) => {
            let key = HostKey::parse(h)?;
            if hosts.contains(&key) {
                Ok(key)
            } else {
                Err(Error::new(ErrorKind::UnknownHost { given: h.clone(), known: hosts.known() }))
            }
        }
        None => Ok(matched.clone()),
    }
}

/// The host to use when the repository itself did not name one: the `--host`/env preference if
/// given, then the checkout's own remotes, and only then `active`.
///
/// The middle step is the one that is easy to leave out, and leaving it out is the bug this
/// module's docs describe: `fjo pr list -R them/sibling` typed inside a clone of
/// `code.example/me/proj` means the sibling repository *on this server*, not the same path on
/// whichever host `fjo auth switch` last made active.
fn host_from_pref(
    hosts: &Hosts,
    pref: &Option<(String, &'static str)>,
    git: &dyn GitCtx,
    env: &dyn Env,
) -> Result<HostKey> {
    match pref {
        Some((h, _)) => {
            let key = HostKey::parse(h)?;
            if hosts.contains(&key) {
                Ok(key)
            } else {
                Err(Error::new(ErrorKind::UnknownHost { given: h.clone(), known: hosts.known() }))
            }
        }
        None => match host_from_remotes(hosts, git)? {
            Some(key) => Ok(key),
            None => hosts.resolve_host(None, env),
        },
    }
}

/// Validates the host and attaches a login.
fn finish(
    hosts: &Hosts,
    host: HostKey,
    login: Option<&str>,
    slug: RepoSlug,
    source: RepoSource,
) -> Result<RepoContext> {
    if !hosts.contains(&host) {
        return Err(Error::new(ErrorKind::UnknownHost {
            given: host.to_string(),
            known: hosts.known(),
        }));
    }
    // An explicitly requested login that does not exist is a usage error; an absent default
    // is not, because the auth layer reports that far better than we can here.
    let login = match login {
        Some(u) => Some(hosts.resolve_login(&host, Some(u))?),
        None => hosts.resolve_login(&host, None).ok(),
    };
    Ok(RepoContext { host, login, slug, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MapEnv;

    fn hosts_with(inputs: &[&str]) -> Hosts {
        let mut h = Hosts::empty_at(std::path::Path::new("/nonexistent/hosts.toml"));
        for i in inputs {
            let key = h.add_host(i).unwrap().name.clone();
            h.add_login(&key, "me", None, vec![], None).unwrap();
        }
        h
    }

    fn resolve(
        git: &dyn GitCtx,
        hosts: &Hosts,
        env: &MapEnv,
        opts: ResolveOptions<'_>,
    ) -> Result<RepoContext> {
        resolve_repo(&opts, hosts, git, env)
    }

    #[test]
    fn fork_checkout_defaults_to_upstream() {
        // The scoring rule's whole purpose: in a fork, a pull request belongs to the
        // upstream project, not to the contributor's own copy. Getting this backwards means
        // every `fjo pr create` opens a PR against the fork.
        let git = FakeGit::repo()
            .with_remote("origin", "https://git.example.org/me/fork.git")
            .with_remote("upstream", "https://git.example.org/them/proj.git");
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "them/proj");
        assert_eq!(ctx.source, RepoSource::Remote { remote: "upstream".into(), score: 3 });
        assert_eq!(ctx.login.as_deref(), Some("me"));
    }

    #[test]
    fn origin_wins_over_an_unrecognised_name() {
        let git = FakeGit::repo()
            .with_remote("origin", "https://git.example.org/me/proj.git")
            .with_remote("mirror", "https://git.example.org/backup/proj.git");
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "me/proj");
    }

    #[test]
    fn a_tie_at_the_top_is_ambiguous() {
        // Two zero-score remotes: guessing would silently target the wrong repository.
        let git = FakeGit::repo()
            .with_remote("alpha", "https://git.example.org/a/x.git")
            .with_remote("beta", "https://git.example.org/b/x.git");
        let hosts = hosts_with(&["git.example.org"]);
        let err = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap_err();
        match &*err.kind {
            ErrorKind::AmbiguousRemote { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].remote, "alpha");
                assert_eq!(candidates[1].slug, "b/x");
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn fjo_resolved_base_uses_that_remotes_url() {
        let git = FakeGit::repo()
            .with_remote("origin", "https://git.example.org/me/fork.git")
            .with_remote("upstream", "https://git.example.org/them/proj.git")
            .with_config(&resolved_key("origin"), RESOLVED_BASE);
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        // `base` on origin beats upstream's higher score: the user said so explicitly.
        assert_eq!(ctx.slug.to_string(), "me/fork");
        assert_eq!(
            ctx.source,
            RepoSource::GitConfig { remote: "origin".into(), value: "base".into() }
        );
    }

    #[test]
    fn fjo_resolved_owner_name_bypasses_url_parsing() {
        // The escape hatch for an SSH alias: the remote URL is unparseable, and set-default
        // still works because it never looks at the URL.
        let git = FakeGit::repo()
            .with_remote("origin", "git@work-forge:whatever/thing.git")
            .with_config(&resolved_key("origin"), "them/proj");
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "them/proj");
        assert_eq!(ctx.host.as_str(), "git.example.org");
    }

    #[test]
    fn fjo_resolved_host_owner_name_picks_the_host() {
        let git = FakeGit::repo()
            .with_remote("origin", "git@work-forge:whatever/thing.git")
            .with_config(&resolved_key("origin"), "codeberg.org/them/proj");
        let hosts = hosts_with(&["git.example.org", "codeberg.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.host.as_str(), "codeberg.org");
        assert_eq!(ctx.slug.to_string(), "them/proj");
    }

    #[test]
    fn fjo_resolved_none_stops_instead_of_prompting_forever() {
        let git = FakeGit::repo()
            .with_remote("origin", "https://git.example.org/me/proj.git")
            .with_config(&resolved_key("origin"), RESOLVED_NONE);
        let hosts = hosts_with(&["git.example.org"]);
        let err = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap_err();
        match &*err.kind {
            ErrorKind::RepoNotResolved { tried } => {
                assert!(
                    tried.iter().any(|a| a.outcome.contains("NONE")),
                    "the attempts must say the user opted out: {tried:?}"
                );
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn not_a_git_repo_reports_what_it_tried() {
        let hosts = hosts_with(&["git.example.org"]);
        let err =
            resolve(&FakeGit::not_a_repo(), &hosts, &MapEnv::new(), ResolveOptions::default())
                .unwrap_err();
        match &*err.kind {
            ErrorKind::RepoNotResolved { tried } => {
                let what: Vec<&str> = tried.iter().map(|a| a.what).collect();
                assert_eq!(what, ["-R/--repo", "$FJO_REPO", "$FORGEJO_REPO", "git work tree"]);
            }
            other => panic!("wrong kind: {other:?}"),
        }
        // ...while a command that truly needs a work tree gets the specific variant.
        assert!(matches!(
            *require_git_repo(&FakeGit::not_a_repo()).unwrap_err().kind,
            ErrorKind::NotAGitRepo
        ));
    }

    #[test]
    fn unconfigured_host_names_the_remote_and_the_host() {
        let git = FakeGit::repo().with_remote("origin", "https://other.example/me/proj.git");
        let hosts = hosts_with(&["git.example.org"]);
        let err = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap_err();
        match &*err.kind {
            ErrorKind::RemoteHostUnknown { remote, host } => {
                assert_eq!(remote, "origin");
                assert_eq!(host, "other.example");
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn unparseable_remotes_are_skipped_not_fatal() {
        // A local mirror alongside a real remote must not break resolution.
        let git = FakeGit::repo()
            .with_remote("backup", "/srv/mirrors/proj.git")
            .with_remote("origin", "https://git.example.org/me/proj.git");
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "me/proj");
    }

    #[test]
    fn push_url_is_tried_when_the_fetch_url_is_not_a_forge() {
        let git = FakeGit::repo().with_split_remote(
            "origin",
            "/srv/cache/proj.git",
            "https://git.example.org/me/proj.git",
        );
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "me/proj");
    }

    #[test]
    fn fetch_url_wins_over_push_url() {
        // Triangular workflow: fetch from upstream, push to a fork. The command is about
        // what we fetch.
        let git = FakeGit::repo().with_split_remote(
            "origin",
            "https://git.example.org/them/proj.git",
            "https://git.example.org/me/fork.git",
        );
        let hosts = hosts_with(&["git.example.org"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "them/proj");
    }

    #[test]
    fn repo_flag_needs_no_git_repo() {
        let hosts = hosts_with(&["git.example.org"]);
        let r: RepoRef = "them/proj".parse().unwrap();
        let ctx = resolve(
            &FakeGit::not_a_repo(),
            &hosts,
            &MapEnv::new(),
            ResolveOptions { repo: Some(&r), ..Default::default() },
        )
        .unwrap();
        assert_eq!(ctx.slug.to_string(), "them/proj");
        assert_eq!(ctx.source, RepoSource::Flag);
    }

    #[test]
    fn repo_flag_host_conflicting_with_host_flag_is_a_usage_error() {
        let hosts = hosts_with(&["git.example.org", "codeberg.org"]);
        let r: RepoRef = "codeberg.org/them/proj".parse().unwrap();
        let err = resolve(
            &FakeGit::not_a_repo(),
            &hosts,
            &MapEnv::new(),
            ResolveOptions { repo: Some(&r), host: Some("git.example.org"), ..Default::default() },
        )
        .unwrap_err();
        match &*err.kind {
            // Both values must appear, or the user cannot tell which to change.
            ErrorKind::Usage(m) => {
                assert!(m.contains("codeberg.org"), "{m}");
                assert!(m.contains("git.example.org"), "{m}");
            }
            other => panic!("wrong kind: {other:?}"),
        }
        // The same conflict via the environment is caught too, and names the variable.
        let env = MapEnv::new().with("FORGEJO_HOST", "git.example.org");
        let err = resolve(
            &FakeGit::not_a_repo(),
            &hosts,
            &env,
            ResolveOptions { repo: Some(&r), ..Default::default() },
        )
        .unwrap_err();
        match &*err.kind {
            ErrorKind::Usage(m) => assert!(m.contains("FORGEJO_HOST"), "{m}"),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn env_repo_precedence_and_shape() {
        let hosts = hosts_with(&["git.example.org", "codeberg.org"]);
        let git = FakeGit::repo().with_remote("origin", "https://git.example.org/me/proj.git");

        // FJO_REPO beats FORGEJO_REPO...
        let env = MapEnv::new().with("FJO_REPO", "a/one").with("FORGEJO_REPO", "b/two");
        let ctx = resolve(&git, &hosts, &env, ResolveOptions::default()).unwrap();
        assert_eq!(ctx.slug.to_string(), "a/one");
        assert_eq!(ctx.source, RepoSource::Env { var: "FJO_REPO" });

        // ...and both beat the git remote.
        let env = MapEnv::new().with("FORGEJO_REPO", "codeberg.org/b/two");
        let ctx = resolve(&git, &hosts, &env, ResolveOptions::default()).unwrap();
        assert_eq!(ctx.host.as_str(), "codeberg.org");
        assert_eq!(ctx.slug.to_string(), "b/two");

        // A malformed value is a usage error naming the variable, not a silent fallthrough.
        let env = MapEnv::new().with("FJO_REPO", "justaname");
        match &*resolve(&git, &hosts, &env, ResolveOptions::default()).unwrap_err().kind {
            ErrorKind::Usage(m) => assert!(m.contains("FJO_REPO"), "{m}"),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn subpath_install_resolves_through_a_remote() {
        let git = FakeGit::repo().with_remote("origin", "https://example.org/forgejo/me/proj.git");
        let hosts = hosts_with(&["https://example.org/forgejo"]);
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.host.as_str(), "example.org/forgejo");
        assert_eq!(ctx.slug.to_string(), "me/proj");
    }

    #[test]
    fn explicit_login_that_does_not_exist_is_a_usage_error() {
        let git = FakeGit::repo().with_remote("origin", "https://git.example.org/me/proj.git");
        let hosts = hosts_with(&["git.example.org"]);
        let err = resolve(
            &git,
            &hosts,
            &MapEnv::new(),
            ResolveOptions { login: Some("someone-else"), ..Default::default() },
        )
        .unwrap_err();
        assert!(matches!(*err.kind, ErrorKind::Usage(_)));
    }

    #[test]
    fn a_host_with_no_login_still_resolves() {
        // Repository resolution must not fail on an auth problem: the HTTP layer's
        // NotAuthenticated message is far more useful than one from here.
        let mut hosts = Hosts::empty_at(std::path::Path::new("/nonexistent/hosts.toml"));
        hosts.add_host("git.example.org").unwrap();
        let git = FakeGit::repo().with_remote("origin", "https://git.example.org/me/proj.git");
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.login, None);
    }

    // ---------------------------------------------------------------------------------
    // Host resolution must follow the remote, not `hosts.toml`'s `active`.
    //
    // The bug these cover, in one sentence: a clone of `code.example/them/proj` with
    // `active = "other.example"` resolved the *slug* from the remote and the *host* from
    // `active`, and sent the request to a server nobody named. A 404 is the lucky outcome;
    // when the other server happens to hold a repository of the same name, the answer comes
    // back green from the wrong instance.
    // ---------------------------------------------------------------------------------

    /// `active` is the *last* resort, so a repository flag with no host must take the host from
    /// the checkout it was typed in.
    #[test]
    fn the_repo_flag_without_a_host_uses_the_remotes_host_not_active() {
        let mut hosts = hosts_with(&["code.example", "other.example"]);
        hosts.set_active(&HostKey::parse("other.example").unwrap()).unwrap();
        let git = FakeGit::repo().with_remote("origin", "https://code.example/me/proj.git");

        let r: RepoRef = "them/sibling".parse().unwrap();
        let ctx = resolve(
            &git,
            &hosts,
            &MapEnv::new(),
            ResolveOptions { repo: Some(&r), ..Default::default() },
        )
        .unwrap();

        assert_eq!(ctx.host.as_str(), "code.example", "-R must not fall through to `active`");
        assert_eq!(ctx.slug.to_string(), "them/sibling");
    }

    /// The same for `$FJO_REPO`: the variable names a repository, not a server.
    #[test]
    fn an_env_repo_without_a_host_uses_the_remotes_host_not_active() {
        let mut hosts = hosts_with(&["code.example", "other.example"]);
        hosts.set_active(&HostKey::parse("other.example").unwrap()).unwrap();
        let git = FakeGit::repo().with_remote("origin", "https://code.example/me/proj.git");
        let env = MapEnv::new().with("FJO_REPO", "them/sibling");

        let ctx = resolve(&git, &hosts, &env, ResolveOptions::default()).unwrap();
        assert_eq!(ctx.host.as_str(), "code.example");
    }

    /// …and an explicit host still wins, or `--host` would have become unusable.
    #[test]
    fn an_explicit_host_still_outranks_the_remote() {
        let hosts = hosts_with(&["code.example", "other.example"]);
        let git = FakeGit::repo().with_remote("origin", "https://code.example/me/proj.git");
        let r: RepoRef = "them/sibling".parse().unwrap();

        for (opts, env, why) in [
            (
                ResolveOptions { repo: Some(&r), host: Some("other.example"), login: None },
                MapEnv::new(),
                "--host",
            ),
            (
                ResolveOptions { repo: Some(&r), ..Default::default() },
                MapEnv::new().with("FJO_HOST", "other.example"),
                "$FJO_HOST",
            ),
            (
                ResolveOptions { repo: Some(&r), ..Default::default() },
                MapEnv::new().with("FORGEJO_HOST", "other.example"),
                "$FORGEJO_HOST",
            ),
        ] {
            let ctx = resolve(&git, &hosts, &env, opts).unwrap();
            assert_eq!(ctx.host.as_str(), "other.example", "{why} must outrank the remote");
        }
    }

    /// Bug this prevents: `--host` moving the client while [`RepoContext::host`] kept saying what
    /// the remote said, so the two disagreed about one command. The field is what callers trust to
    /// answer "which server is this about?", and a field that is right only sometimes is worse
    /// than no field at all.
    #[test]
    fn an_explicit_host_also_moves_the_host_the_remote_would_have_chosen() {
        let hosts = hosts_with(&["code.example", "other.example"]);
        // No -R at all: the repository comes from the remote, so only `explicit_or` can be
        // keeping the host and the client in agreement here.
        let git = FakeGit::repo().with_remote("origin", "https://code.example/them/proj.git");

        let ctx = resolve(
            &git,
            &hosts,
            &MapEnv::new(),
            ResolveOptions { host: Some("other.example"), ..Default::default() },
        )
        .unwrap();
        assert_eq!(ctx.host.as_str(), "other.example", "--host must move RepoContext::host too");
        assert_eq!(ctx.slug.to_string(), "them/proj");

        // The same through `fjo repo set-default`'s `base` override, which reads the URL itself.
        let git = FakeGit::repo()
            .with_remote("origin", "https://code.example/them/proj.git")
            .with_config(&resolved_key("origin"), RESOLVED_BASE);
        let ctx = resolve(
            &git,
            &hosts,
            &MapEnv::new(),
            ResolveOptions { host: Some("other.example"), ..Default::default() },
        )
        .unwrap();
        assert_eq!(ctx.host.as_str(), "other.example");
        assert_eq!(ctx.slug.to_string(), "them/proj");
    }

    /// A remote this configuration knows nothing about must not become an error on a path that
    /// never asked about the remote: `fjo issue list -R a/b` inside a GitHub clone still works,
    /// and `active` is the right answer there.
    #[test]
    fn a_remote_on_an_unconfigured_host_falls_back_to_active() {
        let mut hosts = hosts_with(&["code.example", "other.example"]);
        hosts.set_active(&HostKey::parse("other.example").unwrap()).unwrap();
        let git = FakeGit::repo().with_remote("origin", "https://github.com/me/proj.git");

        let r: RepoRef = "them/sibling".parse().unwrap();
        let ctx = resolve(
            &git,
            &hosts,
            &MapEnv::new(),
            ResolveOptions { repo: Some(&r), ..Default::default() },
        )
        .unwrap();
        assert_eq!(ctx.host.as_str(), "other.example");
    }

    /// Outside a work tree there is no remote to read, so `active` stands.
    #[test]
    fn outside_a_work_tree_the_repo_flag_uses_active() {
        let mut hosts = hosts_with(&["code.example", "other.example"]);
        hosts.set_active(&HostKey::parse("other.example").unwrap()).unwrap();
        let r: RepoRef = "them/sibling".parse().unwrap();
        let ctx = resolve(
            &FakeGit::not_a_repo(),
            &hosts,
            &MapEnv::new(),
            ResolveOptions { repo: Some(&r), ..Default::default() },
        )
        .unwrap();
        assert_eq!(ctx.host.as_str(), "other.example");
    }

    /// `host_from_remotes` must read the tree the same way [`resolve_repo`] does, or the host
    /// and the repository could still be decided by two different remotes. `upstream` outranks
    /// `origin` in both.
    #[test]
    fn host_from_remotes_walks_the_same_order_as_repo_resolution() {
        let hosts = hosts_with(&["code.example", "other.example"]);
        let git = FakeGit::repo()
            .with_remote("origin", "https://other.example/me/fork.git")
            .with_remote("upstream", "https://code.example/them/proj.git");

        assert_eq!(
            host_from_remotes(&hosts, &git).unwrap().map(|h| h.to_string()),
            Some("code.example".to_owned())
        );
        // …and the full resolution agrees, which is the property that matters.
        let ctx = resolve(&git, &hosts, &MapEnv::new(), ResolveOptions::default()).unwrap();
        assert_eq!(ctx.host.as_str(), "code.example");
    }

    /// `fjo repo set-default` is the documented override, so it has to move the host too.
    #[test]
    fn host_from_remotes_honours_fjo_resolved() {
        let hosts = hosts_with(&["code.example", "other.example"]);
        // `base` points at the remote whose URL is authoritative, beating `upstream`'s score.
        let git = FakeGit::repo()
            .with_remote("origin", "https://other.example/me/fork.git")
            .with_remote("upstream", "https://code.example/them/proj.git")
            .with_config(&resolved_key("origin"), RESOLVED_BASE);
        assert_eq!(
            host_from_remotes(&hosts, &git).unwrap().map(|h| h.to_string()),
            Some("other.example".to_owned())
        );

        // An explicit `host/owner/name` names the host outright.
        let git = FakeGit::repo()
            .with_remote("origin", "git@work-forge:whatever/thing.git")
            .with_config(&resolved_key("origin"), "code.example/them/proj");
        assert_eq!(
            host_from_remotes(&hosts, &git).unwrap().map(|h| h.to_string()),
            Some("code.example".to_owned())
        );
    }

    /// No remotes, no work tree, and an unparseable remote all mean "no opinion" — never an
    /// error, because every caller has a fallback and a hard failure here would break
    /// commands that need no repository at all.
    #[test]
    fn host_from_remotes_has_no_opinion_rather_than_an_error() {
        let hosts = hosts_with(&["code.example"]);
        assert_eq!(host_from_remotes(&hosts, &FakeGit::not_a_repo()).unwrap(), None);
        assert_eq!(host_from_remotes(&hosts, &FakeGit::repo()).unwrap(), None);
        let git = FakeGit::repo().with_remote("backup", "/srv/mirrors/proj.git");
        assert_eq!(host_from_remotes(&hosts, &git).unwrap(), None);
        let git = FakeGit::repo().with_remote("origin", "https://github.com/me/proj.git");
        assert_eq!(host_from_remotes(&hosts, &git).unwrap(), None);
    }
}

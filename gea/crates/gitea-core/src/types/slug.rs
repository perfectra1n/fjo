//! `owner/name` repository slugs, and the optional `host/` prefix accepted by `-R`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A repository reference: `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoSlug {
    pub owner: String,
    pub name: String,
}

impl RepoSlug {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self { owner: owner.into(), name: name.into() }
    }
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSlugError {
    pub input: String,
    pub reason: &'static str,
}

impl fmt::Display for ParseSlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a repository: {}", self.input, self.reason)
    }
}

impl std::error::Error for ParseSlugError {}

impl FromStr for RepoSlug {
    type Err = ParseSlugError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim().trim_end_matches('/');
        let err = |reason| ParseSlugError { input: s.to_owned(), reason };
        let (owner, name) = t.split_once('/').ok_or_else(|| err("expected owner/name"))?;
        if owner.is_empty() || name.is_empty() {
            return Err(err("owner and name must both be present"));
        }
        if name.contains('/') {
            return Err(err("expected exactly one '/'"));
        }
        Ok(Self::new(owner, name.trim_end_matches(".git")))
    }
}

/// What `-R/--repo` accepts: `owner/name`, `host/owner/name`, or a full URL.
///
/// Keeping the host optional here — rather than demanding it — is what lets `-R` work both
/// inside a clone (host comes from context) and outside one (host comes from config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub host: Option<String>,
    pub slug: RepoSlug,
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(h) => write!(f, "{h}/{}", self.slug),
            None => fmt::Display::fmt(&self.slug, f),
        }
    }
}

impl FromStr for RepoRef {
    type Err = ParseSlugError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let err = |reason| ParseSlugError { input: s.to_owned(), reason };

        // A full URL: strip the scheme and any credentials, then treat the rest as
        // host/path. Credentials are dropped and never retained, so they cannot leak into
        // a log line or an error message.
        let rest = if let Some((_scheme, r)) = t.split_once("://") {
            r.split_once('@').map_or(r, |(_creds, after)| after)
        } else {
            t
        };

        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        match parts.as_slice() {
            [owner, name] => {
                Ok(Self { host: None, slug: RepoSlug::new(*owner, name.trim_end_matches(".git")) })
            }
            // 3+ segments: the first is the host, the last two are owner/name. Anything
            // between is a subpath install (Gitea's ROOT_URL may carry a path prefix)
            // and is not part of the slug.
            [host, .., owner, name] if !host.is_empty() => Ok(Self {
                host: Some((*host).to_owned()),
                slug: RepoSlug::new(*owner, name.trim_end_matches(".git")),
            }),
            _ => Err(err("expected owner/name or host/owner/name")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_slug() {
        let r: RepoRef = "perf3ct/gea".parse().unwrap();
        assert_eq!(r.host, None);
        assert_eq!(r.slug.to_string(), "perf3ct/gea");
    }

    #[test]
    fn host_prefixed() {
        let r: RepoRef = "git.example.org/perf3ct/gea".parse().unwrap();
        assert_eq!(r.host.as_deref(), Some("git.example.org"));
        assert_eq!(r.slug.to_string(), "perf3ct/gea");
    }

    #[test]
    fn full_url_with_git_suffix() {
        let r: RepoRef = "https://git.example.org/perf3ct/gea.git".parse().unwrap();
        assert_eq!(r.host.as_deref(), Some("git.example.org"));
        assert_eq!(r.slug.name, "gea");
    }

    #[test]
    fn subpath_install() {
        // Gitea may be mounted under a path prefix; the prefix is not part of the slug.
        let r: RepoRef = "https://example.org/gitea/perf3ct/gea".parse().unwrap();
        assert_eq!(r.host.as_deref(), Some("example.org"));
        assert_eq!(r.slug.to_string(), "perf3ct/gea");
    }

    #[test]
    fn credentials_are_dropped() {
        let r: RepoRef = "https://user:secret@git.example.org/o/r".parse().unwrap();
        assert_eq!(r.host.as_deref(), Some("git.example.org"));
        assert!(!r.to_string().contains("secret"));
    }

    #[test]
    fn rejects_bare_name() {
        assert!("gea".parse::<RepoRef>().is_err());
        assert!("perf3ct/".parse::<RepoSlug>().is_err());
    }
}

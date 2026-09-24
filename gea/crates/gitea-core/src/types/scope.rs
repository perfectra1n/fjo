//! Gitea token scopes.
//!
//! Gitea scopes are `read:<area>` / `write:<area>` over route groups — *not* GitHub's
//! `repo` / `read:org` vocabulary. Error messages must speak Gitea's language, because a
//! user following a GitHub guide will otherwise be told to create a scope that does not exist.
//!
//! Parsing never rejects: new areas will appear in future Gitea releases, and a scope we
//! do not recognise is still worth displaying verbatim.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A token scope, e.g. `write:repository`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(String);

/// The known route-group areas, as of Gitea 16. Used for suggestions and completion, never
/// for validation.
pub const KNOWN_AREAS: &[&str] = &[
    "activitypub",
    "admin",
    "issue",
    "misc",
    "notification",
    "organization",
    "package",
    "repository",
    "user",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    pub const fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
        }
    }

    /// Which access level an HTTP method implies. `GET` and `HEAD` need read; everything
    /// else needs write.
    pub fn for_method(method: &str) -> Self {
        match method.to_ascii_uppercase().as_str() {
            "GET" | "HEAD" | "OPTIONS" => Access::Read,
            _ => Access::Write,
        }
    }
}

impl Scope {
    pub fn new(access: Access, area: &str) -> Self {
        Self(format!("{}:{}", access.as_str(), area))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits into `(access, area)`, or `None` for a scope that is not in `access:area`
    /// form. Gitea has historically also issued bare scopes like `all`.
    pub fn parts(&self) -> Option<(Access, &str)> {
        let (a, area) = self.0.split_once(':')?;
        let access = match a {
            "read" => Access::Read,
            "write" => Access::Write,
            _ => return None,
        };
        Some((access, area))
    }

    /// Whether holding `self` satisfies a requirement for `needed`.
    ///
    /// `write:` implies `read:` on the same area, and the bare scope `all` satisfies
    /// everything.
    pub fn satisfies(&self, needed: &Scope) -> bool {
        if self.0 == "all" || self.0 == needed.0 {
            return true;
        }
        match (self.parts(), needed.parts()) {
            (Some((Access::Write, have)), Some((Access::Read, want))) => have == want,
            _ => false,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Scope {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.trim().to_owned()))
    }
}

impl From<&str> for Scope {
    fn from(s: &str) -> Self {
        Self(s.trim().to_owned())
    }
}

/// Renders a scope list the way error messages want it: `read:repository, write:issue`.
pub fn join(scopes: &[Scope]) -> String {
    scopes.iter().map(Scope::as_str).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_implies_read_on_same_area() {
        let have: Scope = "write:repository".into();
        assert!(have.satisfies(&"read:repository".into()));
        assert!(have.satisfies(&"write:repository".into()));
        // ...but not on a different area.
        assert!(!have.satisfies(&"read:issue".into()));
    }

    #[test]
    fn read_does_not_imply_write() {
        let have: Scope = "read:issue".into();
        assert!(!have.satisfies(&"write:issue".into()));
    }

    #[test]
    fn all_satisfies_everything() {
        let have: Scope = "all".into();
        assert!(have.satisfies(&"write:admin".into()));
    }

    #[test]
    fn unknown_scopes_parse_and_display_verbatim() {
        // A future Gitea may invent an area we have never heard of.
        let s: Scope = "write:quantum".parse().unwrap();
        assert_eq!(s.to_string(), "write:quantum");
        assert_eq!(s.parts(), Some((Access::Write, "quantum")));
    }

    #[test]
    fn method_to_access() {
        assert_eq!(Access::for_method("GET"), Access::Read);
        assert_eq!(Access::for_method("post"), Access::Write);
        assert_eq!(Access::for_method("DELETE"), Access::Write);
    }
}

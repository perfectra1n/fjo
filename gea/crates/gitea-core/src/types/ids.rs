//! Newtyped identifiers.
//!
//! These exist to prevent one specific, expensive bug. Gitea objects carry *both* a global
//! database `id` and a per-repository `index`/`number`, and the API is inconsistent about
//! which one a path parameter wants:
//!
//! ```text
//! GET    /repos/{owner}/{repo}/issues/{index}            <- the number users see, e.g. #42
//! DELETE /repos/{owner}/{repo}/issues/comments/{id}      <- a global row id, e.g. 918273
//! ```
//!
//! Passing the wrong one either 404s or — far worse — silently operates on a *different,
//! real* issue. That is the most common class of Gitea/Gitea API bug, and here it is a
//! compile error.
//!
//! Note the deliberate absence of `Deref` and of `From<Id> for i64`: the friction is the
//! point. Use `.get()` when you genuinely need the integer.

use std::fmt;
use std::str::FromStr;

/// Failed to parse an identifier from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIdError {
    pub input: String,
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a number", self.input)
    }
}

impl std::error::Error for ParseIdError {}

macro_rules! int_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default,
            serde::Serialize, serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub const fn new(v: i64) -> Self {
                Self(v)
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl From<i64> for $name {
            fn from(v: i64) -> Self {
                Self(v)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            /// Accepts a leading `#`, so `gea issue view '#42'` works as users expect.
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let t = s.trim().trim_start_matches('#');
                t.parse::<i64>()
                    .map(Self)
                    .map_err(|_| ParseIdError { input: s.to_owned() })
            }
        }
    };
}

int_id!(
    /// Global database id of an issue. **Not** the number users see — that is [`IssueIndex`].
    IssueId
);
int_id!(
    /// Per-repository issue or pull-request number. This is what `#42` means, and what
    /// `/issues/{index}` and `/pulls/{index}` want.
    IssueIndex
);
int_id!(
    /// Global database id of a pull request. Differs from the pull request's [`IssueIndex`].
    PullRequestId
);
int_id!(/// Global database id of a repository.
    RepoId);
int_id!(/// Global database id of a user.
    UserId);
int_id!(/// Global database id of an organization.
    OrgId);
int_id!(/// Global database id of a team.
    TeamId);
int_id!(/// Global database id of a comment.
    CommentId);
int_id!(/// Global database id of a label.
    LabelId);
int_id!(/// Global database id of a milestone.
    MilestoneId);
int_id!(/// Global database id of a release.
    ReleaseId);
int_id!(/// Global database id of an attachment.
    AttachmentId);
int_id!(/// Global database id of an Actions run.
    RunId);
int_id!(/// Global database id of an Actions job.
    JobId);
int_id!(/// Global database id of an Actions artifact.
    ArtifactId);
int_id!(/// Global database id of a webhook.
    HookId);
int_id!(/// Global database id of an SSH or deploy key.
    KeyId);
int_id!(/// Global database id of a tracked-time entry.
    TrackedTimeId);
int_id!(/// Global database id of a pull-request review.
    ReviewId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_hashed() {
        assert_eq!("42".parse::<IssueIndex>().unwrap(), IssueIndex::new(42));
        assert_eq!("#42".parse::<IssueIndex>().unwrap(), IssueIndex::new(42));
        assert_eq!(" #42 ".parse::<IssueIndex>().unwrap(), IssueIndex::new(42));
    }

    #[test]
    fn rejects_nonsense() {
        assert!("abc".parse::<IssueIndex>().is_err());
        assert!("".parse::<IssueIndex>().is_err());
    }

    #[test]
    fn serializes_transparently() {
        // The wire format must be a bare integer, not {"0": 42}.
        assert_eq!(serde_json::to_string(&RepoId::new(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<RepoId>("7").unwrap(), RepoId::new(7));
    }
}

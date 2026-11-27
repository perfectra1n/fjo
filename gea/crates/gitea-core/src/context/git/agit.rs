//! AGit: a pull request with no branch and no fork.
//!
//! Every forge in the GitHub lineage needs a *branch* to open a pull request from, and — if
//! you cannot write to the repository — a *fork* to put that branch in. Gitea also accepts
//! pull requests over **AGit**, which needs neither. You push to a magic ref:
//!
//! ```text
//! git push origin HEAD:refs/for/main/my-topic
//! ```
//!
//! and the server creates a pull request against `main` from those commits. Nothing is added
//! to the repository's branch namespace, no fork exists, and a contributor with no write
//! access can do it against a repository that allows it. **`git push` is the entire
//! protocol**, which is why this is structural rather than a feature `gh` merely lacks: there
//! is no GitHub API call that would do it, and no amount of REST on our side substitutes for
//! the push. It is also why it lives here, next to the other git writes, rather than in a
//! client module.
//!
//! Three rules the implementation has to respect, because Gitea enforces them:
//!
//! 1. **A topic is mandatory.** `refs/for/<base>` with no topic is refused, so
//!    [`AgitRef::new`] refuses it first, with a message that says why.
//! 2. **Updating means pushing the same topic again.** A different topic opens a *second*
//!    pull request, which is why a caller's default topic has to be something stable — a
//!    branch name, not a timestamp.
//! 3. **An amended or rebased history needs a force push** (`gea pr create --agit
//!    --force-push`), since the update must otherwise be a fast-forward. That is the same rule
//!    as any other push, arriving in a place people do not expect it, so
//!    [`AgitRemedy::ForcePush`](crate::error::AgitRemedy::ForcePush) names it when git's own
//!    words match.
//!
//! A refused push therefore produces [`ErrorKind::AgitRefused`], carrying the refspec, the
//! remedy [`AgitRemedy::from_stderr`](crate::error::AgitRemedy::from_stderr) classified, and
//! git's own words. This module deliberately owns **no** advice text of its own: a second
//! renderer building `"{advice}\n\ngit said:\n{said}"` next to the three-part shape
//! `error::render` owns is how the two silently stop agreeing.
//!
//! Reference: <https://about.gitea.com/docs/latest/user/agit-support/>.

use crate::error::{Error, ErrorKind, Result};

use super::spec::PushSpec;

/// The ref an AGit push targets: `refs/for/<base>/<topic>`.
///
/// A type rather than a formatted string, because the topic is load-bearing — it is the
/// identity of the pull request across pushes — and a `String` that is sometimes a topic and
/// sometimes an empty one is exactly how "a second pull request appeared" happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgitRef {
    base: String,
    topic: String,
}

impl AgitRef {
    /// Validate a base branch and topic.
    ///
    /// Both are trimmed, and both must be non-empty: Gitea refuses `refs/for/<base>` with
    /// no topic, and the error it returns does not explain itself, so refusing locally with a
    /// message that does is strictly better than a round trip.
    pub fn new(base: impl Into<String>, topic: impl Into<String>) -> Result<Self> {
        let base = base.into().trim().to_owned();
        let topic = topic.into().trim().to_owned();
        if base.is_empty() {
            return Err(Error::new(ErrorKind::Usage(
                "an AGit pull request needs a base branch to open against".to_owned(),
            )));
        }
        if topic.is_empty() {
            return Err(Error::new(ErrorKind::Usage(
                "an AGit topic cannot be empty: Gitea refuses refs/for/<base> with no topic. \
                 Use the same topic again to update the same pull request"
                    .to_owned(),
            )));
        }
        Ok(Self { base, topic })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// `refs/for/<base>/<topic>`.
    pub fn refname(&self) -> String {
        format!("refs/for/{}/{}", self.base, self.topic)
    }

    /// `<src>:refs/for/<base>/<topic>`.
    pub fn refspec(&self, src: &str) -> String {
        format!("{src}:{}", self.refname())
    }
}

/// An AGit push, fully described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgitPush {
    pub remote: String,
    pub target: AgitRef,
    /// What to push. `HEAD` unless a caller has a reason.
    pub src: String,
    pub title: String,
    pub body: String,
    /// Required when the history was amended or rebased — an AGit update is a push like any
    /// other, and a non-fast-forward is refused without it.
    pub force: bool,
}

impl AgitPush {
    pub fn new(remote: impl Into<String>, target: AgitRef) -> Self {
        Self {
            remote: remote.into(),
            target,
            src: "HEAD".to_owned(),
            title: String::new(),
            body: String::new(),
            force: false,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    #[must_use]
    pub fn forced(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    #[must_use]
    pub fn from_src(mut self, src: impl Into<String>) -> Self {
        self.src = src.into();
        self
    }

    pub fn refspec(&self) -> String {
        self.target.refspec(&self.src)
    }

    /// The push this becomes.
    ///
    /// The title and description travel as **push options**, because an AGit creation has no
    /// request body at all — the push is the whole API call. An empty title is omitted rather
    /// than sent as `title=`, which would set the pull request's title to the empty string
    /// instead of letting the server fall back to the commit subject.
    pub fn to_push_spec(&self) -> PushSpec {
        let mut spec = PushSpec::new(&self.remote).with_refspec(self.refspec()).forced(self.force);
        if !self.title.trim().is_empty() {
            spec = spec.with_option(format!("title={}", self.title));
        }
        if !self.body.trim().is_empty() {
            spec = spec.with_option(format!("description={}", self.body));
        }
        spec
    }
}

/// What an accepted AGit push told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgitOutcome {
    /// git's stderr, verbatim. Gitea's whole answer arrives here as `remote:` lines, and a
    /// caller should relay it: swallowing it is the `tea` bug in a new place.
    pub stderr: String,
    /// The pull request number, when the server printed a URL we could read.
    pub pull_index: Option<i64>,
}

/// The pull request number in a `remote: …/pulls/12` line.
///
/// The server tells us what it created, and reading it back is far more reliable than guessing
/// from the topic. The *last* match wins: Gitea prints a compare URL before the pull request
/// URL on some paths, and taking the first would fetch pull request 0.
pub fn pull_index(text: &str) -> Option<i64> {
    let mut found = None;
    for token in text.split_whitespace() {
        let cleaned = token.trim_end_matches(['.', ',', ')', '"', '\'']);
        for marker in ["/pulls/", "/pull/"] {
            if let Some((_, rest)) = cleaned.rsplit_once(marker)
                && let Ok(n) = rest.parse::<i64>()
            {
                found = Some(n);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 1. Gitea refuses `refs/for/<base>` with no topic, and its error does not say so.
    #[test]
    fn a_topic_is_mandatory_and_refused_locally() {
        let e = AgitRef::new("main", "   ").expect_err("an empty topic is refused");
        let message = e.to_string();
        assert!(message.contains("topic"), "{message}");
        assert!(message.contains("refs/for/<base>"), "{message}");
        assert!(AgitRef::new("", "t").is_err(), "a base is required too");
        // Surrounding whitespace is trimmed rather than being pushed into the ref name.
        assert_eq!(
            AgitRef::new(" main ", " fix-parser ").unwrap().refname(),
            "refs/for/main/fix-parser"
        );
    }

    /// Rule 2. The refspec is the whole protocol; getting it wrong opens a second pull request
    /// or pushes a branch called `for`.
    #[test]
    fn the_refspec_is_the_magic_ref() {
        let target = AgitRef::new("main", "fix-parser").unwrap();
        assert_eq!(target.refspec("HEAD"), "HEAD:refs/for/main/fix-parser");
        let push = AgitPush::new("origin", target);
        assert_eq!(push.refspec(), "HEAD:refs/for/main/fix-parser");
    }

    /// The title and description have nowhere else to go: an AGit creation has no request
    /// body, only the push.
    #[test]
    fn the_title_and_body_travel_as_push_options() {
        let push = AgitPush::new("origin", AgitRef::new("main", "t").unwrap())
            .with_title("Fix the parser")
            .with_body("Fixes #12");
        let argv: Vec<String> =
            push.to_push_spec().argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            argv,
            [
                "push",
                "origin",
                "HEAD:refs/for/main/t",
                "-o",
                "title=Fix the parser",
                "-o",
                "description=Fixes #12",
            ]
        );
    }

    /// A blank body must not become `-o description=`, which would blank out the description
    /// of a pull request being updated.
    #[test]
    fn a_blank_body_sends_no_description_option() {
        let push = AgitPush::new("origin", AgitRef::new("main", "t").unwrap())
            .with_title("x")
            .with_body("   \n ");
        let argv: Vec<String> =
            push.to_push_spec().argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!argv.iter().any(|a| a.starts_with("description=")), "{argv:?}");
    }

    /// Rule 3. `--force` goes before the remote, where git wants it.
    #[test]
    fn forcing_an_amended_history_produces_a_force_push() {
        let push = AgitPush::new("origin", AgitRef::new("main", "t").unwrap())
            .with_title("x")
            .forced(true);
        let argv: Vec<String> =
            push.to_push_spec().argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv[..3], ["push", "--force", "origin"]);
    }

    /// Bug this prevents: an AGit refusal reported as a bare "push failed", when the two
    /// refusals that actually happen each have a specific, actionable cause.
    ///
    /// This used to assert on a `refusal()` in this module that formatted its own advice string.
    /// That made it a second renderer competing with `error::render`, so it is gone and the test
    /// moved onto the classifier the push path actually uses. The *wording* of each remedy is
    /// asserted where it is produced, in `error::render`; what belongs here is that git's own
    /// words still reach the right one.
    #[test]
    fn a_refusal_names_the_cause_it_can_recognise() {
        use crate::error::AgitRemedy;

        assert_eq!(
            AgitRemedy::from_stderr(
                "! [remote rejected] HEAD -> refs/for/main/t (non-fast-forward)"
            ),
            AgitRemedy::ForcePush
        );
        assert_eq!(
            AgitRemedy::from_stderr("fatal: the receiving end does not support push options"),
            AgitRemedy::PushOptionsDisabled
        );
        assert_eq!(
            AgitRemedy::from_stderr("remote: Gitea: topic is required"),
            AgitRemedy::TopicRequired
        );
        // An unrecognised refusal stays unrecognised rather than being guessed at.
        assert_eq!(AgitRemedy::from_stderr("something nobody predicted"), AgitRemedy::Unrecognised);
    }

    /// Bug this prevents: reading the *compare* URL Gitea prints before the pull request
    /// URL, and then fetching pull request 0.
    #[test]
    fn the_pull_request_number_is_read_out_of_gits_output() {
        let output = "remote: \n\
             remote: Create a new pull request for 'main':\n\
             remote:   http://localhost:3000/me/proj/compare/main...me/topic\n\
             remote: \n\
             remote: Processed 1 references in total\n\
             remote: http://localhost:3000/me/proj/pulls/7\n";
        assert_eq!(pull_index(output), Some(7));
        assert_eq!(pull_index("remote: https://x/o/r/pull/3."), Some(3));
        assert_eq!(pull_index("nothing here"), None);
        assert_eq!(pull_index("remote: https://x/o/r/pulls/notanumber"), None);
    }
}

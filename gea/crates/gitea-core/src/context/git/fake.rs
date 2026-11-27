//! Canned git state for tests.
//!
//! Every test in this crate that touches git uses this: no temporary repositories, no
//! `git init`, no process spawns, so the suite runs in microseconds and cannot be affected by
//! the developer's global gitconfig — or by whether `git` is installed at all.
//!
//! The fake does **not** reimplement git. It answers the reads from canned state and records
//! the writes as the argv they would have produced. That is deliberate: the thing worth
//! testing about `push_agit` is that it produces
//! `push --force origin HEAD:refs/for/main/topic -o title=…`, and a fake that simulated a
//! receive-pack would be testing itself.
//!
//! Because the derived reads go through [`GitCtx::exec`] like everything else, the fake
//! recognises them **by their exact argv**. If a provided method's argv changes, the fake
//! stops recognising it and the test fails — which is the coupling we want, not an accident.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::Result;

use super::spec::{CommitMessage, GitIo, GitOutput};
use super::{GitCtx, Remote};

/// One recorded `git` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitAction {
    pub argv: Vec<String>,
    pub io: GitIo,
}

impl GitAction {
    /// The argv as one space-separated line, for a readable assertion.
    pub fn line(&self) -> String {
        self.argv.join(" ")
    }

    /// Whether this invocation starts with `prefix`.
    pub fn starts_with(&self, prefix: &[&str]) -> bool {
        self.argv.len() >= prefix.len() && self.argv.iter().zip(prefix).all(|(a, b)| a == b)
    }
}

/// A programmable [`GitCtx`].
#[derive(Debug, Default)]
pub struct FakeGit {
    git_dir: Option<PathBuf>,
    remotes: RefCell<Vec<Remote>>,
    config: RefCell<BTreeMap<String, String>>,
    branch: Option<String>,
    remote_heads: BTreeMap<String, String>,

    // ---- canned answers for the derived reads
    revs: BTreeSet<String>,
    ancestors: BTreeSet<(String, String)>,
    status: String,
    log: BTreeMap<String, Vec<CommitMessage>>,
    ff_only_succeeds: bool,

    // ---- what an unrecognised invocation returns
    responses: RefCell<VecDeque<GitOutput>>,

    actions: RefCell<Vec<GitAction>>,
}

impl FakeGit {
    /// A directory that is not a git repository.
    pub fn not_a_repo() -> Self {
        Self::default()
    }

    /// A repository with no remotes.
    pub fn repo() -> Self {
        Self {
            git_dir: Some(PathBuf::from(".git")),
            // A fast-forward works unless a test says otherwise; the interesting case is the
            // divergence, and it should have to be asked for.
            ff_only_succeeds: true,
            ..Self::default()
        }
    }

    /// Adds a remote whose fetch and push URLs are the same, which is the usual case.
    #[must_use]
    pub fn with_remote(self, name: &str, url: &str) -> Self {
        self.remotes.borrow_mut().push(Remote {
            name: name.to_owned(),
            fetch: Some(url.to_owned()),
            push: Some(url.to_owned()),
        });
        self
    }

    /// Adds a remote with distinct fetch and push URLs, as `pushInsteadOf` or an explicit
    /// `pushurl` produces.
    #[must_use]
    pub fn with_split_remote(self, name: &str, fetch: &str, push: &str) -> Self {
        self.remotes.borrow_mut().push(Remote {
            name: name.to_owned(),
            fetch: Some(fetch.to_owned()),
            push: Some(push.to_owned()),
        });
        self
    }

    #[must_use]
    pub fn with_config(self, key: &str, value: &str) -> Self {
        self.config.borrow_mut().insert(key.to_owned(), value.to_owned());
        self
    }

    #[must_use]
    pub fn with_branch(mut self, name: &str) -> Self {
        self.branch = Some(name.to_owned());
        self
    }

    #[must_use]
    pub fn with_remote_head(mut self, remote: &str, branch: &str) -> Self {
        self.remote_heads.insert(remote.to_owned(), branch.to_owned());
        self
    }

    /// A revision that resolves — `git rev-parse --verify` will succeed for it.
    #[must_use]
    pub fn with_rev(mut self, rev: &str) -> Self {
        self.revs.insert(rev.to_owned());
        self
    }

    /// `ancestor` is reachable from `descendant`, so a fast-forward between them is possible.
    #[must_use]
    pub fn with_ancestor(mut self, ancestor: &str, descendant: &str) -> Self {
        self.ancestors.insert((ancestor.to_owned(), descendant.to_owned()));
        self
    }

    /// What `git status --porcelain` says. Non-empty means a dirty work tree.
    #[must_use]
    pub fn with_status(mut self, porcelain: &str) -> Self {
        self.status = porcelain.to_owned();
        self
    }

    /// The commits in a revision range, oldest first.
    #[must_use]
    pub fn with_log(mut self, range: &str, commits: &[(&str, &str)]) -> Self {
        self.log.insert(
            range.to_owned(),
            commits
                .iter()
                .map(|(s, b)| CommitMessage { subject: (*s).to_owned(), body: (*b).to_owned() })
                .collect(),
        );
        self
    }

    /// Make `git merge --ff-only` refuse, as it does on a diverged branch.
    #[must_use]
    pub fn with_diverged_branch(mut self) -> Self {
        self.ff_only_succeeds = false;
        self
    }

    /// Queue what the next unrecognised invocation (a push, a clone, a checkout) returns.
    /// Queued in order; once the queue is empty, invocations succeed silently.
    #[must_use]
    pub fn with_response(self, out: GitOutput) -> Self {
        self.responses.borrow_mut().push_back(out);
        self
    }

    /// What `config_set_local` recorded, so a test can assert on what would have been
    /// written.
    pub fn config_snapshot(&self) -> BTreeMap<String, String> {
        self.config.borrow().clone()
    }

    /// Every `git` invocation, in order.
    pub fn actions(&self) -> Vec<GitAction> {
        self.actions.borrow().clone()
    }

    /// Every invocation rendered as a command line, for a readable assertion.
    pub fn command_lines(&self) -> Vec<String> {
        self.actions.borrow().iter().map(GitAction::line).collect()
    }

    /// The first invocation starting with `prefix`, if any.
    pub fn action_starting_with(&self, prefix: &[&str]) -> Option<GitAction> {
        self.actions.borrow().iter().find(|a| a.starts_with(prefix)).cloned()
    }
}

/// The full message git would print for `--pretty=format:%B`.
fn full_message(c: &CommitMessage) -> String {
    if c.body.is_empty() { c.subject.clone() } else { format!("{}\n\n{}", c.subject, c.body) }
}

impl GitCtx for FakeGit {
    fn exec(&self, argv: &[OsString], io: GitIo) -> Result<GitOutput> {
        let args: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        self.actions.borrow_mut().push(GitAction { argv: args.clone(), io });
        let as_str: Vec<&str> = args.iter().map(String::as_str).collect();

        // The derived reads, recognised by the exact argv their provided method builds.
        match as_str.as_slice() {
            ["rev-parse", "--verify", "--quiet", rev] => {
                return Ok(ok_or_no(self.revs.contains(*rev)));
            }
            ["merge-base", "--is-ancestor", a, b] => {
                let pair = ((*a).to_owned(), (*b).to_owned());
                return Ok(ok_or_no(self.ancestors.contains(&pair)));
            }
            ["status", "--porcelain"] => {
                return Ok(GitOutput::success().with_stdout(self.status.clone()));
            }
            ["merge", "--ff-only", _] => {
                return Ok(if self.ff_only_succeeds {
                    GitOutput::success()
                } else {
                    GitOutput::failure(1, "fatal: Not possible to fast-forward, aborting.")
                });
            }
            ["log", "--reverse", "--no-merges", pretty, range] => {
                let commits = self.log.get(*range).cloned().unwrap_or_default();
                let rendered = if *pretty == "--pretty=format:%s" {
                    commits.iter().map(|c| c.subject.clone()).collect::<Vec<_>>().join("\n")
                } else {
                    // `%B%x00`: each message followed by a NUL.
                    commits.iter().map(full_message).collect::<Vec<_>>().join("\0")
                };
                return Ok(GitOutput::success().with_stdout(rendered));
            }
            // Kept in sync with the fake's own remote list, so a test can add a remote and
            // then resolve against it.
            ["remote", "add", name, url] => {
                self.remotes.borrow_mut().push(Remote {
                    name: (*name).to_owned(),
                    fetch: Some((*url).to_owned()),
                    push: Some((*url).to_owned()),
                });
                return Ok(GitOutput::success());
            }
            ["remote", "rename", from, to] => {
                let mut remotes = self.remotes.borrow_mut();
                match remotes.iter_mut().find(|r| r.name == *from) {
                    Some(r) => r.name = (*to).to_owned(),
                    None => {
                        return Ok(GitOutput::failure(
                            128,
                            format!("error: No such remote: '{from}'"),
                        ));
                    }
                }
                return Ok(GitOutput::success());
            }
            _ => {}
        }

        Ok(self.responses.borrow_mut().pop_front().unwrap_or_else(GitOutput::success))
    }

    fn git_dir(&self) -> Result<Option<PathBuf>> {
        Ok(self.git_dir.clone())
    }

    fn remotes(&self) -> Result<Vec<Remote>> {
        Ok(self.remotes.borrow().clone())
    }

    fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.config.borrow().get(key).cloned())
    }

    fn config_get_regexp(&self, pattern: &str) -> Result<Vec<(String, String)>> {
        // A deliberately tiny subset of regex: `^prefix`, `suffix$`, and plain substrings.
        // That covers every pattern this crate uses, and a fake that implemented real regex
        // would be testing the regex engine rather than the caller.
        let anchored_start = pattern.strip_prefix('^');
        let pattern_body = anchored_start.unwrap_or(pattern);
        let (body, anchored_end) = match pattern_body.strip_suffix('$') {
            Some(b) => (b, true),
            None => (pattern_body, false),
        };
        let literal = body.replace("\\.", ".").replace(".*", "\u{0}");
        let (head, tail) = literal.split_once('\u{0}').unwrap_or((literal.as_str(), ""));

        Ok(self
            .config
            .borrow()
            .iter()
            .filter(|(k, _)| {
                let head_ok =
                    if anchored_start.is_some() { k.starts_with(head) } else { k.contains(head) };
                let tail_ok = if anchored_end { k.ends_with(tail) } else { k.contains(tail) };
                head_ok && tail_ok
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    fn config_set_local(&self, key: &str, value: &str) -> Result<()> {
        self.actions.borrow_mut().push(GitAction {
            argv: vec!["config".to_owned(), "--local".to_owned(), key.to_owned(), value.to_owned()],
            io: GitIo::Capture,
        });
        self.config.borrow_mut().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn config_unset_local(&self, key: &str) -> Result<()> {
        self.actions.borrow_mut().push(GitAction {
            argv: vec![
                "config".to_owned(),
                "--local".to_owned(),
                "--unset".to_owned(),
                key.to_owned(),
            ],
            io: GitIo::Capture,
        });
        // Idempotent, like the real thing: removing an absent key is success.
        self.config.borrow_mut().remove(key);
        Ok(())
    }

    fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.branch.clone())
    }

    fn remote_head(&self, remote: &str) -> Result<Option<String>> {
        Ok(self.remote_heads.get(remote).cloned())
    }
}

/// git's "no" — exit 1 with nothing on stderr — versus its "yes".
fn ok_or_no(yes: bool) -> GitOutput {
    if yes { GitOutput::success() } else { GitOutput::failure(1, String::new()) }
}

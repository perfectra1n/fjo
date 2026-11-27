//! Reading *and writing* the local git repository, by **shelling out to `git`**.
//!
//! ## Why not `git2`/libgit2
//!
//! libgit2 does not honour `url.<base>.insteadOf` / `pushInsteadOf` rewrites, and does not
//! process `includeIf` in gitconfig. Both are common in corporate setups — an `insteadOf`
//! rule that rewrites `https://forge.internal/` to `ssh://git@forge.internal:2222/`, or an
//! `includeIf "gitdir:~/work/"` block that sets a different identity per directory tree.
//!
//! With libgit2 we would read the *unrewritten* URL, disagree with what the user's real
//! `git` uses for the very same remote, and then report "not a configured host" for a
//! repository they can clone and push to. That bug is nearly impossible to diagnose from the
//! error message. Shelling out inherits the user's actual configuration for free, matches
//! what `gh` does, and drops a C dependency from the build.
//!
//! On the write side the argument is stronger still. A clone URL the API handed us is
//! rewritten exactly as the user's own `git clone` would rewrite it — on a corporate forge the
//! API's `https://` URL is frequently unusable and the `insteadOf` rule is the only thing that
//! makes it work at all. Credential helpers, SSH agents and `includeIf` identities all apply,
//! which means **this crate never sees or handles a git credential**: the strongest possible
//! statement about not leaking one.
//!
//! The cost — a process spawn per operation — is real but small. `GIT_OPTIONAL_LOCKS=0` is set
//! on every invocation so that a read of a repository does not take `index.lock` and race with
//! an editor's background `git status`. Nothing is ever run through a shell: arguments are
//! passed as an argv, so a branch called `; rm -rf /` is a branch name and not a command.
//!
//! ## Shape
//!
//! [`GitCtx`] has two kinds of method. A handful are **required**: the reads repository
//! resolution needs, the two config writes, and [`GitCtx::exec`], which runs an argv. Everything
//! else — clone, fetch, push, checkout, branch creation, the AGit push — is a **provided**
//! method written once in terms of `exec`. So there is exactly one implementation of "what
//! argv does an AGit push produce", the real [`GitCli`] and the test [`FakeGit`] cannot drift
//! apart in it, and a test can assert on the argv a fake recorded rather than on a real
//! repository's state.
//!
//! Errors carry **git's own stderr**, never a generic replacement. `git push` refusals
//! ("Updates were rejected because the tip of your current branch is behind") and Gitea's
//! AGit refusals both arrive there, and a wrapper that replaced them with "push failed" would
//! be the bug this project criticises `tea` for. Every message is run through `scrub` first,
//! because a remote URL can carry an embedded credential and an error is exactly the text that
//! gets pasted into a public bug report.

pub mod agit;
mod cli;
mod fake;
mod scrub;
mod spec;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::error::{AgitRemedy, Error, ErrorKind, Result};

pub use agit::{AgitOutcome, AgitPush, AgitRef};
pub use cli::GitCli;
pub use fake::{FakeGit, GitAction};
pub use spec::{Checkout, CloneSpec, CommitMessage, FetchSpec, GitIo, GitOutput, PushSpec};

/// One git remote. `fetch` and `push` URLs differ whenever `pushInsteadOf` or an explicit
/// `remote.<name>.pushurl` is configured, and the fetch URL is the one that names the forge
/// we read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch: Option<String>,
    pub push: Option<String>,
}

impl Remote {
    /// The URLs to try, fetch first.
    ///
    /// Fetch before push because a triangular workflow pushes to a fork and fetches from
    /// upstream; the fetch URL is the one that identifies the repository the command is
    /// *about*.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.fetch.as_deref().into_iter().chain(self.push.as_deref())
    }
}

/// Everything this crate does with the local git repository.
pub trait GitCtx {
    // ------------------------------------------------------------------------ the primitive

    /// Run `git` with `argv`.
    ///
    /// A non-zero exit is reported in [`GitOutput::ok`], **not** as an `Err`: several git
    /// commands use exit status to mean "no" rather than "failed" (`config --get` exits 1 for
    /// a missing key, `merge-base --is-ancestor` exits 1 for "no"). Only a failure to *start*
    /// git at all is an error, and the message then says so — "not a git repository" and "git
    /// is not installed" need completely different remedies.
    fn exec(&self, argv: &[OsString], io: GitIo) -> Result<GitOutput>;

    // ----------------------------------------------------------------------- required reads

    /// `git rev-parse --git-dir`. `Ok(None)` means "not inside a git work tree", which is a
    /// normal state (`gea api user` works anywhere), not an error.
    fn git_dir(&self) -> Result<Option<PathBuf>>;

    /// `git remote -v`, in the order git lists them (alphabetical).
    fn remotes(&self) -> Result<Vec<Remote>>;

    /// `git config --get <key>`.
    fn config_get(&self, key: &str) -> Result<Option<String>>;

    /// `git config --get-regexp <pattern>`, as `(key, value)` pairs.
    fn config_get_regexp(&self, pattern: &str) -> Result<Vec<(String, String)>>;

    /// `git config --local <key> <value>`, used by `gea repo set-default`.
    fn config_set_local(&self, key: &str, value: &str) -> Result<()>;

    /// `git config --local --unset <key>`. Idempotent: unsetting a key that was never set is
    /// success, so `gea repo set-default --unset` does not fail the second time it is run.
    fn config_unset_local(&self, key: &str) -> Result<()>;

    /// `git branch --show-current`. `None` on a detached HEAD.
    fn current_branch(&self) -> Result<Option<String>>;

    /// `git symbolic-ref refs/remotes/<remote>/HEAD`, reduced to the branch name.
    /// `None` when the ref is absent, which is the common case until someone runs
    /// `git remote set-head`.
    fn remote_head(&self, remote: &str) -> Result<Option<String>>;

    // ------------------------------------------------------------------- generic escape hatch

    /// Run `git`, capturing both streams, treating a non-zero exit as data rather than failure.
    fn try_run(&self, args: &[&str]) -> Result<GitOutput> {
        self.exec(&to_argv(args), GitIo::Capture)
    }

    /// Run `git` and fail if it did, with git's own (redacted) stderr in the message.
    fn run(&self, args: &[&str]) -> Result<GitOutput> {
        let out = self.try_run(args)?;
        if !out.ok {
            return Err(failed(args, &out));
        }
        Ok(out)
    }

    /// Whether `git` exited zero, treating a non-zero exit as a plain `false`.
    fn succeeds(&self, args: &[&str]) -> Result<bool> {
        Ok(self.try_run(args)?.ok)
    }

    /// The captured stdout of a successful `git`, trimmed.
    fn stdout_of(&self, args: &[&str]) -> Result<String> {
        Ok(self.run(args)?.stdout.trim_end().to_owned())
    }

    // ------------------------------------------------------------------------- derived reads

    /// Whether a revision resolves in this repository.
    ///
    /// `--quiet` as well as `--verify`, so a missing revision is an exit code and not a line
    /// of noise on the user's terminal.
    fn rev_exists(&self, rev: &str) -> Result<bool> {
        self.succeeds(&["rev-parse", "--verify", "--quiet", rev])
    }

    /// Whether `ancestor` is reachable from `descendant` — "can this fast-forward".
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        self.succeeds(&["merge-base", "--is-ancestor", ancestor, descendant])
    }

    /// `git status --porcelain`, trimmed. Empty means a clean work tree.
    fn porcelain_status(&self) -> Result<String> {
        Ok(self.run(&["status", "--porcelain"])?.stdout.trim().to_owned())
    }

    /// Whether the work tree has no uncommitted changes.
    fn is_clean(&self) -> Result<bool> {
        Ok(self.porcelain_status()?.is_empty())
    }

    /// Subject lines of `<range>`, oldest first — what `pr create --fill` reads.
    ///
    /// `%s` and not `%B`: the body of every commit concatenated is not a pull request
    /// description, and `--fill` exists to produce something a human would have typed.
    fn commit_subjects(&self, range: &str) -> Result<Vec<String>> {
        let out =
            self.stdout_of(&["log", "--reverse", "--no-merges", "--pretty=format:%s", range])?;
        Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
    }

    /// Subject and body of the *first* commit in `<range>` — `--fill-first`.
    fn first_commit_message(&self, range: &str) -> Result<Option<CommitMessage>> {
        let out =
            self.stdout_of(&["log", "--reverse", "--no-merges", "--pretty=format:%B%x00", range])?;
        let Some(first) = out.split('\0').next().map(str::trim) else { return Ok(None) };
        if first.is_empty() {
            return Ok(None);
        }
        let (subject, body) = first.split_once('\n').unwrap_or((first, ""));
        Ok(Some(CommitMessage { subject: subject.trim().to_owned(), body: body.trim().to_owned() }))
    }

    // ----------------------------------------------------------------------------- remotes

    fn remote_exists(&self, name: &str) -> Result<bool> {
        Ok(self.remotes()?.iter().any(|r| r.name == name))
    }

    fn remote_add(&self, name: &str, url: &str) -> Result<()> {
        self.run(&["remote", "add", name, url]).map(|_| ())
    }

    /// `git remote rename`, which also rewrites every `branch.*.remote` that referred to it.
    fn remote_rename(&self, from: &str, to: &str) -> Result<()> {
        self.run(&["remote", "rename", from, to]).map(|_| ())
    }

    // ------------------------------------------------------------------------------ writes

    /// `git clone`, with git's progress meter on the terminal. Returns where it landed.
    fn clone_repo(&self, spec: &CloneSpec) -> Result<PathBuf> {
        self.checked(&spec.argv(), GitIo::Inherit)?;
        Ok(spec.target_dir())
    }

    /// `git fetch`.
    fn fetch(&self, spec: &FetchSpec) -> Result<()> {
        let io = if spec.progress { GitIo::Inherit } else { GitIo::Capture };
        self.checked(&spec.argv(), io).map(|_| ())
    }

    /// `git push`, **without** failing on a refusal.
    ///
    /// The caller gets the [`GitOutput`] either way, because a push's stderr is the server's
    /// entire reply — the pull request URL on success, the reason on refusal — and a wrapper
    /// that turned a refusal into an `Err` before the caller could relay it would discard the
    /// only useful part. Use [`GitCtx::push_or_fail`] when there is nothing to read.
    fn push(&self, spec: &PushSpec) -> Result<GitOutput> {
        let io = if spec.progress { GitIo::Inherit } else { GitIo::Capture };
        self.exec(&spec.argv(), io)
    }

    /// `git push`, failing with git's own stderr if the server refused.
    fn push_or_fail(&self, spec: &PushSpec) -> Result<GitOutput> {
        let out = self.push(spec)?;
        if !out.ok {
            return Err(failed(&spec.argv(), &out));
        }
        Ok(out)
    }

    /// Open or update a pull request over AGit — no branch, no fork. See [`agit`].
    ///
    /// On refusal this returns [`ErrorKind::AgitRefused`], which carries the refspec, the
    /// classified remedy and git's own (scrubbed) words, since there is no successful return
    /// for a caller to relay them from. A caller that wants to print the server's reply *as it
    /// arrives* should use [`GitCtx::push`] with [`AgitPush::to_push_spec`] and call
    /// [`agit::pull_index`] itself; this method is the convenience over exactly that.
    ///
    /// The remedy is classified from the SCRUBBED stderr, not the raw text, so the advice and
    /// the words displayed underneath it can never disagree about what git said.
    fn push_agit(&self, push: &AgitPush) -> Result<AgitOutcome> {
        let out = self.push(&push.to_push_spec())?;
        if !out.ok {
            let stderr = scrub::text(out.detail()).into_owned();
            return Err(Error::new(ErrorKind::AgitRefused {
                refspec: push.refspec(),
                remedy: AgitRemedy::from_stderr(&stderr),
                stderr,
            }));
        }
        Ok(AgitOutcome { pull_index: agit::pull_index(&out.stderr), stderr: out.stderr })
    }

    /// `git checkout`.
    fn checkout(&self, what: &Checkout) -> Result<()> {
        self.checked(&what.argv(), GitIo::Capture).map(|_| ())
    }

    /// `git checkout`, reporting a refusal as `false` rather than as an error.
    ///
    /// For the best-effort cases — switching away from a branch that was just merged so it can
    /// be deleted — where failing the whole command for something that already succeeded on
    /// the server would be reporting the wrong outcome.
    fn try_checkout(&self, what: &Checkout) -> Result<bool> {
        Ok(self.exec(&what.argv(), GitIo::Capture)?.ok)
    }

    /// `git branch [-f] <name> [<start-point>]` — create a branch without switching to it.
    fn create_branch(&self, name: &str, start_point: Option<&str>, force: bool) -> Result<()> {
        let mut args: Vec<&str> = vec!["branch"];
        if force {
            args.push("--force");
        }
        args.push(name);
        if let Some(start) = start_point {
            args.push(start);
        }
        self.run(&args).map(|_| ())
    }

    /// `git branch -d/-D <name>`, reporting a refusal as `false`.
    ///
    /// git refuses `-d` for an unmerged branch, and that refusal is information rather than a
    /// failure — the caller decides whether losing the commits is acceptable.
    fn delete_branch(&self, name: &str, force: bool) -> Result<bool> {
        self.succeeds(&["branch", if force { "-D" } else { "-d" }, name])
    }

    /// `git merge --ff-only <target>`, reporting "it would not fast-forward" as `false`.
    fn merge_ff_only(&self, target: &str) -> Result<bool> {
        self.succeeds(&["merge", "--ff-only", target])
    }

    /// `git reset --hard <target>`. Destroys uncommitted work — check [`GitCtx::is_clean`]
    /// first, because nobody who passes a `--force` flag about *branch history* means "and
    /// also delete the file I am editing".
    fn reset_hard(&self, target: &str) -> Result<()> {
        self.run(&["reset", "--hard", target]).map(|_| ())
    }

    /// `git worktree add [--detach] <path> <rev>`.
    fn worktree_add(&self, path: &Path, rev: &str, detach: bool) -> Result<()> {
        let mut argv: Vec<OsString> = vec!["worktree".into(), "add".into()];
        if detach {
            argv.push("--detach".into());
        }
        argv.push(path.as_os_str().to_owned());
        argv.push(OsString::from(rev));
        self.checked(&argv, GitIo::Capture).map(|_| ())
    }

    /// `git submodule update --init --recursive`, with progress.
    ///
    /// `--init` as well as `--recursive`: a submodule added by the change being checked out
    /// has never been initialised here, and without it `git submodule update` silently skips
    /// it.
    fn update_submodules(&self) -> Result<()> {
        self.checked(&to_argv(&["submodule", "update", "--init", "--recursive"]), GitIo::Inherit)
            .map(|_| ())
    }

    /// [`GitCtx::exec`] plus "a non-zero exit is an error, carrying git's own words".
    ///
    /// Not part of the public surface anyone should call directly; it exists so the write
    /// methods above share one failure path.
    #[doc(hidden)]
    fn checked(&self, argv: &[OsString], io: GitIo) -> Result<GitOutput> {
        let out = self.exec(argv, io)?;
        if !out.ok {
            return Err(failed(argv, &out));
        }
        Ok(out)
    }
}

fn to_argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// A `git` failure, carrying git's own stderr.
///
/// [`ErrorKind::GitFailed`] exists precisely so that the subprocess's diagnosis survives: it
/// renders the command, the exit status, and every line git said, because the subprocess is
/// the only party that knows what went wrong. Replacing that with "push failed" would be the
/// bug this project criticises `tea` for, one layer down.
///
/// Both the argv and git's output go through [`scrub`]: a remote URL can carry an embedded
/// credential, and this string is precisely the one that ends up in a bug report.
pub(super) fn failed<S: AsRef<OsStr>>(args: &[S], out: &GitOutput) -> Error {
    Error::new(ErrorKind::GitFailed {
        command: format!("git {}", scrub::argv(args)),
        stderr: scrub::text(out.detail()).into_owned(),
        status: out.code,
    })
}

#[cfg(test)]
mod tests;

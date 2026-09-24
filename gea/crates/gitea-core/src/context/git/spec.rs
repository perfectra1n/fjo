//! The shapes a git invocation takes.
//!
//! Every write operation is described by a small owned struct rather than by a list of
//! arguments, for two reasons. A test can assert on the *description* — "this is a force push
//! of `HEAD` to `refs/for/main/parser` on `origin`" — instead of on a string of flags. And the
//! argv is built in exactly one place, so `--force` cannot be spelled `-f` in one command and
//! `--force` in another, and a flag added for one caller is available to all of them.

use std::ffi::OsString;
use std::path::PathBuf;

/// Where a git invocation's output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitIo {
    /// Capture both streams. Anything we parse or relay, which is most things — notably a
    /// push, because the server's entire answer arrives on git's stderr.
    Capture,
    /// Let git write straight to the terminal. `clone` and `fetch` print a progress meter,
    /// and capturing it turns a visible thirty-second download into an apparently hung
    /// command.
    Inherit,
}

/// What one `git` invocation produced.
///
/// `stderr` is captured on success as well as on failure: `git push` reports the *server's*
/// messages there, including the pull request URL an AGit push comes back with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitOutput {
    pub ok: bool,
    /// `None` when git was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    /// A successful run with no output — what a fake returns by default.
    pub fn success() -> Self {
        Self { ok: true, code: Some(0), stdout: String::new(), stderr: String::new() }
    }

    /// A failed run whose reason is `stderr`.
    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Self { ok: false, code: Some(code), stdout: String::new(), stderr: stderr.into() }
    }

    #[must_use]
    pub fn with_stdout(mut self, stdout: impl Into<String>) -> Self {
        self.stdout = stdout.into();
        self
    }

    #[must_use]
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = stderr.into();
        self
    }

    /// Whatever git said about a failure: stderr, falling back to stdout.
    ///
    /// The fallback matters because a handful of git commands report their refusal on stdout
    /// (`git merge` says "Not possible to fast-forward, aborting." there), and an error that
    /// printed nothing because it only looked at stderr is the bug this whole module exists
    /// to avoid.
    pub fn detail(&self) -> &str {
        if self.stderr.trim().is_empty() { self.stdout.trim() } else { self.stderr.trim() }
    }
}

/// A commit's message, split the way a pull request wants it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitMessage {
    pub subject: String,
    pub body: String,
}

// ------------------------------------------------------------------------------------- clone

/// `git clone`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneSpec {
    pub url: String,
    /// Where to put it. `None` lets git choose, which is the repository's own name.
    pub dir: Option<PathBuf>,
    /// Anything the user put after `--`. `OsString` because a path argument need not be UTF-8.
    pub extra: Vec<OsString>,
}

impl CloneSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), dir: None, extra: Vec::new() }
    }

    #[must_use]
    pub fn into_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    #[must_use]
    pub fn maybe_into_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.dir = dir;
        self
    }

    #[must_use]
    pub fn with_extra(mut self, extra: impl IntoIterator<Item = OsString>) -> Self {
        self.extra = extra.into_iter().collect();
        self
    }

    /// The directory the clone will land in.
    ///
    /// Guessed from the URL when no directory was given, the same way git guesses. A caller
    /// that already knows the repository's name from the API should pass it explicitly —
    /// `--bare` and `--separate-git-dir` move things, and wiring remotes into the wrong
    /// directory is worse than not wiring them at all.
    pub fn target_dir(&self) -> PathBuf {
        if let Some(dir) = &self.dir {
            return dir.clone();
        }
        PathBuf::from(dir_from_url(&self.url))
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["clone".into(), OsString::from(&self.url)];
        if let Some(dir) = &self.dir {
            argv.push(dir.clone().into_os_string());
        }
        argv.extend(self.extra.iter().cloned());
        argv
    }
}

/// The last path segment of a clone URL, minus `.git` — what `git clone` would name the
/// directory.
fn dir_from_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    // scp-like `git@host:owner/repo.git` has no `/` before `owner` on some forges
    // (`git@host:repo.git`), so `:` is a segment boundary too.
    let last = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

// ------------------------------------------------------------------------------------- fetch

/// `git fetch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchSpec {
    pub remote: String,
    pub refspecs: Vec<String>,
    /// `--force`. Needed whenever a refspec writes a local ref that would not fast-forward.
    pub force: bool,
    pub prune: bool,
    /// Show git's progress meter rather than capturing it.
    pub progress: bool,
}

impl FetchSpec {
    pub fn new(remote: impl Into<String>) -> Self {
        Self { remote: remote.into(), progress: true, ..Self::default() }
    }

    #[must_use]
    pub fn with_refspec(mut self, refspec: impl Into<String>) -> Self {
        self.refspecs.push(refspec.into());
        self
    }

    #[must_use]
    pub fn with_refspecs(mut self, refspecs: impl IntoIterator<Item = String>) -> Self {
        self.refspecs.extend(refspecs);
        self
    }

    #[must_use]
    pub fn forced(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    #[must_use]
    pub fn pruning(mut self, prune: bool) -> Self {
        self.prune = prune;
        self
    }

    /// Capture git's output instead of letting it draw a progress meter. For a fetch whose
    /// failure is tolerated, where a progress bar followed by nothing would be confusing.
    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.progress = false;
        self
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["fetch".into()];
        if self.force {
            argv.push("--force".into());
        }
        if self.prune {
            argv.push("--prune".into());
        }
        argv.push(OsString::from(&self.remote));
        argv.extend(self.refspecs.iter().map(OsString::from));
        argv
    }
}

// -------------------------------------------------------------------------------------- push

/// `git push`.
///
/// The AGit form is a [`super::AgitPush`], which builds one of these. Everything a push can
/// do is here rather than split across two types, because the difference between an ordinary
/// push and an AGit one is the refspec and nothing else — that is precisely what makes AGit
/// implementable at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushSpec {
    pub remote: String,
    pub refspecs: Vec<String>,
    pub force: bool,
    /// `-u`: record the upstream, so the next bare `git push` in this clone knows where to go.
    pub set_upstream: bool,
    /// `-o <option>` push options. How an AGit pull request's title and description reach the
    /// server: there is no request body in an AGit creation, only the push.
    pub options: Vec<String>,
    pub progress: bool,
}

impl PushSpec {
    pub fn new(remote: impl Into<String>) -> Self {
        Self { remote: remote.into(), ..Self::default() }
    }

    #[must_use]
    pub fn with_refspec(mut self, refspec: impl Into<String>) -> Self {
        self.refspecs.push(refspec.into());
        self
    }

    #[must_use]
    pub fn forced(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    #[must_use]
    pub fn setting_upstream(mut self, set: bool) -> Self {
        self.set_upstream = set;
        self
    }

    #[must_use]
    pub fn with_option(mut self, option: impl Into<String>) -> Self {
        self.options.push(option.into());
        self
    }

    /// Let git draw its progress meter instead of capturing the streams.
    ///
    /// Off by default, and deliberately: a push's stderr is the server's reply, and the
    /// commands that read it (AGit) need it captured. Turn it on only for a push whose output
    /// nobody parses.
    #[must_use]
    pub fn with_progress(mut self) -> Self {
        self.progress = true;
        self
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["push".into()];
        if self.force {
            argv.push("--force".into());
        }
        if self.set_upstream {
            argv.push("-u".into());
        }
        argv.push(OsString::from(&self.remote));
        argv.extend(self.refspecs.iter().map(OsString::from));
        for option in &self.options {
            argv.push("-o".into());
            argv.push(OsString::from(option));
        }
        argv
    }
}

// ---------------------------------------------------------------------------------- checkout

/// `git checkout`, in the three shapes the porcelain needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout {
    /// Switch to an existing branch, or to any revision.
    Rev(String),
    /// Detach HEAD onto a revision. For reading and testing: it leaves no branch behind.
    Detach(String),
    /// Create a branch and switch to it. `force` is `-B` rather than `-b`, which resets a
    /// branch that already exists.
    NewBranch { branch: String, start_point: Option<String>, force: bool },
}

impl Checkout {
    /// `git checkout -b <branch> [<start>]`.
    pub fn new_branch(branch: impl Into<String>) -> Self {
        Self::NewBranch { branch: branch.into(), start_point: None, force: false }
    }

    #[must_use]
    pub fn from_point(self, start_point: impl Into<String>) -> Self {
        match self {
            Self::NewBranch { branch, force, .. } => {
                Self::NewBranch { branch, start_point: Some(start_point.into()), force }
            }
            other => other,
        }
    }

    #[must_use]
    pub fn forced(self, force: bool) -> Self {
        match self {
            Self::NewBranch { branch, start_point, .. } => {
                Self::NewBranch { branch, start_point, force }
            }
            other => other,
        }
    }

    pub fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["checkout".into()];
        match self {
            Self::Rev(rev) => argv.push(OsString::from(rev)),
            Self::Detach(rev) => {
                argv.push("--detach".into());
                argv.push(OsString::from(rev));
            }
            Self::NewBranch { branch, start_point, force } => {
                argv.push(if *force { "-B".into() } else { OsString::from("-b") });
                argv.push(OsString::from(branch));
                if let Some(start) = start_point {
                    argv.push(OsString::from(start));
                }
            }
        }
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_directory_is_guessed_the_way_git_guesses() {
        assert_eq!(dir_from_url("https://forge/them/proj.git"), "proj");
        assert_eq!(dir_from_url("https://forge/them/proj"), "proj");
        assert_eq!(dir_from_url("https://forge/them/proj/"), "proj");
        assert_eq!(dir_from_url("git@forge:them/proj.git"), "proj");
        assert_eq!(dir_from_url("ssh://git@forge:2222/them/proj.git"), "proj");
        // A subpath install: the repository is still the last two segments, and the directory
        // is still the last one.
        assert_eq!(dir_from_url("https://forge/gitea/them/proj.git"), "proj");
    }

    #[test]
    fn an_explicit_directory_wins_over_the_guess() {
        let spec = CloneSpec::new("https://forge/them/proj.git").into_dir("elsewhere");
        assert_eq!(spec.target_dir(), PathBuf::from("elsewhere"));
        assert_eq!(
            spec.argv(),
            ["clone", "https://forge/them/proj.git", "elsewhere"]
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    /// Bug this prevents: `gea repo clone o/r -- --depth 1` dropping the passthrough
    /// arguments, or putting them before the URL where git reads `--depth` as the repository.
    #[test]
    fn extra_clone_arguments_come_last() {
        let spec = CloneSpec::new("u")
            .with_extra(["--depth".into(), "1".into()])
            .into_dir(PathBuf::from("d"));
        let argv: Vec<String> =
            spec.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, ["clone", "u", "d", "--depth", "1"]);
    }

    #[test]
    fn a_fetch_puts_force_before_the_remote() {
        let spec = FetchSpec::new("origin").with_refspec("refs/pull/7/head").forced(true);
        let argv: Vec<String> =
            spec.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, ["fetch", "--force", "origin", "refs/pull/7/head"]);
    }

    #[test]
    fn a_push_puts_options_after_the_refspec() {
        let spec = PushSpec::new("origin")
            .with_refspec("HEAD:refs/for/main/t")
            .forced(true)
            .with_option("title=x");
        let argv: Vec<String> =
            spec.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, ["push", "--force", "origin", "HEAD:refs/for/main/t", "-o", "title=x"]);
    }

    #[test]
    fn setting_upstream_is_dash_u_before_the_remote() {
        let spec = PushSpec::new("origin").with_refspec("HEAD").setting_upstream(true);
        let argv: Vec<String> =
            spec.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, ["push", "-u", "origin", "HEAD"]);
    }

    #[test]
    fn checkout_renders_its_three_shapes() {
        let render = |c: Checkout| -> Vec<String> {
            c.argv().iter().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        assert_eq!(render(Checkout::Rev("main".into())), ["checkout", "main"]);
        assert_eq!(
            render(Checkout::Detach("FETCH_HEAD".into())),
            ["checkout", "--detach", "FETCH_HEAD"]
        );
        assert_eq!(
            render(Checkout::new_branch("pr/7").from_point("FETCH_HEAD")),
            ["checkout", "-b", "pr/7", "FETCH_HEAD"]
        );
        // `-B` and not `-b --force`: the latter is not a thing, and would fail at runtime.
        assert_eq!(render(Checkout::new_branch("pr/7").forced(true)), ["checkout", "-B", "pr/7"]);
    }

    /// Bug this prevents: an error that printed nothing because `git merge --ff-only` put its
    /// refusal on stdout.
    #[test]
    fn output_detail_falls_back_to_stdout() {
        let out = GitOutput::failure(1, "").with_stdout("Not possible to fast-forward, aborting.");
        assert_eq!(out.detail(), "Not possible to fast-forward, aborting.");
        let out = GitOutput::failure(1, "  fatal: no  ").with_stdout("noise");
        assert_eq!(out.detail(), "fatal: no");
    }
}

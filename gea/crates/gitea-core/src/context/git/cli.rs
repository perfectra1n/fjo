//! `git` on `$PATH`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, ErrorKind, Result};

use super::spec::{GitIo, GitOutput};
use super::{GitCtx, Remote};

/// The real thing: `git` on `$PATH`.
#[derive(Debug, Clone, Default)]
pub struct GitCli {
    /// Directory to run in. `None` means the process's current directory.
    cwd: Option<PathBuf>,
}

impl GitCli {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs in `dir` instead of the current directory.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { cwd: Some(dir.into()) }
    }

    /// Where this will run. `None` is the process's current directory.
    pub fn dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    fn command(&self, argv: &[OsString]) -> Command {
        let mut cmd = Command::new("git");
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd.args(argv)
            // Do not take index.lock for a read: it races with an editor's background
            // `git status` and can make an `gea` invocation fail for no reason. Harmless on a
            // write, which takes the locks it actually needs explicitly.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .stdin(Stdio::null());
        cmd
    }
}

impl GitCtx for GitCli {
    fn exec(&self, argv: &[OsString], io: GitIo) -> Result<GitOutput> {
        let mut cmd = self.command(argv);
        match io {
            GitIo::Capture => {
                // `LC_ALL=C` only here. Captured output is output we parse or match against —
                // `AgitRemedy::from_stderr` looks for "non-fast-forward", `is_clean()` looks at porcelain
                // status — and under a translated locale those matches silently stop working,
                // which is a bug nobody would ever report because it looks like the feature
                // simply does not exist. Inherited output goes straight to the user, so it
                // stays in the user's own language; see the other arm.
                cmd.env("LC_ALL", "C");
                let out = cmd.output().map_err(spawn_error)?;
                Ok(GitOutput {
                    ok: out.status.success(),
                    code: out.status.code(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                })
            }
            GitIo::Inherit => {
                // Deliberately *not* `LC_ALL=C`: this is `clone` and `fetch`, whose progress
                // meter the user reads. Forcing the C locale would un-translate it for
                // everyone who set a locale, to no benefit — nothing parses it.
                let status = cmd
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status()
                    .map_err(spawn_error)?;
                Ok(GitOutput {
                    ok: status.success(),
                    code: status.code(),
                    // Nothing was captured; git already printed it.
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
    }

    fn git_dir(&self) -> Result<Option<PathBuf>> {
        let out = self.try_run(&["rev-parse", "--git-dir"])?;
        let dir = out.stdout.trim();
        Ok(if out.ok && !dir.is_empty() { Some(PathBuf::from(dir)) } else { None })
    }

    fn remotes(&self) -> Result<Vec<Remote>> {
        let out = self.try_run(&["remote", "-v"])?;
        if !out.ok {
            return Ok(Vec::new());
        }
        Ok(parse_remote_v(&out.stdout))
    }

    fn config_get(&self, key: &str) -> Result<Option<String>> {
        let out = self.try_run(&["config", "--get", key])?;
        Ok(if out.ok { Some(out.stdout.trim().to_owned()) } else { None })
    }

    fn config_get_regexp(&self, pattern: &str) -> Result<Vec<(String, String)>> {
        let out = self.try_run(&["config", "--get-regexp", pattern])?;
        if !out.ok {
            // Exit 1 simply means no key matched.
            return Ok(Vec::new());
        }
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
            .collect())
    }

    fn config_set_local(&self, key: &str, value: &str) -> Result<()> {
        let out = self.try_run(&["config", "--local", key, value])?;
        if !out.ok {
            return Err(super::failed(&["config", "--local", key, value], &out));
        }
        Ok(())
    }

    fn config_unset_local(&self, key: &str) -> Result<()> {
        let out = self.try_run(&["config", "--local", "--unset", key])?;
        // git exits 5 for "the key did not exist", which is success for an idempotent unset.
        // Matching on the exit code rather than on empty stderr, because a *real* failure with
        // a quiet git would otherwise be swallowed.
        if !out.ok && out.code != Some(5) {
            return Err(super::failed(&["config", "--local", "--unset", key], &out));
        }
        Ok(())
    }

    fn current_branch(&self) -> Result<Option<String>> {
        let out = self.try_run(&["branch", "--show-current"])?;
        let name = out.stdout.trim();
        Ok(if out.ok && !name.is_empty() { Some(name.to_owned()) } else { None })
    }

    fn remote_head(&self, remote: &str) -> Result<Option<String>> {
        let refname = format!("refs/remotes/{remote}/HEAD");
        let out = self.try_run(&["symbolic-ref", &refname])?;
        if !out.ok {
            return Ok(None);
        }
        // refs/remotes/origin/main -> main
        Ok(out.stdout.trim().rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_owned))
    }
}

/// `git` itself could not be started. Distinguished from "git ran and refused", because the
/// remedy is completely different and a caller that conflates them prints the wrong one.
fn spawn_error(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        return Error::new(ErrorKind::Usage(
            "`git` was not found on PATH; install git, or pass -R owner/name to skip repository \
             detection"
                .to_owned(),
        ));
    }
    Error::new(ErrorKind::Io(e))
}

/// Parses `git remote -v` output.
///
/// ```text
/// origin  https://git.example.org/o/r.git (fetch)
/// origin  ssh://git@git.example.org/o/r.git (push)
/// ```
///
/// The separator is a tab, but the URL may itself contain spaces (a local path remote), so
/// the trailing `(fetch)`/`(push)` marker is stripped from the end rather than split on.
pub(super) fn parse_remote_v(out: &str) -> Vec<Remote> {
    let mut by_name: BTreeMap<String, Remote> = BTreeMap::new();
    for line in out.lines() {
        let Some((name, rest)) = line.split_once('\t') else {
            continue;
        };
        let rest = rest.trim_end();
        let (url, kind) = match rest.rsplit_once(' ') {
            Some((u, k)) => (u.trim(), k),
            None => (rest, ""),
        };
        let entry = by_name.entry(name.to_owned()).or_insert_with(|| Remote {
            name: name.to_owned(),
            fetch: None,
            push: None,
        });
        match kind {
            "(fetch)" => entry.fetch = Some(url.to_owned()),
            "(push)" => entry.push = Some(url.to_owned()),
            // No marker at all: treat it as both, so a future git format change degrades
            // into something usable rather than into "no remotes".
            _ => {
                let whole = rest.to_owned();
                entry.fetch.get_or_insert_with(|| whole.clone());
                entry.push.get_or_insert(whole);
            }
        }
    }
    by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_v_with_split_urls() {
        // The triangular-workflow case: pushurl differs from the fetch URL, and reading the
        // wrong one sends a PR to the wrong repository.
        let out = "origin\thttps://git.example.org/me/fork.git (fetch)\n\
                   origin\tssh://git@git.example.org/me/fork.git (push)\n\
                   upstream\thttps://git.example.org/them/proj.git (fetch)\n\
                   upstream\thttps://git.example.org/them/proj.git (push)\n";
        let remotes = parse_remote_v(out);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].fetch.as_deref(), Some("https://git.example.org/me/fork.git"));
        assert_eq!(remotes[0].push.as_deref(), Some("ssh://git@git.example.org/me/fork.git"));
        // Fetch is tried first.
        assert_eq!(
            remotes[0].urls().collect::<Vec<_>>(),
            ["https://git.example.org/me/fork.git", "ssh://git@git.example.org/me/fork.git"]
        );
    }

    #[test]
    fn remote_url_containing_a_space_survives() {
        // A local-path remote can contain spaces; splitting on whitespace truncates it.
        let out = "local\t/home/me/my repos/x.git (fetch)\n";
        let remotes = parse_remote_v(out);
        assert_eq!(remotes[0].fetch.as_deref(), Some("/home/me/my repos/x.git"));
    }
}

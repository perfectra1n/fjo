//! The pager, with `gh`'s precedence and `gh`'s escape hatches.
//!
//! Precedence is `GEA_PAGER` → `PAGER` → the config file's `pager` value. The tool-specific
//! variable comes first so that `GEA_PAGER=cat gea pr list` disables paging for one command
//! without disturbing `PAGER`, which the user set for `git` and `man`.
//!
//! Paging is skipped entirely when stdout is not a TTY, when the resolved command is empty,
//! or when it is literally `cat`. The last two are how a user turns paging off, and `cat` is
//! special-cased rather than actually executed because spawning `cat` just to copy bytes
//! costs a process and an extra pipe, and — the real reason — a `cat` child breaks the
//! `BrokenPipe`-means-success handling below one level deeper than we can see.

use std::io::{self, Write};
use std::process::{Child, Command, Stdio};

use super::tty::{Env, Term};

/// Resolve the pager command, or `None` when output should not be paged.
///
/// Returns the split argv, so `PAGER="less -FRX"` works and `PAGER="/opt/my pager/bin/p"`
/// does not have to.
pub fn resolve(env: &dyn Env, term: &Term, config: Option<&str>) -> Option<Vec<String>> {
    if !term.tty {
        // A pager writing into a pipe would either block forever waiting for a keypress or
        // inject terminal control sequences into machine-read output.
        return None;
    }
    let raw =
        env.var("GEA_PAGER").or_else(|| env.var("PAGER")).or_else(|| config.map(str::to_string))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "cat" {
        return None;
    }
    let argv = split_command(trimmed);
    argv.first().is_some_and(|p| !p.is_empty()).then_some(argv)
}

/// Split a command string into argv, honouring single and double quotes and backslash
/// escapes.
///
/// A naive `split_whitespace` mangles `PAGER='less -R'` on Windows paths and on any path with
/// a space in it, and the resulting "No such file or directory" names only the first word,
/// which makes it look like the pager is missing rather than mis-split.
pub fn split_command(s: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (Some(_), '\\') => match chars.next() {
                Some(next) => cur.push(next),
                None => cur.push('\\'),
            },
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                started = true;
                quote = Some(c);
            }
            (None, '\\') => match chars.next() {
                Some(next) => {
                    started = true;
                    cur.push(next);
                }
                None => cur.push('\\'),
            },
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    argv.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            (None, c) => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started || !cur.is_empty() {
        argv.push(cur);
    }
    argv
}

/// A running pager, or a passthrough to stdout.
pub struct Pager {
    child: Option<Child>,
}

impl Pager {
    /// Spawn the pager, or fall through to stdout when [`resolve`] declined.
    ///
    /// A pager that fails to spawn is **not** an error: the user asked to see output, not to
    /// see `less`. We fall back to stdout, which is the outcome they wanted anyway.
    pub fn start(env: &dyn Env, term: &Term, config: Option<&str>) -> Self {
        let Some(argv) = resolve(env, term, config) else {
            return Self { child: None };
        };
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::piped());

        // Strip the inherited PAGER so a pager that itself shells out (git's `less` wrapper,
        // `delta`, `bat`) does not recursively re-page and hang on a second nested pager.
        cmd.env_remove("PAGER");

        // `F` quits if the content fits on one screen, `R` passes our SGR escapes through
        // instead of showing `ESC[32m`, and `X` suppresses the alt-screen switch so short
        // output stays visible after exit. Only injected when unset: a user who configured
        // LESS deliberately gets to keep it.
        if env.var("LESS").is_none() {
            cmd.env("LESS", "FRX");
        }
        if env.var("LV").is_none() {
            cmd.env("LV", "-c");
        }

        Self { child: cmd.spawn().ok() }
    }

    /// A `Pager` that never pages. For tests and for `--output <file>`.
    pub fn none() -> Self {
        Self { child: None }
    }

    pub fn is_paging(&self) -> bool {
        self.child.is_some()
    }

    /// Write everything through the pager, then wait for it.
    ///
    /// `BrokenPipe` is success, not failure. A user who reads the first screen and presses `q`
    /// closes our pipe mid-write; treating that as an error would print a diagnostic and exit
    /// non-zero for the most ordinary interaction the pager has.
    pub fn finish<F>(mut self, write: F) -> io::Result<()>
    where
        F: FnOnce(&mut dyn Write) -> io::Result<()>,
    {
        let result = match self.child.as_mut() {
            Some(child) => {
                let mut stdin = child.stdin.take().expect("stdin was piped");
                let r = write(&mut stdin).and_then(|()| stdin.flush());
                drop(stdin);
                r
            }
            None => {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                write(&mut lock).and_then(|()| lock.flush())
            }
        };
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        match result {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        }
    }
}

impl Drop for Pager {
    /// If `finish` was never called (an error path unwound past it), still reap the child so
    /// the terminal is handed back rather than left in the pager's alternate screen.
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            drop(child.stdin.take());
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::tty::MapEnv;

    fn tty() -> Term {
        Term::tty(80)
    }

    /// Bug this prevents: reading `PAGER` before `GEA_PAGER`, so a per-command
    /// `GEA_PAGER=cat` is ignored and the user cannot disable paging without editing their
    /// shell profile.
    #[test]
    fn gea_pager_wins_over_pager_wins_over_config() {
        let env = MapEnv::new().with("GEA_PAGER", "moar").with("PAGER", "less");
        assert_eq!(resolve(&env, &tty(), Some("bat")), Some(vec!["moar".into()]));

        let env = MapEnv::new().with("PAGER", "less");
        assert_eq!(resolve(&env, &tty(), Some("bat")), Some(vec!["less".into()]));

        assert_eq!(resolve(&MapEnv::new(), &tty(), Some("bat")), Some(vec!["bat".into()]));
        assert_eq!(resolve(&MapEnv::new(), &tty(), None), None);
    }

    /// Bug this prevents: paging into a pipe, where `less` blocks forever waiting for a
    /// keypress that will never come and the command appears to hang.
    #[test]
    fn never_page_when_not_a_tty() {
        let env = MapEnv::new().with("PAGER", "less");
        assert_eq!(resolve(&env, &Term::piped(), None), None);
    }

    /// Bug this prevents: actually spawning `cat`, and an empty `PAGER=` resolving to a
    /// command named `""`. Both are how users turn paging off.
    #[test]
    fn cat_and_empty_mean_no_pager() {
        for value in ["cat", "", "   ", "  cat  "] {
            let env = MapEnv::new().with("PAGER", value);
            assert_eq!(resolve(&env, &tty(), None), None, "PAGER={value:?}");
        }
        // ...but `cat -v` is a real request for a real program.
        let env = MapEnv::new().with("PAGER", "cat -v");
        assert_eq!(resolve(&env, &tty(), None), Some(vec!["cat".into(), "-v".into()]));
    }

    /// Bug this prevents: `split_whitespace`, which turns `/opt/my pager/p` into two
    /// nonexistent programs and reports only the first in the error.
    #[test]
    fn command_splitting_respects_quotes() {
        assert_eq!(split_command("less -FRX"), vec!["less", "-FRX"]);
        assert_eq!(split_command("  less   -R  "), vec!["less", "-R"]);
        assert_eq!(split_command("\"/opt/my pager/p\" -x"), vec!["/opt/my pager/p", "-x"]);
        assert_eq!(split_command("'/opt/my pager/p'"), vec!["/opt/my pager/p"]);
        assert_eq!(split_command("/opt/my\\ pager/p"), vec!["/opt/my pager/p"]);
        assert_eq!(split_command("less '-P a b'"), vec!["less", "-P a b"]);
        // An empty quoted argument is a real, distinct argument.
        assert_eq!(split_command("p ''"), vec!["p", ""]);
        assert_eq!(split_command(""), Vec::<String>::new());
    }

    /// Bug this prevents: `Pager::none().finish(..)` diverging from the paged path, so the
    /// no-pager case is never exercised by any other test.
    #[test]
    fn passthrough_pager_writes_nothing_of_its_own() {
        assert!(!Pager::none().is_paging());
    }
}

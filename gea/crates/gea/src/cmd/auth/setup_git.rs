//! `gea auth setup-git` — teach `git` to authenticate with the token `gea` already has.
//!
//! Without this, a user who has just run `gea auth login` still gets a password prompt from
//! `git push`, and the obvious "fix" is to paste the token into the remote URL — where it ends up
//! in `.git/config`, in `git remote -v` output, and in every screenshot of the repository.

use std::io::Write;
use std::process::Command;

use clap::Args as ClapArgs;
use gitea_core::error::Result;

use super::common::{self, Setup};
use crate::cmd::support;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    /// Print the `git config` commands instead of running them
    #[arg(long)]
    pub dry_run: bool,
}

const LONG_HELP: &str = "\
Configure Git to authenticate over HTTPS using your saved gea token.

Writes a host-specific credential helper to your global Git configuration.
Other hosts and helpers are unchanged. Avoid tokens in remote URLs: they can
appear in .git/config and git remote -v output.

  gea auth setup-git --host git.example.org
  gea auth setup-git --host git.example.org --dry-run";

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let setup = Setup::load()?;
    let key = setup.hosts.resolve_host(globals.host.as_deref(), common::env())?;
    let entry = setup
        .hosts
        .get(&key)
        .ok_or_else(|| support::usage(format!("{key} is not a configured host")))?;

    let helper = helper_command()?;
    let section = format!("credential.{}.helper", entry.url.trim_end_matches('/'));

    // Two invocations, in this order, copying `gh`:
    //
    //   --replace-all <section> ""   resets the list to a single empty entry
    //   --add        <section> <us>  appends ours
    //
    // The empty first entry is not noise: git treats an empty helper value as "discard every
    // helper configured so far", so this makes ours authoritative for this host without
    // disturbing helpers configured for any other. Running only `--add` would leave a stale
    // entry from a previous `setup-git` behind, and git would ask the older one first.
    let mut steps: Vec<Vec<String>> = vec![
        vec![
            "config".to_owned(),
            "--global".to_owned(),
            "--replace-all".to_owned(),
            section.clone(),
            String::new(),
        ],
        vec![
            "config".to_owned(),
            "--global".to_owned(),
            "--add".to_owned(),
            section,
            helper.clone(),
        ],
    ];

    // A subpath install shares its authority with anything else behind the same proxy, and git
    // only sends the path to a helper when asked to. Without this, two Gitea instances at
    // example.org/a and example.org/b are indistinguishable to the helper.
    if !key.subpath().is_empty() {
        steps.push(vec![
            "config".to_owned(),
            "--global".to_owned(),
            format!("credential.{}.useHttpPath", entry.url.trim_end_matches('/')),
            "true".to_owned(),
        ]);
    }

    let mut out = support::writer(globals)?;
    if args.dry_run {
        for step in &steps {
            // Empty and space-bearing arguments are shown quoted, so the printed line is one a
            // reader can paste. The `--replace-all … ''` step is *entirely* an empty argument, and
            // printing it bare would look like a truncated command.
            let shown: Vec<String> = step
                .iter()
                .map(|a| if a.is_empty() || a.contains(' ') { format!("'{a}'") } else { a.clone() })
                .collect();
            writeln!(out, "git {}", shown.join(" "))?;
        }
        out.flush()?;
        return Ok(());
    }

    for step in &steps {
        run_git(step)?;
    }
    writeln!(out, "git will now authenticate to {key} with your gea token")?;
    writeln!(out, "Helper: {helper}")?;
    out.flush()?;
    Ok(())
}

/// The value git stores, e.g. `!/usr/local/bin/gea auth git-credential`.
///
/// A leading `!` makes git run the string as a shell command rather than looking for
/// `git-credential-<name>` on `$PATH`. The absolute path of *this* executable is used rather than
/// the bare word `gea`, so a user running `./target/debug/gea` does not end up with a git
/// configuration pointing at some other `gea` on their `$PATH`.
fn helper_command() -> Result<String> {
    let exe = std::env::current_exe().map_err(|e| {
        support::usage(format!(
            "cannot find the gea executable path for the Git credential helper: {e}"
        ))
    })?;
    let path = exe.display().to_string();
    // git runs the value through a shell, so a path with a space in it needs quoting. Single
    // quotes with the usual `'\''` escape, which is safe for every byte a path can hold.
    if path.contains(['\'', ' ', '"', '$', '\\']) {
        return Ok(format!("!'{}' auth git-credential", path.replace('\'', r"'\''")));
    }
    Ok(format!("!{path} auth git-credential"))
}

/// `git config --global` has no equivalent on [`gitea_core::context::GitCtx`], which exposes
/// the read paths plus a repository-*local* setter. Shelling out here is deliberate and narrow: it
/// is a write to the user's own git configuration, not remote-URL parsing, which
/// `docs/porcelain-conventions.md` reserves to `gitea_core::context`.
fn run_git(args: &[String]) -> Result<()> {
    let out = Command::new("git").args(args).output().map_err(|e| {
        support::usage(format!("could not run git ({e}); it has to be installed for this to work"))
    })?;
    if !out.status.success() {
        return Err(support::usage(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: a helper entry that names the bare word `gea`, so a developer testing
    /// `./target/debug/gea auth setup-git` silently configures git to use whatever other `gea`
    /// is first on `$PATH` — or none at all.
    #[test]
    fn the_helper_names_this_executable_absolutely() {
        let helper = helper_command().unwrap();
        assert!(helper.starts_with('!'), "{helper}");
        assert!(helper.ends_with(" auth git-credential"), "{helper}");
        let exe = std::env::current_exe().unwrap().display().to_string();
        assert!(helper.contains(exe.trim_matches('\'')), "{helper}");
    }

    /// Bug this prevents: a path containing a space becoming two shell words, so git reports
    /// `credential helper '!/Users/a' not found` and nobody connects that to the space in
    /// `/Users/a b/bin/gea`.
    #[test]
    fn a_path_with_a_space_is_quoted() {
        // Exercised through the same expression the function uses, because `current_exe` cannot
        // be made to contain a space from inside a test.
        let quote = |path: &str| {
            if path.contains(['\'', ' ', '"', '$', '\\']) {
                format!("!'{}' auth git-credential", path.replace('\'', r"'\''"))
            } else {
                format!("!{path} auth git-credential")
            }
        };
        assert_eq!(quote("/a b/gea"), "!'/a b/gea' auth git-credential");
        assert_eq!(quote("/a/gea"), "!/a/gea auth git-credential");
        assert_eq!(quote("/it's/gea"), r"!'/it'\''s/gea' auth git-credential");
    }
}

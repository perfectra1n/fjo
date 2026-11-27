//! `gea repo clone` — clone, then wire up the remotes a contributor actually needs.
//!
//! `git clone` on its own is one command and needs no wrapper. What earns this its place is the
//! part everybody does by hand afterwards, and half of them get wrong: when the thing you cloned
//! is a **fork**, add an `upstream` remote pointing at the parent and record that `upstream` — not
//! `origin` — is the repository `gea` commands are about.
//!
//! That last step is the one that matters. Without it `gea pr create` in a fresh fork clone
//! targets the fork, opening a pull request from a branch to itself; the resolver's remote-name
//! scoring (`upstream` 3, `origin` 1, see `gitea_core::context`) is what fixes it, and this
//! command is what puts the `upstream` remote there for it to score.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args as ClapArgs;
use gitea_core::context::git::{CloneSpec, FetchSpec, GitCli, GitCtx};
use gitea_core::context::{RESOLVED_BASE, resolved_key};
use gitea_core::{Result, types::RepoSlug};
use gitea_model::Repository;

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Clone a repository.

Accepts owner/name, a name in your account, or a URL. For forks, adds the parent
as `upstream` and uses it as the default base for pull requests.
Arguments after -- are passed to git clone.

  gea repo clone gitea/gitea
  gea repo clone my-thing ~/src/my-thing
  gea repo clone gitea/gitea -- --depth 1 --filter=blob:none")]
pub struct Args {
    /// `owner/name`, a bare name in your own account, or a URL
    #[arg(value_name = "REPOSITORY")]
    pub repo: String,

    /// Directory to clone into. Defaults to the repository name
    #[arg(value_name = "DIRECTORY")]
    pub directory: Option<PathBuf>,

    /// Name for the remote added for a fork's parent
    #[arg(long, value_name = "NAME", default_value = "upstream")]
    pub upstream_name: String,

    /// Clone over SSH instead of HTTPS
    #[arg(long)]
    pub ssh: bool,

    /// Extra arguments for `git clone`
    #[arg(last = true, value_name = "GIT-ARGS")]
    pub git_args: Vec<OsString>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::slug_from_arg(&rt, &api, &args.repo).await?;
        let repo = api.repo().get(&slug.owner, &slug.name).await?;
        clone(&rt, args, &slug, &repo)
    })
}

fn clone(rt: &Runtime, args: &Args, slug: &RepoSlug, repo: &Repository) -> Result<()> {
    // The directory is always passed to `git clone` explicitly, even when it is the one git
    // would have chosen anyway. That is what makes the work tree's location *known* rather than
    // guessed: `-- --bare` and `-- --separate-git-dir` move where a clone lands, and wiring
    // remotes into the wrong directory would be worse than not wiring them at all.
    let spec = CloneSpec::new(clone_url(repo, args.ssh))
        .into_dir(args.directory.clone().unwrap_or_else(|| PathBuf::from(&repo.name)))
        .with_extra(args.git_args.iter().cloned());
    let dir = rt.git().clone_repo(&spec)?;
    let git = GitCli::in_dir(&dir);

    if !repo.fork {
        return Ok(());
    }
    // `parent` is only populated on a single-repository GET, which is why this command fetches
    // the repository rather than trusting whatever the caller already had.
    let Some(parent) = repo.parent.as_deref() else {
        support::note(rt.term(), &format!("{slug} is a fork, but the API did not name its parent"));
        return Ok(());
    };

    let upstream = &args.upstream_name;
    if git.remote_exists(upstream)? {
        support::note(rt.term(), &format!("remote {upstream} already exists; leaving it alone"));
    } else {
        git.remote_add(upstream, &clone_url(parent, args.ssh))?;
        // `--filter=blob:none` style clones do not want a full upstream fetch, and a `git fetch`
        // that fails must not fail the clone: the remote is configured either way, and the next
        // `git fetch upstream` will report the problem in context. Captured rather than shown,
        // because a progress meter followed by "could not fetch" reads as a crash.
        if git.fetch(&FetchSpec::new(upstream).quiet()).is_err() {
            support::note(
                rt.term(),
                &format!("could not fetch {upstream}; the remote is configured"),
            );
        }
    }

    // The point of the whole exercise: pull requests belong to the parent.
    git.config_set_local(&resolved_key(upstream), RESOLVED_BASE)?;
    support::note(
        rt.term(),
        &format!(
            "{} added as {upstream}, and set as the base repository for gea commands",
            parent.full_name
        ),
    );
    Ok(())
}

/// The URL to clone from.
///
/// Whatever this returns is still subject to the user's `url.<base>.insteadOf` rules, because we
/// shell out to `git`; that is the whole reason `git2` is banned. So an instance whose API
/// advertises an unreachable `https://` URL still works for anyone who has the rewrite rule their
/// colleagues have.
pub(crate) fn clone_url(repo: &Repository, ssh: bool) -> String {
    if ssh && !repo.ssh_url.is_empty() { repo.ssh_url.clone() } else { repo.clone_url.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repository {
        Repository {
            name: "proj".to_owned(),
            clone_url: "https://git.example.org/them/proj.git".to_owned(),
            ssh_url: "git@git.example.org:them/proj.git".to_owned(),
            ..Repository::default()
        }
    }

    #[test]
    fn ssh_is_opt_in_and_falls_back_when_the_instance_offers_no_ssh_url() {
        assert_eq!(clone_url(&repo(), false), "https://git.example.org/them/proj.git");
        assert_eq!(clone_url(&repo(), true), "git@git.example.org:them/proj.git");
        // An instance with SSH disabled sends an empty `ssh_url`; cloning "" would fail with a
        // baffling message instead of just using HTTPS.
        let no_ssh = Repository { ssh_url: String::new(), ..repo() };
        assert_eq!(clone_url(&no_ssh, true), "https://git.example.org/them/proj.git");
    }

    /// Bug this prevents: `gea repo clone o/r -- --depth 1` treating `--depth` as one of our own
    /// flags and failing with "unexpected argument".
    #[test]
    fn git_arguments_after_a_double_dash_are_passed_through() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        let h = <Harness as clap::Parser>::try_parse_from([
            "gea",
            "o/r",
            "dir",
            "--",
            "--depth",
            "1",
            "--filter=blob:none",
        ])
        .expect("parses");
        assert_eq!(h.args.repo, "o/r");
        assert_eq!(h.args.directory.as_deref(), Some(std::path::Path::new("dir")));
        assert_eq!(h.args.git_args, ["--depth", "1", "--filter=blob:none"]);
    }
}

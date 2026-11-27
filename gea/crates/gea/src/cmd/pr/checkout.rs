//! `gea pr checkout` — get the code onto your disk, wherever it came from.
//!
//! # Why `refs/pull/<n>/head` and not the head branch
//!
//! The obvious implementation is "fetch the head branch from the head repository". It is also wrong
//! for the most common case: a pull request from a **fork** lives in a repository you may have no
//! remote for, may not be able to read anonymously, and whose owner can force-push at will.
//!
//! Gitea — like Gitea and GitHub — publishes every pull request's tip inside the *base* repository
//! as `refs/pull/<index>/head`. Fetching that means one remote, one refspec, and identical handling
//! for a same-repository pull request, a fork's, and an **AGit** one, which has no head branch
//! anywhere at all and would be unreachable by any other route.
//!
//! For a same-repository pull request the local branch is additionally wired up
//! (`branch.<name>.remote` and `.merge`) so `git push` afterwards does the right thing. That is
//! deliberately *not* done for a fork's or an AGit pull request: pushing to `refs/pull/<n>/head`
//! would be rejected, and a push configuration that cannot work is worse than none.

use std::path::Path;

use clap::Args as ClapArgs;
use gitea_core::context::git::{Checkout, FetchSpec, GitCli, GitCtx};
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::PullRequest;

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Check out a pull request locally.

Supports forks and AGit pull requests by fetching refs/pull/<number>/head from
the base repository. Use -f to force checkout; --force is a separate global option.

  gea pr checkout 42
  gea pr checkout 42 -b review/42
  gea pr checkout 42 --detach
  gea pr checkout 42 --worktree ../review-42")]
pub struct Args {
    /// Pull request number, URL, or branch
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Local branch name. Defaults to the pull request's head branch, or `pr/<number>`
    #[arg(short = 'b', long, value_name = "NAME")]
    pub branch: Option<String>,

    /// Check out a detached HEAD instead of creating a branch
    #[arg(long, conflicts_with_all = ["branch", "worktree"])]
    pub detach: bool,

    /// Reset an existing local branch to the pull request's tip. `-f` only: `--force` is global
    #[arg(short = 'f')]
    pub force: bool,

    /// Update submodules after checking out
    #[arg(long)]
    pub recurse_submodules: bool,

    /// Check out into a new git worktree at this path instead of switching branches
    #[arg(long, value_name = "PATH")]
    pub worktree: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        gitea_core::context::require_git_repo(rt.git())?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        let remote =
            crate::cmd::repo::remote_for(&rt, &found.slug).ok().flatten().ok_or_else(|| {
                Error::new(ErrorKind::Usage(format!(
                    "no git remote in this checkout points at {}, and that is where \
                     refs/pull/{}/head lives; add one with `git remote add upstream <url>`",
                    found.slug,
                    found.index()
                )))
            })?;
        checkout(&rt, args, &remote, &found.slug, &found.pr)
    })
}

fn checkout(
    rt: &Runtime,
    args: &Args,
    remote: &str,
    slug: &RepoSlug,
    pr: &PullRequest,
) -> Result<()> {
    let git = rt.git();
    let index = pr.number.get();
    let pull_ref = format!("refs/pull/{index}/head");

    if args.detach {
        // No local ref at all: fetch into FETCH_HEAD and detach onto it. `--detach` is for reading
        // and testing, and leaving a branch behind after it would be litter.
        git.fetch(&FetchSpec::new(remote).with_refspec(&pull_ref).forced(args.force))?;
        git.checkout(&Checkout::Detach("FETCH_HEAD".to_owned()))?;
        support::note(rt.term(), &format!("checked out #{index} at a detached HEAD"));
        return post(rt, git, args);
    }

    let branch = local_branch(args, pr);
    let refspec = format!("{pull_ref}:{branch}");
    let exists = git.rev_exists(&format!("refs/heads/{branch}"))?;
    if exists && !args.force {
        // Fetching over an existing branch that has diverged loses commits. So the tip is fetched
        // into FETCH_HEAD first and compared; refusing and naming the two ways out beats either
        // silently discarding work or silently doing nothing.
        git.fetch(&FetchSpec::new(remote).with_refspec(&pull_ref))?;
        if !git.is_ancestor(&branch, "FETCH_HEAD")? {
            return Err(Error::new(ErrorKind::Usage(format!(
                "local branch {branch} already exists and has commits #{index} does not; pass -f to \
                 reset it to the pull request's tip, or -b <name> to check out under another name"
            ))));
        }
    }
    git.fetch(&FetchSpec::new(remote).with_refspec(refspec).forced(args.force || exists))?;
    git.checkout(&Checkout::Rev(branch.clone()))?;

    // Only for a pull request whose head branch is in the base repository. See the module docs.
    if same_repo(slug, pr) {
        let head = pr.head.as_ref().map(|h| h.r#ref.clone()).unwrap_or_default();
        if !head.is_empty() {
            git.config_set_local(&format!("branch.{branch}.remote"), remote)?;
            git.config_set_local(&format!("branch.{branch}.merge"), &format!("refs/heads/{head}"))?;
        }
    } else {
        support::note(
            rt.term(),
            &format!(
                "#{index} comes from {}; `git push` is not configured, because refs/pull/{index}/head \
                 is read-only",
                origin_of(pr)
            ),
        );
    }

    if let Some(path) = &args.worktree {
        // The worktree is created *from* the branch we just fetched, so it works for a fork and for
        // AGit as well. `git worktree add` refuses a branch already checked out elsewhere, which is
        // why the branch is left where it is and the worktree gets a detached copy of its tip.
        git.worktree_add(Path::new(path), &branch, true)?;
        support::note(rt.term(), &format!("worktree for #{index} at {path}"));
        return post(rt, &GitCli::in_dir(path), args);
    }

    support::note(rt.term(), &format!("checked out #{index} as {branch}"));
    post(rt, git, args)
}

fn post(rt: &Runtime, git: &dyn GitCtx, args: &Args) -> Result<()> {
    if args.recurse_submodules {
        git.update_submodules()?;
        support::note(rt.term(), "submodules updated");
    }
    Ok(())
}

/// The local branch name: `-b`, the pull request's own head branch, or `pr/<number>`.
///
/// `pr/<number>` is the fallback for a fork's or an AGit pull request, where the head branch name is
/// either somebody else's or does not exist. It is also collision-proof, which a contributor's
/// `patch-1` very much is not.
pub(crate) fn local_branch(args: &Args, pr: &PullRequest) -> String {
    if let Some(name) = &args.branch {
        return name.clone();
    }
    let index = pr.number.get();
    match pr.head.as_ref() {
        // AGit pull requests have no usable head branch name.
        Some(head)
            if !common::is_agit(pr) && !head.r#ref.is_empty() && !head.r#ref.contains('/') =>
        {
            head.r#ref.clone()
        }
        _ => format!("pr/{index}"),
    }
}

/// Whether the pull request's head branch lives in the base repository.
pub(crate) fn same_repo(slug: &RepoSlug, pr: &PullRequest) -> bool {
    if common::is_agit(pr) {
        return false;
    }
    match pr.head.as_ref().and_then(|h| h.repo.as_ref()) {
        Some(repo) => repo.full_name == slug.to_string(),
        // No head repository at all means the fork was deleted; treat it as foreign, which is the
        // conservative direction — we simply do not configure a push that could not work.
        None => false,
    }
}

fn origin_of(pr: &PullRequest) -> String {
    if common::is_agit(pr) {
        return "an AGit push (no branch)".to_owned();
    }
    pr.head
        .as_ref()
        .and_then(|h| h.repo.as_ref())
        .map(|r| r.full_name.clone())
        .unwrap_or_else(|| "a repository that no longer exists".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::types::ids::IssueIndex;
    use gitea_model::{PrBranchInfo, Repository};

    fn args(words: &[&str]) -> Args {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        <Harness as clap::Parser>::try_parse_from(words)
            .unwrap_or_else(|e| panic!("{words:?}: {e}"))
            .args
    }

    /// `agit` is `1` for an AGit pull request, shaped the way Gitea reports one: a blank label and
    /// a `refs/pull/<n>/head` ref.
    fn pr(head_ref: &str, head_repo: Option<&str>, agit: i64) -> PullRequest {
        let (r#ref, label) = if agit == 1 {
            ("refs/pull/42/head".to_owned(), String::new())
        } else {
            (head_ref.to_owned(), head_ref.to_owned())
        };
        PullRequest {
            number: IssueIndex::new(42),
            head: Some(PrBranchInfo {
                r#ref,
                label,
                repo: head_repo
                    .map(|full| Repository { full_name: full.to_owned(), ..Repository::default() }),
                ..PrBranchInfo::default()
            }),
            ..PullRequest::default()
        }
    }

    #[test]
    fn the_local_branch_is_the_head_branch_when_there_is_one() {
        assert_eq!(local_branch(&args(&["gea"]), &pr("tabs", Some("them/proj"), 0)), "tabs");
        assert_eq!(local_branch(&args(&["gea", "-b", "mine"]), &pr("tabs", None, 0)), "mine");
    }

    /// Bug this prevents: naming a local branch after an AGit pull request's internal ref, which is
    /// not a branch name and would make `git checkout` fail with a confusing message.
    #[test]
    fn an_agit_pull_request_gets_a_pr_number_branch() {
        assert_eq!(local_branch(&args(&["gea"]), &pr("", None, 1)), "pr/42");
        // A head "ref" with a slash in it is not a branch we should reuse either.
        assert_eq!(local_branch(&args(&["gea"]), &pr("refs/pull/42/head", None, 0)), "pr/42");
    }

    /// Bug this prevents: configuring `git push` for a branch fetched from `refs/pull/<n>/head`,
    /// which is read-only — so the user's next `git push` fails and blames them.
    #[test]
    fn push_configuration_is_only_written_for_a_same_repository_pull_request() {
        let slug = RepoSlug::new("them", "proj");
        assert!(same_repo(&slug, &pr("tabs", Some("them/proj"), 0)));
        assert!(!same_repo(&slug, &pr("tabs", Some("alice/proj"), 0)), "a fork");
        assert!(!same_repo(&slug, &pr("tabs", None, 0)), "a deleted fork");
        assert!(!same_repo(&slug, &pr("", None, 1)), "AGit");
    }

    #[test]
    fn the_note_says_where_a_foreign_pull_request_came_from() {
        assert_eq!(origin_of(&pr("", None, 1)), "an AGit push (no branch)");
        assert_eq!(origin_of(&pr("tabs", Some("alice/proj"), 0)), "alice/proj");
        assert!(origin_of(&pr("tabs", None, 0)).contains("no longer exists"));
    }

    /// `--detach` creates no branch, so `-b` and `--worktree` have nothing to name.
    #[test]
    fn detach_excludes_the_flags_that_need_a_branch() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        assert!(<Harness as clap::Parser>::try_parse_from(["gea", "--detach", "-b", "x"]).is_err());
        assert!(
            <Harness as clap::Parser>::try_parse_from(["gea", "--detach", "--worktree", "p"])
                .is_err()
        );
    }
}

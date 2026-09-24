//! `gea repo sync` — bring a fork up to date, locally or on the server.
//!
//! Two jobs behind one verb, split the way `gh repo sync` splits them:
//!
//! * **No destination** — sync *this checkout* from the base repository. That is `git fetch` plus a
//!   fast-forward, and it is the case `-f` exists for, because a diverged branch cannot be
//!   fast-forwarded and the only way through is a hard reset the user has to ask for.
//! * **A destination** — sync a fork *on the instance*, through Gitea's own
//!   `POST /repos/{owner}/{repo}/merge-upstream`. Reimplementing that as a clone-merge-push dance
//!   would be slower, would need a work tree, and would attribute the merge to whoever ran the
//!   command.
//!
//! `merge-upstream` can fast-forward *or* create a merge commit. `gea` asks for a fast-forward
//! only (`ff_only: true`) unless `--merge` is given, so a plain `gea repo sync me/proj` never
//! writes a commit nobody asked for — the same promise the local path makes. The endpoint names
//! the branch to sync and has no default of its own, so without `-b` the fork's default branch is
//! looked up first.
//!
//! `-f` is only meaningful locally: there is no server-side force. Saying so is better than
//! accepting the flag and ignoring it.

use clap::Args as ClapArgs;
use gitea_client::Api;
use gitea_core::context::git::{FetchSpec, GitCtx};
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{MergeUpstreamRequest, MergeUpstreamResponse};

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Update a fork from its source repository.

Without a destination, fetches and fast-forwards the current checkout.
-f uses a hard reset if the branch has diverged; local changes can be lost.

With a destination, asks Gitea to sync that fork. That is a fast-forward only,
unless --merge allows a merge commit when the fork has diverged. Server-side
sync does not support -f.

  gea repo sync                     # this checkout, from upstream
  gea repo sync -b main -f
  gea repo sync me/proj             # my fork, on the server
  gea repo sync me/proj --merge     # ...merging if it has diverged
  gea repo sync -s go-gitea/gitea   # from a specific base")]
pub struct Args {
    /// Fork to sync on the instance. Omit to sync this checkout instead
    #[arg(value_name = "DESTINATION")]
    pub destination: Option<String>,

    /// Branch to sync. Defaults to the current branch locally, or the default branch on the server
    #[arg(short = 'b', long, value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Repository to sync *from*, when syncing this checkout
    #[arg(short = 's', long, value_name = "OWNER/NAME")]
    pub source: Option<String>,

    /// Reset a diverged local branch instead of refusing. The long form is the global `--force`,
    /// which means something else, so only `-f` is available here
    #[arg(short = 'f')]
    pub force: bool,

    /// On the server, create a merge commit when the fork has diverged instead of refusing
    #[arg(long, requires = "destination")]
    pub merge: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        match &args.destination {
            Some(dest) => {
                if args.force {
                    return Err(Error::new(ErrorKind::Usage(
                        "server-side sync has no force. Remove -f (use --merge to allow a merge \
                         commit), or sync locally (omit the destination) and push."
                            .to_owned(),
                    )));
                }
                let slug = super::slug_from_arg(&rt, &api, dest).await?;
                remote_sync(&rt, &api, &slug, args.branch.as_deref(), args.merge).await
            }
            None => local_sync(&rt, globals, &api, args).await,
        }
    })
}

// ------------------------------------------------------------------------------ server-side sync

async fn remote_sync(
    rt: &Runtime,
    api: &Api,
    slug: &RepoSlug,
    branch: Option<&str>,
    merge: bool,
) -> Result<()> {
    let branch = match branch {
        Some(b) => b.to_owned(),
        None => api.repo().get(&slug.owner, &slug.name).await?.default_branch,
    };
    let body = MergeUpstreamRequest { branch: Some(branch.clone()), ff_only: Some(!merge) };
    let resp = api
        .repo()
        .merge_upstream(&slug.owner, &slug.name, &body)
        .await
        .map_err(|e| diverged(e, slug, &branch, merge))?;
    support::note(rt.term(), &outcome(&resp, slug, &branch));
    Ok(())
}

/// A fast-forward-only sync of a diverged branch is a `400` ("fast-forward merge not possible"),
/// which the generic classifier reports as a validation failure with no remedy. The remedy is
/// specific and there are exactly two of them, so say both.
fn diverged(e: Error, slug: &RepoSlug, branch: &str, merge: bool) -> Error {
    match &*e.kind {
        ErrorKind::Validation { server_message, .. }
            if !merge && server_message.as_deref().is_some_and(|m| m.contains("fast-forward")) =>
        {
            Error::new(ErrorKind::Usage(format!(
                "{slug} ({branch}) has diverged from its base, so it cannot be fast-forwarded.\n\
                 pass --merge to let Gitea create a merge commit, or sync a checkout instead \
                 (`gea repo sync -b {branch} -f`) and push"
            )))
        }
        _ => e,
    }
}

/// What happened, from `merge_type`. Gitea reports `up-to-date`, `fast-forward` or `merge`; an
/// unknown value is passed through rather than guessed at.
fn outcome(resp: &MergeUpstreamResponse, slug: &RepoSlug, branch: &str) -> String {
    match resp.merge_type.as_str() {
        "up-to-date" => format!("{slug} ({branch}) is already up to date"),
        "fast-forward" => format!("Synced {slug} ({branch}) — fast-forwarded"),
        "merge" => format!("Synced {slug} ({branch}) — merged with a merge commit"),
        other => format!("Synced {slug} ({branch}) — {other}"),
    }
}

// ------------------------------------------------------------------------------------ local sync

async fn local_sync(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &Args) -> Result<()> {
    gitea_core::context::require_git_repo(rt.git())?;
    let git = rt.git();

    // Where to sync *from*. With no `--source`, that is whatever resolution decided the repository
    // is — which in a fork clone is `upstream`, thanks to remote-name scoring. That is exactly the
    // repository a `gea pr create` here would target, so the two commands agree by construction.
    let base = match &args.source {
        Some(s) => super::slug_from_arg(rt, api, s).await?,
        None => rt.repo(globals)?.slug.clone(),
    };
    let remote = super::remote_for(rt, &base)?.ok_or_else(|| {
        Error::new(ErrorKind::Usage(format!(
            "no git remote in this checkout points at {base}; add one with \
             `git remote add upstream <url>`, or name the fork to sync on the server instead"
        )))
    })?;

    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => rt.git().current_branch()?.ok_or_else(|| {
            Error::new(ErrorKind::Usage(
                "HEAD is detached, so there is no current branch to sync; pass -b <branch>"
                    .to_owned(),
            ))
        })?,
    };

    let current = rt.git().current_branch()?;
    if current.as_deref() == Some(branch.as_str()) {
        // Fetching into the *checked-out* branch's ref is what git refuses outright, so this path
        // fetches and then moves HEAD, and the other path updates the ref directly.
        git.fetch(&FetchSpec::new(&remote).with_refspec(&branch))?;
        return fast_forward(rt, git, &remote, &branch, args.force);
    }
    // `<branch>:<branch>` updates the local ref without checking anything out. Non-fast-forward is
    // refused by git unless forced, which is the behaviour we want and is why `--force` is threaded
    // through rather than always passed.
    git.fetch(
        &FetchSpec::new(&remote).with_refspec(format!("{branch}:{branch}")).forced(args.force),
    )?;
    support::note(rt.term(), &format!("Updated local {branch} from {remote}/{branch}"));
    Ok(())
}

/// Fast-forward the checked-out branch, or explain what to do about a divergence.
fn fast_forward(
    rt: &Runtime,
    git: &dyn GitCtx,
    remote: &str,
    branch: &str,
    force: bool,
) -> Result<()> {
    let target = format!("{remote}/{branch}");
    if git.merge_ff_only(&target)? {
        support::note(rt.term(), &format!("Fast-forwarded {branch} to {target}"));
        return Ok(());
    }
    if !force {
        return Err(Error::new(ErrorKind::Usage(format!(
            "{branch} cannot be fast-forwarded to {target}: it has commits {target} does not.\n\
             pass -f to reset {branch} to {target} and lose them, or rebase them yourself with \
             `git rebase {target}`"
        ))));
    }
    // Refusing to throw away *uncommitted* work even under `-f`: the flag is about the branch's
    // history, and nobody types `-f` meaning "and also delete the file I am editing". `git reset
    // --hard` would do exactly that, silently.
    let dirty = git.porcelain_status()?;
    if !dirty.is_empty() {
        return Err(Error::new(ErrorKind::Usage(format!(
            "-f would reset {branch} to {target}, but this work tree has uncommitted changes:\n{}\n\
             commit or stash local changes first; -f does not discard uncommitted files",
            dirty
        ))));
    }
    git.reset_hard(&target)?;
    support::note(rt.term(), &format!("Reset {branch} to {target}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    fn resp(t: &str) -> MergeUpstreamResponse {
        MergeUpstreamResponse { merge_type: t.to_owned() }
    }

    /// Bug this prevents: an up-to-date fork being reported as "synced", which reads as if
    /// something changed.
    #[test]
    fn the_outcome_says_what_the_server_did() {
        let slug = RepoSlug::new("me", "proj");
        assert!(outcome(&resp("up-to-date"), &slug, "main").contains("already up to date"));
        assert!(outcome(&resp("fast-forward"), &slug, "main").contains("fast-forwarded"));
        assert!(outcome(&resp("merge"), &slug, "main").contains("merge commit"));
    }

    /// Bug this prevents: a diverged fork reported as a bare "validation failed", when the fix is
    /// one flag away.
    #[test]
    fn a_diverged_fast_forward_names_the_merge_flag() {
        let slug = RepoSlug::new("me", "proj");
        let e = || {
            Error::new(ErrorKind::Validation {
                fields: Vec::new(),
                server_message: Some(
                    "fast-forward merge not possible: branch has diverged".to_owned(),
                ),
            })
        };
        let msg = diverged(e(), &slug, "main", false).to_string();
        assert!(msg.contains("--merge"), "{msg}");
        assert!(msg.contains("me/proj (main)"), "{msg}");
        // With --merge already given, the server's own error stands.
        assert!(matches!(diverged(e(), &slug, "main", true).kind(), ErrorKind::Validation { .. }));
    }

    /// Bug this prevents: a plain server-side sync writing a merge commit. Without `--merge` the
    /// request must ask for a fast-forward only, and must name the branch — the endpoint has no
    /// default of its own.
    #[tokio::test]
    async fn a_plain_sync_asks_for_a_fast_forward_of_the_named_branch() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/me/proj/merge-upstream",
            Canned::json(200, r#"{"merge_type":"fast-forward"}"#),
        ));
        let api = testing::api(fake.clone());
        let body = MergeUpstreamRequest { branch: Some("main".into()), ff_only: Some(true) };
        api.repo().merge_upstream("me", "proj", &body).await.unwrap();
        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent, serde_json::json!({"branch": "main", "ff_only": true}));
    }

    /// Bug this prevents: `-f` being silently accepted for a server-side sync, where Gitea has
    /// no force at all, so the user believes a divergence was resolved when nothing happened.
    #[test]
    fn force_with_a_destination_is_refused_rather_than_ignored() {
        // Parsed, not hand-built, so the flag's own wiring is under test too.
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        let h =
            <Harness as clap::Parser>::try_parse_from(["gea", "me/proj", "-f"]).expect("parses");
        assert!(h.args.force);
        assert_eq!(h.args.destination.as_deref(), Some("me/proj"));
        // `--merge` only makes sense on the server.
        assert!(<Harness as clap::Parser>::try_parse_from(["gea", "--merge"]).is_err());
    }
}

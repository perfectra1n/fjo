//! `fjo repo` — repositories: create, clone, fork, view, list, edit, archive, sync.
//!
//! Every command here earns its place by one of the four tests in
//! `docs/porcelain-conventions.md`, and most of them by *orchestration*: `repo create --clone
//! --push` is an API call plus four `git` invocations, `repo fork --clone` is a fork plus a clone
//! plus a remote rename plus a `set-default`, and neither is something anybody should have to
//! type out of `fjo raw`.
//!
//! Two Forgejo-specific things live here and nowhere else:
//!
//! * **`create --mirror-from <url>`.** A *pull* mirror can only be established when the
//!   repository is created — `PATCH /repos/{owner}/{repo}` cannot turn an ordinary repository
//!   into one. So the flag belongs on `create`, and `fjo mirror` (a different group) covers push
//!   mirrors and syncing, which *are* editable afterwards.
//! * **`create --object-format sha256`,** likewise irreversible, and `--trust-model`.
//!
//! # Flag names that differ from `gh`, and why
//!
//! The global flags in [`crate::global`] are `clap` globals: they are propagated into every
//! subcommand, and clap answers a duplicate long or short name with a **panic**. So a layer-3
//! command may not declare `--template`, `--force`, `--color`, `--limit`, `--repo`, `--json`, …
//! nor the shorts `-R`, `-q`, `-t`. Where that collides with a `gh` name it is called out in the
//! flag's own help text, so the difference is discoverable at the point of use rather than only
//! in a changelog. `repo create --from-template` and `repo sync -f` are the two here.

pub mod archive;
pub mod clone;
pub mod create;
pub mod edit;
pub mod fork;
pub mod list;
pub mod set_default;
pub mod sync;
pub mod view;

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_core::context::git::GitCtx;
use forgejo_core::types::{RepoRef, RepoSlug};
use forgejo_core::{Error, ErrorKind, Result};

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(Debug, Subcommand)]
pub enum Sub {
    /// Create a new repository
    Create(create::Args),
    /// Clone a repository locally, wiring up `upstream` for a fork
    Clone(clone::Args),
    /// Fork a repository, and optionally clone it
    Fork(fork::Args),
    /// Show a repository, with its README on a terminal
    View(view::Args),
    /// List repositories for a user or organization
    List(list::Args),
    /// Delete a repository
    Delete(archive::DeleteArgs),
    /// Rename a repository
    Rename(edit::RenameArgs),
    /// Change a repository's settings
    Edit(edit::Args),
    /// Archive a repository, making it read-only
    Archive(archive::Args),
    /// Un-archive a repository
    Unarchive(archive::Args),
    /// Bring a fork's branch up to date with its upstream
    Sync(sync::Args),
    /// Record which remote `fjo` should treat as the repository
    #[command(name = "set-default")]
    SetDefault(set_default::Args),
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.cmd {
        Sub::Create(a) => create::run(globals, a),
        Sub::Clone(a) => clone::run(globals, a),
        Sub::Fork(a) => fork::run(globals, a),
        Sub::View(a) => view::run(globals, a),
        Sub::List(a) => list::run(globals, a),
        Sub::Delete(a) => archive::run_delete(globals, a),
        Sub::Rename(a) => edit::run_rename(globals, a),
        Sub::Edit(a) => edit::run(globals, a),
        Sub::Archive(a) => archive::run(globals, a, true),
        Sub::Unarchive(a) => archive::run(globals, a, false),
        Sub::Sync(a) => sync::run(globals, a),
        Sub::SetDefault(a) => set_default::run(globals, a),
    }
}

// --------------------------------------------------------------------------------- shared helpers

/// The repository a command is about: an argument if given, otherwise resolved context.
///
/// A bare `name` with no `/` is resolved against the authenticated user, which is what makes
/// `fjo repo view fjo` work — and is the one case that costs an extra request, so it is only
/// paid when the argument really has no owner.
pub async fn target(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    arg: Option<&str>,
) -> Result<RepoSlug> {
    let Some(arg) = arg else {
        return Ok(rt.repo(globals)?.slug.clone());
    };
    slug_from_arg(rt, api, arg).await
}

/// Parse a positional repository argument.
///
/// Accepts `owner/name`, `host/owner/name`, a URL, or a bare `name`. A host that is not the one
/// the client is pointed at is a usage error rather than a silent request to the wrong instance:
/// one `Client` speaks to one host, and quietly using the other one is exactly the class of bug
/// `RepoContext` exists to prevent.
pub async fn slug_from_arg(rt: &Runtime, api: &Api, arg: &str) -> Result<RepoSlug> {
    match arg.parse::<RepoRef>() {
        Ok(r) => {
            if let Some(host) = &r.host
                && !host.eq_ignore_ascii_case(rt.host().as_str())
            {
                return Err(Error::new(ErrorKind::Usage(format!(
                    "{arg} names host {host}, but this command is talking to {}; add \
                     --host {host}",
                    rt.host()
                ))));
            }
            Ok(r.slug)
        }
        // No `/` at all: a bare repository name in the user's own account.
        Err(_) if !arg.contains('/') && !arg.trim().is_empty() => {
            Ok(RepoSlug::new(support::me(api).await?, arg.trim()))
        }
        Err(e) => Err(Error::new(ErrorKind::Usage(format!("{e}")))),
    }
}

/// Split an `owner/name`, or return `None` for a bare name.
pub fn split_owner(arg: &str) -> Option<(String, String)> {
    let (owner, name) = arg.trim().split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| (owner.to_owned(), name.trim_end_matches(".git").to_owned()))
}

/// The name of the git remote that points at `slug`, if this checkout has one.
///
/// Used by `repo set-default`, `pr create --agit` and `pr checkout`: all three need to talk to the
/// *repository the command is about* through git, and guessing `origin` is wrong in exactly the
/// fork workflow those commands exist for.
pub fn remote_for(rt: &Runtime, slug: &RepoSlug) -> Result<Option<String>> {
    remote_in(rt.git(), rt.host(), slug)
}

/// [`remote_for`] against an explicit git and host, so it can be tested without a runtime.
///
/// When several remotes name the repository, the current branch's upstream wins over the first
/// one `git remote -v` lists. That order is alphabetical, and an `aaa` remote fetched once and
/// forgotten would otherwise be chosen over the `origin` the user pulls through — and its stale
/// remote-tracking refs are what `pr create --fill` once measured "my commits" against.
fn remote_in(
    git: &dyn GitCtx,
    host: &forgejo_core::config::HostKey,
    slug: &RepoSlug,
) -> Result<Option<String>> {
    use forgejo_core::context::remote_url;
    // A one-host key set: we only care whether the *path* names this repository, and which host
    // the command is about was already settled by resolution.
    let keys = [host.clone()];
    let wanted = slug.to_string();
    let names_it = |url: &str| {
        matches!(
            remote_url::resolve(url, &keys),
            remote_url::Resolution::Matched { slug: found, .. } if found.to_string() == wanted
        )
    };
    let matching: Vec<String> = git
        .remotes()?
        .into_iter()
        .filter(|remote| remote.urls().any(names_it))
        .map(|remote| remote.name)
        .collect();
    if matching.len() > 1
        && let Some(branch) = git.current_branch()?
        && let Some(upstream) = git.config_get(&format!("branch.{branch}.remote"))?
        && matching.contains(&upstream)
    {
        return Ok(Some(upstream));
    }
    Ok(matching.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_name_splits_and_a_bare_name_does_not() {
        assert_eq!(split_owner("me/proj"), Some(("me".to_owned(), "proj".to_owned())));
        assert_eq!(split_owner("me/proj.git"), Some(("me".to_owned(), "proj".to_owned())));
        assert_eq!(split_owner("proj"), None);
        assert_eq!(split_owner("a/b/c"), None);
    }

    fn host() -> forgejo_core::config::HostKey {
        forgejo_core::config::HostKey::parse("git.example.com").unwrap()
    }

    /// Bug this prevents (fjo#30, gea#2): two remotes on the same URL, and the alphabetically
    /// first one — fetched once, never again — chosen over the upstream the branch tracks.
    #[test]
    fn the_branchs_upstream_wins_when_several_remotes_name_the_repository() {
        let url = "https://git.example.com/me/proj.git";
        let slug: RepoSlug = "me/proj".parse().unwrap();
        let git = forgejo_core::context::git::FakeGit::repo()
            .with_remote("aaa", url)
            .with_remote("origin", url)
            .with_branch("topic")
            .with_config("branch.topic.remote", "origin");
        assert_eq!(remote_in(&git, &host(), &slug).unwrap().as_deref(), Some("origin"));
    }

    /// An upstream that is some *other* repository — the fork a topic branch is pushed to —
    /// says nothing about which remote names this one, so the first match still stands.
    #[test]
    fn an_upstream_on_another_repository_does_not_override_the_match() {
        let slug: RepoSlug = "me/proj".parse().unwrap();
        let git = forgejo_core::context::git::FakeGit::repo()
            .with_remote("fork", "https://git.example.com/you/proj.git")
            .with_remote("upstream", "https://git.example.com/me/proj.git")
            .with_branch("topic")
            .with_config("branch.topic.remote", "fork");
        assert_eq!(remote_in(&git, &host(), &slug).unwrap().as_deref(), Some("upstream"));
    }
}

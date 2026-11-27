//! `gea repo archive`, `unarchive`, and `delete` — the three that need a confirmation.
//!
//! All three are one API call, so what earns them a place in layer 3 is context inference plus the
//! confirmation: `gea raw repo delete o r` deletes a repository with no questions asked, and a
//! porcelain command that did the same would be a footgun with a friendlier name.
//!
//! Destructive on a terminal means *ask*; destructive without one means **require `--yes`**. Never
//! "proceed because nobody was watching".

use clap::Args as ClapArgs;
use gitea_core::Result;
use gitea_core::types::RepoSlug;

use super::edit::patch_repo;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Archive or unarchive a repository.

Archived repositories are read-only.

  gea repo archive
  gea repo archive them/proj --yes")]
pub struct Args {
    /// `owner/name`, a bare name in your own account, or a URL
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Delete a repository. This cannot be undone.

You will be asked to confirm. Without a terminal, --yes is required.

  gea repo delete old-experiment --yes")]
pub struct DeleteArgs {
    /// `owner/name`, a bare name in your own account, or a URL
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args, archived: bool) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::target(&rt, globals, &api, args.repo.as_deref()).await?;

        let (verb, question) = if archived {
            ("Archived", format!("Archive {slug}? It becomes read-only."))
        } else {
            ("Un-archived", format!("Un-archive {slug}?"))
        };
        support::confirm_question(support::can_prompt(&rt), args.yes, &question)?;

        // `archived` and nothing else: every other field of `EditRepoOption` stays `None` and is
        // skipped on serialisation, so the two dozen settings nobody mentioned are not rewritten.
        let body = gitea_model::EditRepoOption {
            archived: Some(archived),
            ..gitea_model::EditRepoOption::default()
        };
        patch_repo(&rt, &api, &slug, &body).await?;
        support::note(rt.term(), &format!("{verb} {slug}"));
        Ok(())
    })
}

pub fn run_delete(globals: &GlobalOpts, args: &DeleteArgs) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::target(&rt, globals, &api, args.repo.as_deref()).await?;
        confirm_delete(&rt, &slug, args.yes)?;
        api.repo().delete(&slug.owner, &slug.name).await?;
        support::note(rt.term(), &format!("Deleted {slug}"));
        Ok(())
    })
}

/// The delete confirmation, which asks for the repository's **name** rather than a yes.
///
/// A `y/N` prompt is muscle memory and gets a reflexive `y`; typing `owner/name` cannot be done by
/// reflex, and this is the one command in the group with no undo. `gh repo delete` does the same,
/// and for the same reason.
fn confirm_delete(rt: &Runtime, slug: &RepoSlug, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !support::can_prompt(rt) {
        return Err(gitea_core::Error::new(gitea_core::ErrorKind::Usage(format!(
            "deleting {slug} cannot be undone, and there is no terminal to confirm on; pass --yes"
        ))));
    }
    let typed = support::interact::ask(&format!("Type {slug} to confirm deletion"), None)?;
    if typed.trim() != slug.to_string() {
        return Err(gitea_core::Error::new(gitea_core::ErrorKind::Cancelled));
    }
    Ok(())
}

//! `gea pr close`, `reopen`, `ready`, `comment` and `edit`.
//!
//! # `ready` is a title edit
//!
//! Gitea has no draft field and no `ready` endpoint — a pull request is a draft when its title
//! carries a work-in-progress prefix (`WIP:` by default). So `gea pr ready` reads the title, strips
//! the prefix, and PATCHes it back, and `--undo` puts it on. Both are one call and would be a
//! two-step guessing game by hand, which is exactly what a porcelain command is for.
//!
//! # `edit` builds its own body, and mutates rather than replaces
//!
//! Two reasons, and both are load-bearing:
//!
//! 1. [`gitea_model::EditPullRequestOption`] cannot express an absent field — the same defect
//!    documented at length in [`crate::cmd::repo::edit`]. Worse here than there, because its
//!    `labels: Vec<i64>` serialises to `[]`, and Gitea reads a present-but-empty label list as
//!    "remove every label".
//! 2. `--add-label`/`--remove-label` rather than `--label`, because the API replaces the whole set.
//!    An edit with replace-semantics silently discards labels somebody else added between your read
//!    and your write; the add/remove pair reads the current set first and sends the union.

use clap::Args as ClapArgs;
use gitea_client::Api;
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result, http::Request, http::encode};
use gitea_model::{CreateIssueCommentOption, PullRequest};
use serde_json::{Map, Value};

use super::common;
use crate::cmd::support::{self, BodyOpts};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

// ------------------------------------------------------------------------------- close and reopen

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Close a pull request without merging.

Use -c/--comment to add a comment before closing.

  gea pr close
  gea pr close 42 -c 'superseded by #45'
  gea pr close 42 -d")]
pub struct CloseArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Leave this comment before closing
    #[arg(short = 'c', long, value_name = "TEXT")]
    pub comment: Option<String>,

    /// Delete the head branch as well
    #[arg(short = 'd', long)]
    pub delete_branch: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Reopen a closed pull request.

Merged pull requests cannot be reopened.

  gea pr reopen 42")]
pub struct ReopenArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,
}

pub fn run_close(globals: &GlobalOpts, args: &CloseArgs) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        if found.pr.merged {
            return Err(Error::new(ErrorKind::Usage(format!(
                "#{} is merged, so there is nothing to close",
                found.pr.number
            ))));
        }
        // The comment goes first, so a failure to close does not leave an explanation for something
        // that is still open — and so the explanation is never lost to a failed close.
        if let Some(text) = &args.comment {
            let body = CreateIssueCommentOption { body: text.clone() };
            api.issue()
                .create_comment(&found.slug.owner, &found.slug.name, found.index(), &body)
                .await?;
        }
        patch(&rt, &found.slug, found.index(), serde_json::json!({ "state": "closed" })).await?;
        support::note(rt.term(), &format!("Closed #{}", found.pr.number));

        if args.delete_branch {
            delete_head_branch(&rt, &api, &found.slug, &found.pr).await;
        }
        Ok(())
    })
}

pub fn run_reopen(globals: &GlobalOpts, args: &ReopenArgs) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        if found.pr.merged {
            return Err(Error::new(ErrorKind::Usage(format!(
                "#{} was merged, and a merged pull request cannot be reopened; open a new one",
                found.pr.number
            ))));
        }
        patch(&rt, &found.slug, found.index(), serde_json::json!({ "state": "open" })).await?;
        support::note(rt.term(), &format!("Reopened #{}", found.pr.number));
        Ok(())
    })
}

/// Delete the head branch on the server after closing. Best-effort and quiet.
///
/// An AGit pull request has no branch, and a fork's branch is not ours to delete — so both are
/// skipped with a note rather than producing a 403 that looks like a failure of the close.
async fn delete_head_branch(rt: &Runtime, api: &Api, slug: &RepoSlug, pr: &PullRequest) {
    if common::is_agit(pr) {
        support::note(rt.term(), "no head branch to delete: this pull request arrived over AGit");
        return;
    }
    let Some(head) = pr.head.as_ref() else { return };
    let in_base = head.repo.as_ref().is_some_and(|r| r.full_name == slug.to_string());
    if !in_base {
        support::note(rt.term(), "the head branch is in another repository, so it was left alone");
        return;
    }
    match api.repo().delete_branch(&slug.owner, &slug.name, &head.r#ref).await {
        Ok(()) => support::note(rt.term(), &format!("deleted branch {}", head.r#ref)),
        Err(e) => support::note(rt.term(), &format!("could not delete branch {}: {e}", head.r#ref)),
    }
}

// ------------------------------------------------------------------------------------------ ready

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Mark a pull request as ready for review.

Removes the work-in-progress title prefix. --undo restores it.

  gea pr ready
  gea pr ready 42 --undo")]
pub struct ReadyArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Mark it as a draft again instead
    #[arg(long)]
    pub undo: bool,
}

pub fn run_ready(globals: &GlobalOpts, args: &ReadyArgs) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        let title = if args.undo {
            common::add_wip(&found.pr.title)
        } else {
            common::strip_wip(&found.pr.title)
        };
        if title == found.pr.title {
            support::note(
                rt.term(),
                &format!(
                    "#{} is already {}",
                    found.pr.number,
                    if args.undo { "a draft" } else { "ready for review" }
                ),
            );
            return Ok(());
        }
        patch(&rt, &found.slug, found.index(), serde_json::json!({ "title": title })).await?;
        support::note(
            rt.term(),
            &format!(
                "#{} is now {}",
                found.pr.number,
                if args.undo { "a draft" } else { "ready for review" }
            ),
        );
        Ok(())
    })
}

// ---------------------------------------------------------------------------------------- comment

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Add a comment to a pull request.

Use -b for text, -F for a file (- for stdin), or -e to open an editor.

  gea pr comment -b 'rebased onto main'
  gea pr comment 42 -F notes.md
  gea pr comment 42 -e")]
pub struct CommentArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    #[command(flatten)]
    pub body: BodyOpts,
}

pub fn run_comment(globals: &GlobalOpts, args: &CommentArgs) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_COMMENT)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        let supplied = args.body.resolve(&rt, "")?;
        let body = match supplied.body {
            Some(b) if !b.trim().is_empty() => b,
            _ if support::can_prompt(&rt) => support::interact::ask("Comment", None)?,
            _ => return Err(support::missing("-b/--body or -F/--body-file", "a comment body")),
        };
        if body.trim().is_empty() {
            return Err(Error::new(ErrorKind::Cancelled));
        }

        let option = CreateIssueCommentOption { body };
        let comment = api
            .issue()
            .create_comment(&found.slug.owner, &found.slug.name, found.index(), &option)
            .await?;

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&comment)?)
            }
            _ => {
                println!("{}", comment.html_url);
                Ok(())
            }
        }
    })
}

// ------------------------------------------------------------------------------------------- edit

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Edit a pull request's title, body, base branch, labels, or assignees.

Use --add-label/--remove-label and --add-assignee/--remove-assignee to change sets.
Use --title for titles; -t is reserved for output templates.

  gea pr edit --title 'Teach the parser about tabs'
  gea pr edit 42 --add-label bug --remove-label needs-triage
  gea pr edit 42 -B main
  gea pr edit 42 -e")]
pub struct EditArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// New title. There is no `-t`: that is the global `--template`
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    #[command(flatten)]
    pub body: BodyOpts,

    /// Retarget the pull request at this branch
    #[arg(short = 'B', long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Add a label by name. Repeatable
    #[arg(long, value_name = "NAME")]
    pub add_label: Vec<String>,
    /// Remove a label by name. Repeatable
    #[arg(long, value_name = "NAME")]
    pub remove_label: Vec<String>,

    /// Add an assignee. Repeatable; `@me` is you
    #[arg(long, value_name = "USER")]
    pub add_assignee: Vec<String>,
    /// Remove an assignee. Repeatable; `@me` is you
    #[arg(long, value_name = "USER")]
    pub remove_assignee: Vec<String>,

    /// Milestone, by title. An empty value clears it
    #[arg(short = 'm', long, value_name = "TITLE")]
    pub milestone: Option<String>,
}

pub fn run_edit(globals: &GlobalOpts, args: &EditArgs) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        // Seeded with the current body, so `-e` opens what is there rather than a blank buffer —
        // an editor that discards the existing description is a trap, not a convenience.
        let supplied = args.body.resolve(&rt, &found.pr.body)?;
        let mut body = Map::new();
        if let Some(title) = args.title.clone().or(supplied.title) {
            body.insert("title".to_owned(), Value::String(title));
        }
        if let Some(text) = supplied.body {
            body.insert("body".to_owned(), Value::String(text));
        }
        if let Some(base) = &args.base {
            body.insert("base".to_owned(), Value::String(base.clone()));
        }
        // The milestone title, the label names and the `@me` assignees are three independent
        // read-only lookups made before the patch is sent, so they run at the same time.
        //
        // `join!` with a fixed unwrap order rather than `try_join!`: today a bad milestone title
        // always beats a bad label name because the milestone was looked up first, and
        // `try_join!` returns the first error to *occur* — so `-m nope -l nope` would report a
        // different problem depending on which response arrived first.
        //
        // The `--add-assignee @me --remove-assignee @me` pair is joined too: it was two identical
        // `GET /user` requests, one after the other, for one answer.
        let resolve_milestone = async {
            match &args.milestone {
                None => Ok(None),
                Some(title) if title.trim().is_empty() => Ok(Some(0)),
                Some(title) => common::milestone_id(&api, &found.slug, title).await.map(Some),
            }
        };
        let resolve_labels = async {
            if args.add_label.is_empty() && args.remove_label.is_empty() {
                return Ok(None);
            }
            let names = mutate(
                found.pr.labels.iter().map(|l| l.name.clone()).collect(),
                &args.add_label,
                &args.remove_label,
            );
            common::label_ids(&api, &found.slug, &names).await.map(Some)
        };
        let resolve_assignees = async {
            if args.add_assignee.is_empty() && args.remove_assignee.is_empty() {
                return Ok::<_, Error>(None);
            }
            let (add, remove) = futures::join!(
                support::resolve_me(&api, &args.add_assignee),
                support::resolve_me(&api, &args.remove_assignee)
            );
            let names = mutate(
                found.pr.assignees.iter().map(|u| u.login.clone()).collect(),
                &add?,
                &remove?,
            );
            Ok(Some(names))
        };
        let resolved = futures::join!(resolve_milestone, resolve_labels, resolve_assignees);

        // Inserted in the order the serial version inserted them, so the patch body is
        // byte-identical.
        if let Some(id) = resolved.0? {
            body.insert("milestone".to_owned(), Value::from(id));
        }
        if let Some(ids) = resolved.1? {
            body.insert("labels".to_owned(), Value::from(ids));
        }
        if let Some(names) = resolved.2? {
            body.insert("assignees".to_owned(), Value::from(names));
        }

        if body.is_empty() {
            return Err(Error::new(ErrorKind::Usage(
                "no changes specified; see `gea pr edit --help` for settings".to_owned(),
            )));
        }

        let updated = patch(&rt, &found.slug, found.index(), Value::Object(body)).await?;
        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&updated)?)
            }
            _ => {
                support::note(rt.term(), &format!("Updated #{}", updated.number));
                println!("{}", updated.html_url);
                Ok(())
            }
        }
    })
}

/// The `--add-X` / `--remove-X` set operation, order-preserving and case-insensitive.
///
/// Order is preserved so an edit does not reshuffle a list for no reason, and comparison is
/// case-insensitive because Gitea treats logins and label names that way — `--remove-assignee
/// Alice` must remove `alice`.
pub(crate) fn mutate(current: Vec<String>, add: &[String], remove: &[String]) -> Vec<String> {
    let mut out = current;
    out.retain(|existing| !remove.iter().any(|r| r.eq_ignore_ascii_case(existing)));
    for name in add {
        if !out.iter().any(|existing| existing.eq_ignore_ascii_case(name)) {
            out.push(name.clone());
        }
    }
    out
}

/// `PATCH /repos/{owner}/{repo}/pulls/{index}` with a body we built. See the module docs.
async fn patch(rt: &Runtime, slug: &RepoSlug, index: i64, body: Value) -> Result<PullRequest> {
    let path =
        format!("/repos/{}/{}/pulls/{}", encode::seg(&slug.owner), encode::seg(&slug.name), index);
    rt.trace(&format!("PATCH {path} {body}"));
    rt.client().json(Request::patch(path).json_body(&body)?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `labels: []` on the wire means "remove every label", so a title-only edit that sent one
    /// would strip the labels. The generated model no longer sends it: a not-required
    /// request-body field is `Option<T>` with `skip_serializing_if`.
    ///
    /// Kept as a regression test — this is the shape of the bug, and it is invisible at the
    /// call site.
    #[test]
    fn a_title_only_pull_request_edit_touches_nothing_else() {
        let model = gitea_model::EditPullRequestOption {
            title: Some("new title".to_owned()),
            ..gitea_model::EditPullRequestOption::default()
        };
        let sent = serde_json::to_value(&model).expect("serialisable");
        assert_eq!(sent, serde_json::json!({"title": "new title"}));
    }

    /// Bug this prevents: replace-semantics on an edit. `--add-label bug` must not discard the
    /// `needs-review` label a colleague added thirty seconds ago.
    #[test]
    fn add_and_remove_mutate_the_existing_set() {
        let current = vec!["needs-review".to_owned(), "bug".to_owned()];
        assert_eq!(
            mutate(current.clone(), &["docs".to_owned()], &[]),
            ["needs-review", "bug", "docs"]
        );
        assert_eq!(mutate(current.clone(), &[], &["bug".to_owned()]), ["needs-review"]);
        // Case-insensitive, as Gitea compares them.
        assert_eq!(mutate(current.clone(), &[], &["BUG".to_owned()]), ["needs-review"]);
        // Idempotent: adding what is already there must not duplicate it.
        assert_eq!(mutate(current.clone(), &["bug".to_owned()], &[]), ["needs-review", "bug"]);
    }

    /// `ready` is a title edit, in both directions, and must be a no-op when nothing would change —
    /// a PATCH that rewrites the title to itself still bumps `updated_at` and emails watchers.
    #[test]
    fn ready_and_undo_are_title_edits_and_no_ops_when_nothing_changes() {
        assert_eq!(common::strip_wip("WIP: Add the thing"), "Add the thing");
        assert_eq!(common::strip_wip("Add the thing"), "Add the thing");
        assert_eq!(common::add_wip("Add the thing"), "WIP: Add the thing");
        assert_eq!(common::add_wip("WIP: Add the thing"), "WIP: Add the thing");
    }
}

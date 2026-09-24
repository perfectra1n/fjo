//! The `gea issue` verbs.
//!
//! Split from the argument definitions so that the clap surface can be read in one screen and
//! reviewed against `docs/porcelain-conventions.md` without scrolling past request assembly.
//!
//! Two behaviours here are load-bearing and easy to regress:
//!
//! * **`list` sends `type=issues`.** `GET /repos/{o}/{r}/issues` returns pull requests as well
//!   as issues, so without it `gea issue list` shows pull requests — and then
//!   `gea issue close` on one of them mangles a pull request.
//! * **`edit` adds and removes labels through the label endpoints, never through the issue
//!   body.** `PUT /issues/{i}/labels` replaces the whole set, so an edit that used it would
//!   silently discard a label somebody else added between the read and the write.

use std::io::Write as _;

use futures::StreamExt;
use gitea_client::fields;
use gitea_client::query::{
    IssueGetCommentsQuery, IssueListBlocksQuery, IssueListIssueDependenciesQuery,
    IssueListIssuesQuery,
};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::encode;
use gitea_core::types::ids::{CommentId, IssueIndex};
use gitea_model::{Comment, Issue};

use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

use super::shared::{self, BodyFlags, Cx, IssuePatch, Out, label_chip, markdown, timeago};
use super::{
    Args, Cmd, CommentArgs, CreateArgs, DeleteArgs, DependsArgs, DependsCmd, EditArgs, ListArgs,
    PinArgs, TargetArgs, ViewArgs,
};
use crate::cmd::support;

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // The field table depends on what the verb answers with, and `--json` discovery has to be
    // able to answer before anything is built. `comment` produces a Comment; everything that
    // produces output at all produces an Issue.
    let table = match &args.cmd {
        Cmd::Comment(_) => fields::FIELDS_COMMENT,
        _ => fields::FIELDS_ISSUE,
    };
    let Some(out) = Out::prepare(globals, table)? else { return Ok(()) };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let cx = Cx::in_repo(&rt, globals, out)?;
        match &args.cmd {
            Cmd::Create(a) => create(&cx, a).await,
            Cmd::List(a) => list(&cx, globals, a).await,
            Cmd::View(a) => view(&cx, a).await,
            Cmd::Close(a) => close(&cx, &a.target, a.comment.as_deref()).await,
            Cmd::Reopen(a) => reopen(&cx, a).await,
            Cmd::Comment(a) => comment(&cx, a).await,
            Cmd::Edit(a) => edit(&cx, a).await,
            Cmd::Delete(a) => delete(&cx, a).await,
            Cmd::Pin(a) => pin(&cx, a).await,
            Cmd::Unpin(a) => unpin(&cx, a).await,
            Cmd::Depends(DependsCmd::List(a)) => depends_list(&cx, a).await,
            Cmd::Depends(DependsCmd::Add(a)) => depends_write(&cx, a, true).await,
            Cmd::Depends(DependsCmd::Remove(a)) => depends_write(&cx, a, false).await,
        }
    })
}

// -------------------------------------------------------------------------------- create

/// What `issue create` decided to send, and what it saves on failure.
///
/// Serialisable so that a create that fails after the user typed a long body can be recovered
/// with `--recover`. Losing a paragraph of prose to a 502 is the single most annoying failure
/// an issue tracker CLI has.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Draft {
    title: String,
    body: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignees: Vec<String>,
    #[serde(default)]
    milestone: Option<String>,
}

async fn create(cx: &Cx, a: &CreateArgs) -> Result<()> {
    if a.web {
        let mut url = format!("{}/{}/issues/new", cx.web, cx.repo()?);
        let query = encode::query_string(
            [
                ("title", a.title.as_deref().unwrap_or("")),
                ("body", a.body.as_deref().unwrap_or("")),
            ]
            .into_iter()
            .filter(|(_, v)| !v.is_empty()),
        );
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        return cx.browse(&url);
    }

    let mut draft = Draft {
        labels: a.label.clone(),
        assignees: a.assignee.clone(),
        milestone: a.milestone.clone(),
        ..Draft::default()
    };

    // `--recover` first, so a template and explicit flags can still override the recovered
    // draft rather than being overridden by it.
    if let Some(path) = &a.recover {
        let text = std::fs::read_to_string(path)
            .map_err(|e| support::usage(format!("--recover {}: {e}", path.display())))?;
        let saved: Draft = serde_json::from_str(&text).map_err(|e| {
            support::usage(format!("--recover {}: not a draft gea wrote ({e})", path.display()))
        })?;
        draft = Draft {
            labels: if a.label.is_empty() { saved.labels } else { a.label.clone() },
            assignees: if a.assignee.is_empty() { saved.assignees } else { a.assignee.clone() },
            milestone: a.milestone.clone().or(saved.milestone),
            title: saved.title,
            body: saved.body,
        };
    }

    if let Some(name) = &a.template {
        let chosen = pick_template(cx, name).await?;
        if draft.title.is_empty() {
            draft.title = chosen.title;
        }
        if draft.body.is_empty() {
            draft.body = chosen.content;
        }
        for label in chosen.labels {
            if !draft.labels.iter().any(|l| l.eq_ignore_ascii_case(&label)) {
                draft.labels.push(label);
            }
        }
    }

    if let Some(body) =
        (BodyFlags { body: a.body.as_deref(), body_file: a.body_file.as_deref(), editor: a.editor })
            .read(&mut std::io::stdin())?
    {
        draft.body = body;
    }
    if let Some(title) = &a.title {
        draft.title = title.clone();
    }

    if a.editor {
        // The first line is the title. Seeded with whatever we already have so that
        // `-t 'x' -e` opens an editor with `x` on line one rather than an empty buffer.
        let text = cx.edit_text(&support::editor_seed(&draft.title, &draft.body))?;
        let (title, body) = support::split_editor_text(&text).parts();
        draft.title = title;
        draft.body = body;
    }

    if draft.title.trim().is_empty() {
        if !cx.can_prompt() {
            return Err(support::usage(
                "an issue needs a title; pass --title, or -e/--editor to write one (the first \
                 line is the title)",
            ));
        }
        draft.title = cx.ask("Title")?;
        if draft.title.trim().is_empty() {
            return Err(support::usage("an issue needs a title"));
        }
    }

    // Named labels and a named milestone are resolved before anything is written, so a typo is
    // a validation error naming the labels that do exist rather than a half-created issue.
    //
    // All three are read-only lookups that depend on nothing but the command line, so they run
    // at the same time. `join!` with a fixed unwrap order rather than `try_join!`, and that is a
    // correctness choice: today a bad label name always beats a bad milestone title because the
    // labels were looked up first, and `try_join!` returns the first error to *occur* — so
    // `-l nope -m nope` would report a different problem depending on which response the network
    // delivered first.
    let resolve_labels = shared::label_ids(&cx.api, cx.repo()?, &draft.labels);
    let resolve_milestone = async {
        match &draft.milestone {
            Some(title) => shared::milestone_by_title(cx, title).await.map(Some),
            None => Ok(None),
        }
    };
    let resolve_assignees = cx.resolve_users(&draft.assignees);
    let resolved = futures::join!(resolve_labels, resolve_milestone, resolve_assignees);
    let label_ids = resolved.0?;
    let milestone = resolved.1?;
    let assignees = resolved.2?;

    let mut body = gitea_model::CreateIssueOption {
        title: draft.title.clone(),
        body: Some(draft.body.clone()),
        assignees: Some(assignees.clone()),
        labels: Some(label_ids),
        ..gitea_model::CreateIssueOption::default()
    };
    if let Some(m) = &milestone {
        body.milestone = Some(m.id.get());
    }

    if a.dry_run {
        // Reads happened (labels and the milestone were validated); nothing was written.
        let mut text = format!("Would create in {}:\n", cx.repo()?);
        text.push_str(&format!("  title      {}\n", body.title));
        text.push_str(&format!("  labels     {}\n", or_dash(&draft.labels.join(", "))));
        text.push_str(&format!("  assignees  {}\n", or_dash(&assignees.join(", "))));
        text.push_str(&format!(
            "  milestone  {}\n",
            or_dash(milestone.as_ref().map(|m| m.title.as_str()).unwrap_or(""))
        ));
        text.push_str(&format!(
            "  body       {} byte(s)\n",
            body.body.as_deref().unwrap_or_default().len()
        ));
        return cx.out.text(&text);
    }

    let issue = match cx.api.issue().create_issue(cx.owner()?, cx.name()?, &body).await {
        Ok(i) => i,
        Err(e) => {
            // Never swallow the server's reason; add the draft path and re-raise.
            if let Some(path) = save_draft(&draft) {
                eprintln!(
                    "the issue was not created; your draft is at {} — retry with `gea issue \
                     create --recover {}`",
                    path.display(),
                    path.display()
                );
            }
            return Err(e);
        }
    };
    emit_issue(cx, &issue, "Created")
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "—" } else { s }
}

/// Save a draft next to the temporary directory, returning where.
///
/// Best effort: a failure to save is reported by *not* mentioning a path, never by replacing
/// the server's error with a filesystem one.
fn save_draft(draft: &Draft) -> Option<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("gea-issue-draft-{}.json", std::process::id()));
    let json = serde_json::to_vec_pretty(draft).ok()?;
    std::fs::write(&path, json).ok()?;
    Some(path)
}

async fn pick_template(cx: &Cx, name: &str) -> Result<gitea_model::IssueTemplate> {
    let templates = cx.api.repo().get_issue_templates(cx.owner()?, cx.name()?).await?;
    let hit = templates
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name) || t.file_name.eq_ignore_ascii_case(name));
    match hit {
        Some(t) => Ok(t.clone()),
        None => {
            let mut names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
            names.sort_unstable();
            Err(support::usage(format!(
                "{} has no issue template called {name:?}; it has: {}",
                cx.repo()?,
                if names.is_empty() { "none".to_owned() } else { names.join(", ") }
            )))
        }
    }
}

// ---------------------------------------------------------------------------------- list

async fn list(cx: &Cx, globals: &GlobalOpts, a: &ListArgs) -> Result<()> {
    if a.web {
        let query = encode::query_string([("state", a.state.as_str())]);
        return cx.browse(&format!("{}/{}/issues?{query}", cx.web, cx.repo()?));
    }

    // `type=issues`: this endpoint answers with pull requests too, and a pull request that
    // arrives in `gea issue list` is one `gea issue close` away from real damage.
    let mut query =
        IssueListIssuesQuery::default().with_state(a.state.as_str()).with_type("issues");
    if !a.label.is_empty() {
        query = query.with_labels(&a.label.join(","));
    }
    if let Some(m) = &a.milestone {
        // The API takes names and falls back to ids, so a title goes straight through — and
        // unlike `create`, a missing milestone here means "no matches", not an error.
        query = query.with_milestones(m);
    }
    if let Some(user) = &a.assignee {
        query = query.with_assigned_by(&cx.resolve_user(user).await?);
    }
    if let Some(user) = &a.author {
        query = query.with_created_by(&cx.resolve_user(user).await?);
    }
    if let Some(q) = &a.search {
        query = query.with_q(q);
    }

    // `--paginate` with no explicit cap means "all of them"; otherwise 30, or `-L`/`--limit`.
    let cap = if globals.paginate && a.limit.is_none() && globals.limit.is_none() {
        usize::MAX
    } else {
        support::limit(a.limit, globals)
    };

    // The typed stream, capped with `take`. `ItemStream` follows the collection properly —
    // terminating on the `Link` header rather than on a short page — which matters because
    // Gitea silently clamps `limit` to `max_response_items`, so `-L 100` on an instance whose
    // page size is 50 would otherwise stop at 50 and exit 0. See
    // `gitea_core::http::paginate`.
    let mut stream = cx.api.issue().list_issues(cx.owner()?, cx.name()?, &query).take(cap);

    let mut issues: Vec<Issue> = Vec::new();
    while let Some(item) = stream.next().await {
        issues.push(item?);
    }
    cx.trace(&format!("{} issue(s) after a cap of {cap}", issues.len()));

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&issues)?, &cx.term);
    }
    if issues.is_empty() {
        support::note(&cx.term, &format!("no {} issues in {}", a.state.as_str(), cx.repo()?));
        // Still exit 0 with an empty table: emptiness is not an error.
        return cx.out.table(&Table::new(&cx.term));
    }

    // The banner's one job is to admit what it withheld, and a capped `ItemStream` cannot say:
    // it stopped at `cap` and never reported how many were behind it. Scoped so the borrow of
    // `cx.api` ends before the table is written.
    let total = {
        let (owner, name) = (cx.owner()?, cx.name()?);
        let ops = cx.api.issue();
        support::total_if_truncated(issues.len(), cap, |p| {
            ops.list_issues_page(owner, name, &query, p)
        })
        .await
    };

    let mut table = Table::new(&cx.term);
    table.headers(["NUMBER", "STATE", "TITLE", "LABELS", "UPDATED"]);
    for issue in &issues {
        table.row([
            format!("#{}", issue.number),
            issue.state.to_string(),
            issue.title.clone(),
            issue.labels.iter().map(|l| label_chip(&cx.term, l)).collect::<Vec<_>>().join(", "),
            timeago(issue.updated_at),
        ]);
    }
    let n = total.unwrap_or(issues.len() as u64);
    table.banner(support::banner(
        issues.len(),
        total,
        &format!("{} issue{} in {}", a.state.as_str(), support::plural_s(n), cx.repo()?),
    ));
    cx.out.table(&table)
}

// ---------------------------------------------------------------------------------- view

async fn view(cx: &Cx, a: &ViewArgs) -> Result<()> {
    // The comments are a second read of the same index, independent of the issue itself, so the
    // two overlap into one round trip's worth of waiting instead of two.
    //
    // Gated on all three flags, not just `-c`: `--web` returns below without rendering anything
    // and `--json` emits the issue alone, so fetching comments for either would buy a slowdown
    // for output nobody reads. That is the guard `gea pr view` was missing.
    let (issue, comments) = if a.comments && !a.web && !cx.out.is_machine() {
        let both = futures::join!(
            shared::get_issue(cx, a.target.number),
            all_comments(cx, a.target.number)
        );
        // Unwrapped in the order the two requests used to run in, rather than through
        // `try_join!`: an issue that 404s must still report the issue's error and not whichever
        // arm the network happened to fail first.
        (both.0?, both.1?)
    } else {
        (shared::get_issue(cx, a.target.number).await?, Vec::new())
    };
    if a.web {
        return cx.browse(&issue.html_url);
    }
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&issue)?, &cx.term);
    }

    let mut text = String::new();
    text.push_str(&format!("{} #{}\n", issue.title, issue.number));
    // Assembled as facts and joined, so a server that omits `created_at` yields
    // "open • opened by alice • 1 comment" rather than "opened —".
    let mut facts = Vec::from([issue.state.to_string()]);
    facts.push(match (&issue.user, issue.created_at.filter(|t| !t.is_unset())) {
        (Some(u), Some(t)) => format!("opened by {} {}", u.login, timeago(Some(t))),
        (Some(u), None) => format!("opened by {}", u.login),
        (None, Some(t)) => format!("opened {}", timeago(Some(t))),
        (None, None) => "opened".to_owned(),
    });
    facts.push(format!("{} comment{}", issue.comments, if issue.comments == 1 { "" } else { "s" }));
    text.push_str(&format!("{}\n", facts.join(" • ")));
    if !issue.labels.is_empty() {
        let labels: Vec<String> = issue.labels.iter().map(|l| label_chip(&cx.term, l)).collect();
        text.push_str(&format!("Labels: {}\n", labels.join(", ")));
    }
    if let Some(m) = &issue.milestone {
        text.push_str(&format!("Milestone: {}\n", m.title));
    }
    if !issue.assignees.is_empty() {
        let who: Vec<&str> = issue.assignees.iter().map(|u| u.login.as_str()).collect();
        text.push_str(&format!("Assignees: {}\n", who.join(", ")));
    }
    if let Some(due) = issue.due_date.filter(|t| !t.is_unset()) {
        text.push_str(&format!("Due: {}\n", shared::date(Some(due))));
    }
    if issue.pin_order > 0 {
        text.push_str(&format!("Pinned at position {}\n", issue.pin_order));
    }
    text.push('\n');
    if issue.body.trim().is_empty() {
        text.push_str("No description provided.\n");
    } else {
        text.push_str(&markdown(&cx.term, &issue.body));
    }

    if a.comments {
        for c in &comments {
            text.push_str(&format!(
                "\n─── {} commented {} (comment {})\n",
                c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("someone"),
                timeago(c.created_at),
                c.id
            ));
            text.push_str(&markdown(&cx.term, &c.body));
        }
        if comments.is_empty() {
            text.push_str("\nNo comments.\n");
        }
    }

    text.push_str(&format!("\n{}\n", issue.html_url));
    cx.out.text(&text)
}

async fn all_comments(cx: &Cx, index: IssueIndex) -> Result<Vec<Comment>> {
    let q = IssueGetCommentsQuery::default();
    // A single-page endpoint in the specification, so no walk to do.
    cx.api.issue().get_comments(cx.owner()?, cx.name()?, index.get(), &q).await
}

// ------------------------------------------------------------------- close, reopen, delete

async fn close(cx: &Cx, target: &TargetArgs, comment_text: Option<&str>) -> Result<()> {
    // The comment goes first: closing and *then* failing to explain why is the worse order.
    if let Some(text) = comment_text {
        let body = gitea_model::CreateIssueCommentOption { body: text.to_owned() };
        cx.api.issue().create_comment(cx.owner()?, cx.name()?, target.number.get(), &body).await?;
    }
    let patch = IssuePatch { state: Some("closed".to_owned()), ..IssuePatch::default() };
    let issue = shared::patch_issue(cx, target.number, &patch).await?;
    emit_issue(cx, &issue, "Closed")
}

async fn reopen(cx: &Cx, a: &TargetArgs) -> Result<()> {
    let patch = IssuePatch { state: Some("open".to_owned()), ..IssuePatch::default() };
    let issue = shared::patch_issue(cx, a.number, &patch).await?;
    emit_issue(cx, &issue, "Reopened")
}

async fn delete(cx: &Cx, a: &DeleteArgs) -> Result<()> {
    let issue = shared::get_issue(cx, a.target.number).await?;
    cx.confirm(
        &format!("Delete issue #{} ({}) from {}", issue.number, issue.title, cx.repo()?),
        a.yes,
    )?;
    cx.api.issue().delete(cx.owner()?, cx.name()?, a.target.number.get()).await?;
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&issue)?, &cx.term);
    }
    cx.out.text(&format!("Deleted issue #{} ({})\n", issue.number, issue.title))
}

// -------------------------------------------------------------------------------- comment

async fn comment(cx: &Cx, a: &CommentArgs) -> Result<()> {
    if a.web {
        let issue = shared::get_issue(cx, a.target.number).await?;
        return cx.browse(&issue.html_url);
    }

    let flags =
        BodyFlags { body: a.body.as_deref(), body_file: a.body_file.as_deref(), editor: a.editor };
    let mut body = flags.read(&mut std::io::stdin())?.unwrap_or_default();

    // Editing prefills the editor with what is there now, so `--edit <id> -e` is an amend
    // rather than a blind overwrite.
    if a.editor {
        let seed = match a.edit {
            Some(id) => existing_comment(cx, id).await?.body,
            None => body.clone(),
        };
        body = cx.edit_text(&seed)?;
    }
    if body.trim().is_empty() {
        if !cx.can_prompt() {
            return Err(support::usage(
                "a comment needs a body; pass -b/--body, -F/--body-file, or -e/--editor",
            ));
        }
        body = cx.ask("Comment")?;
        if body.trim().is_empty() {
            return Err(support::usage("a comment needs a body"));
        }
    }

    let comment = match a.edit {
        // A comment id, **not** an issue index: this is `/issues/comments/{id}`, a different
        // route from `/issues/{index}`. Passing one where the other belongs edits a real
        // comment on some other issue.
        Some(id) => {
            let option = gitea_model::EditIssueCommentOption { body };
            cx.api.issue().edit_comment(cx.owner()?, cx.name()?, id.get(), &option).await?
        }
        None => {
            let option = gitea_model::CreateIssueCommentOption { body };
            cx.api
                .issue()
                .create_comment(cx.owner()?, cx.name()?, a.target.number.get(), &option)
                .await?
        }
    };

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&comment)?, &cx.term);
    }
    cx.out.text(&format!("{}\n", comment.html_url))
}

async fn existing_comment(cx: &Cx, id: CommentId) -> Result<Comment> {
    cx.api.issue().get_comment(cx.owner()?, cx.name()?, id.get()).await
}

// ----------------------------------------------------------------------------------- edit

async fn edit(cx: &Cx, a: &EditArgs) -> Result<()> {
    let nothing = a.title.is_none()
        && a.body.is_none()
        && a.body_file.is_none()
        && !a.editor
        && a.add_label.is_empty()
        && a.remove_label.is_empty()
        && a.add_assignee.is_empty()
        && a.remove_assignee.is_empty()
        && a.milestone.is_none()
        && !a.remove_milestone;
    if nothing {
        return Err(support::usage(
            "nothing to change; pass --title, -b/--body, -e/--editor, --add-label, \
             --remove-label, --add-assignee, --remove-assignee, -m/--milestone or \
             --remove-milestone",
        ));
    }

    let needs_current = a.editor || !a.add_assignee.is_empty() || !a.remove_assignee.is_empty();
    let current =
        if needs_current { Some(shared::get_issue(cx, a.target.number).await?) } else { None };

    let mut patch = IssuePatch::default();
    if let Some(title) = &a.title {
        patch.title = Some(title.clone());
    }
    let flags =
        BodyFlags { body: a.body.as_deref(), body_file: a.body_file.as_deref(), editor: a.editor };
    if let Some(body) = flags.read(&mut std::io::stdin())? {
        patch.body = Some(body);
    }
    if a.editor {
        let issue = current.as_ref().expect("needs_current covers --editor");
        let seed = support::editor_seed(
            patch.title.as_deref().unwrap_or(&issue.title),
            patch.body.as_deref().unwrap_or(&issue.body),
        );
        let (title, body) = support::split_editor_text(&cx.edit_text(&seed)?).parts();
        if title.trim().is_empty() {
            return Err(support::usage(
                "the first line of the editor buffer is the title, and it was empty",
            ));
        }
        patch.title = Some(title);
        patch.body = Some(body);
    }

    if let Some(title) = &a.milestone {
        patch.milestone = Some(shared::milestone_by_title(cx, title).await?.id.get());
    }
    if a.remove_milestone {
        // 0 is Gitea's "no milestone". It is only ever sent when asked for, which is the
        // whole reason this body is a sparse patch.
        patch.milestone = Some(0);
    }

    // Assignees are a replace-only field on the API, so "add one" has to read first. This is
    // the one place `edit` is racy, and it is racy because the endpoint offers nothing better.
    if !a.add_assignee.is_empty() || !a.remove_assignee.is_empty() {
        let issue = current.as_ref().expect("needs_current covers assignee flags");
        let mut who: Vec<String> = issue.assignees.iter().map(|u| u.login.clone()).collect();
        for user in cx.resolve_users(&a.add_assignee).await? {
            if !who.iter().any(|w| w.eq_ignore_ascii_case(&user)) {
                who.push(user);
            }
        }
        for user in cx.resolve_users(&a.remove_assignee).await? {
            who.retain(|w| !w.eq_ignore_ascii_case(&user));
        }
        patch.assignees = Some(who);
    }

    // Labels go through the label endpoints. `PUT /labels` would replace the set, which is
    // exactly the silent data loss `--add-label` exists to avoid.
    if !a.add_label.is_empty() {
        let option = gitea_model::IssueLabelsOption { labels: Some(a.add_label.clone()) };
        cx.api.issue().add_label(cx.owner()?, cx.name()?, a.target.number.get(), &option).await?;
    }
    // Gitea's `DELETE …/labels/{id}` takes an id only, so the names are resolved first — and
    // all of them before the first delete, so a typo in the second name removes nothing.
    for id in shared::label_ids(&cx.api, cx.repo()?, &a.remove_label).await? {
        cx.api.issue().remove_label(cx.owner()?, cx.name()?, a.target.number.get(), id).await?;
    }

    let issue = if patch.is_empty() {
        shared::get_issue(cx, a.target.number).await?
    } else {
        shared::patch_issue(cx, a.target.number, &patch).await?
    };
    emit_issue(cx, &issue, "Updated")
}

// ------------------------------------------------------------------------------ pin, unpin

async fn pin(cx: &Cx, a: &PinArgs) -> Result<()> {
    // Validated before anything is sent, for the reason `create` gives above: a `--position 0`
    // checked *after* the pin left the issue pinned and the command exiting 2 with
    // "--position counts from 1", which every user reads as "nothing happened".
    if let Some(position) = a.position
        && position < 1
    {
        return Err(support::usage("--position counts from 1"));
    }

    // Pinning something already pinned is a 400 from Gitea, and `gea issue pin 42
    // --position 1` on an already-pinned issue is a completely reasonable thing to type. One
    // read makes it work instead of failing on the first of two calls.
    let issue = shared::get_issue(cx, a.target.number).await?;
    let mut wrote = false;
    if issue.pin_order == 0 {
        cx.api.issue().pin_issue(cx.owner()?, cx.name()?, a.target.number.get()).await?;
        wrote = true;
    } else {
        cx.trace(&format!("#{} is already pinned at {}", issue.number, issue.pin_order));
    }
    if let Some(position) = a.position {
        cx.api
            .issue()
            .move_issue_pin(cx.owner()?, cx.name()?, a.target.number.get(), position)
            .await?;
        wrote = true;
    }
    // `pin_issue` and `move_issue_pin` both answer with no body, so a `pin_order` that reflects
    // the write has to come from a fresh read. When nothing was written — an issue that was
    // already pinned and no `--position` to move it to — the issue already in hand *is* the
    // current one, and re-reading it is a round trip that can only answer with what we have.
    let issue = if wrote { shared::get_issue(cx, a.target.number).await? } else { issue };
    emit_issue(cx, &issue, "Pinned")
}

async fn unpin(cx: &Cx, a: &TargetArgs) -> Result<()> {
    cx.api.issue().unpin_issue(cx.owner()?, cx.name()?, a.number.get()).await?;
    if cx.out.is_machine() {
        let issue = shared::get_issue(cx, a.number).await?;
        return cx.out.machine(support::to_value(&issue)?, &cx.term);
    }
    cx.out.text(&format!("Unpinned issue #{}\n", a.number))
}

// ------------------------------------------------------------------------- dependencies

/// Which of Gitea's two dependency endpoints a flag means.
///
/// `/issues/{i}/dependencies` holds the issues that must be finished **before** `i`;
/// `/issues/{i}/blocks` holds the ones that are waiting **on** `i`. Naming them after the
/// endpoints would leave every reader guessing, so the flags are `--blocked-by` and `--blocks`.
async fn depends_write(cx: &Cx, a: &DependsArgs, add: bool) -> Result<()> {
    let slug = cx.repo()?.clone();
    let (other, blocked_by) = match (a.blocked_by, a.blocks) {
        (Some(n), None) => (n, true),
        (None, Some(n)) => (n, false),
        // clap's ArgGroup makes both and neither unreachable; a panic here would be a crash on
        // a command line, so it is an error instead.
        _ => return Err(support::usage("pass exactly one of --blocked-by or --blocks")),
    };
    if other == a.target.number {
        return Err(support::usage("an issue cannot depend on itself"));
    }

    let meta = gitea_model::IssueMeta {
        index: Some(other.get()),
        owner: Some(slug.owner.clone()),
        repo: Some(slug.name.clone()),
    };
    // Gitea declares `{index}` as a string on the dependency routes, and as an integer
    // everywhere else.
    let index = &a.target.number.get().to_string();
    let issue = match (add, blocked_by) {
        (true, true) => {
            cx.api.issue().create_issue_dependencies(&slug.owner, &slug.name, index, &meta).await?
        }
        (false, true) => {
            cx.api.issue().remove_issue_dependencies(&slug.owner, &slug.name, index, &meta).await?
        }
        (true, false) => {
            cx.api.issue().create_issue_blocking(&slug.owner, &slug.name, index, &meta).await?
        }
        (false, false) => {
            cx.api.issue().remove_issue_blocking(&slug.owner, &slug.name, index, &meta).await?
        }
    };

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&issue)?, &cx.term);
    }
    // Spelled out per case rather than assembled from fragments: "#42 is now blocks #9" is
    // what fragment-joining produces, and a sentence a user cannot parse is not a confirmation.
    let this = a.target.number;
    cx.out.text(&match (add, blocked_by) {
        (true, true) => format!("#{this} is now blocked by #{other}\n"),
        (false, true) => format!("#{this} is no longer blocked by #{other}\n"),
        (true, false) => format!("#{this} now blocks #{other}\n"),
        (false, false) => format!("#{this} no longer blocks #{other}\n"),
    })
}

async fn depends_list(cx: &Cx, a: &TargetArgs) -> Result<()> {
    let index = &a.number.get().to_string();
    // `/dependencies` and `/blocks` are two independent collections, both keyed by an index that
    // is known before either starts, so they are walked at the same time rather than one after
    // the other.
    let walk_blockers = async {
        let q = IssueListIssueDependenciesQuery::default();
        let mut stream = cx.api.issue().list_issue_dependencies(cx.owner()?, cx.name()?, index, &q);
        let mut out: Vec<Issue> = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
        }
        Ok::<_, Error>(out)
    };
    let walk_blocked = async {
        let q = IssueListBlocksQuery::default();
        let mut stream = cx.api.issue().list_blocks(cx.owner()?, cx.name()?, index, &q);
        let mut out: Vec<Issue> = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
        }
        Ok::<_, Error>(out)
    };
    // `join!` and a fixed unwrap order, not `try_join!`: on a repository where both walks fail
    // `try_join!` reports whichever request lost the race, so the message would change between
    // runs. This reports the blockers' error — the one the serial version always reported.
    let (blockers, blocked) = futures::join!(walk_blockers, walk_blocked);
    let blockers = blockers?;
    let blocked = blocked?;

    if cx.out.is_machine() {
        // One array, each item tagged with its direction, so `--jq` can split them. Two
        // separate documents would make `--json` on this command mean something different
        // from `--json` on every other list.
        let mut items: Vec<serde_json::Value> = Vec::new();
        for (direction, list) in [("blocked_by", &blockers), ("blocks", &blocked)] {
            for issue in list {
                let mut value = support::to_value(issue)?;
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("dependency".to_owned(), serde_json::Value::from(direction));
                }
                items.push(value);
            }
        }
        return cx.out.machine(serde_json::Value::Array(items), &cx.term);
    }

    if blockers.is_empty() && blocked.is_empty() {
        support::note(&cx.term, &format!("#{} has no dependencies", a.number));
        return cx.out.table(&Table::new(&cx.term));
    }
    let mut table = Table::new(&cx.term);
    table.headers(["DIRECTION", "NUMBER", "STATE", "TITLE"]);
    push_dependencies(&mut table, "blocked by", &blockers);
    push_dependencies(&mut table, "blocks", &blocked);
    table.banner(format!("Dependencies of #{}", a.number));
    cx.out.table(&table)
}

fn push_dependencies(table: &mut Table, direction: &str, issues: &[Issue]) {
    for issue in issues {
        table.row([
            direction.to_owned(),
            format!("#{}", issue.number),
            issue.state.to_string(),
            issue.title.clone(),
        ]);
    }
}

// -------------------------------------------------------------------------------- output

/// One issue, rendered the way every mutating verb renders it: JSON when asked, otherwise the
/// line a human wants — what happened, and the URL to look at.
fn emit_issue(cx: &Cx, issue: &Issue, verb: &str) -> Result<()> {
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(issue)?, &cx.term);
    }
    cx.out.text(&format!("{verb} issue #{} ({})\n{}\n", issue.number, issue.title, issue.html_url))
}

/// `ErrorKind` for a wrong-kind index, kept here so the wording is identical wherever it is
/// raised.
#[allow(dead_code, reason = "used by tests and by future verbs that reject a pull request")]
fn not_an_issue(index: IssueIndex) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "#{index} is a pull request, not an issue; use `gea pr` for it"
    )))
}

/// Write a line to stderr. Used for progress, never for results.
#[allow(dead_code, reason = "kept beside `note` so both spellings live in one place")]
fn progress(message: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::issue::TargetArgs;
    use crate::cmd::issue::shared::State;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::RepoSlug;
    use std::sync::{Arc, Mutex};

    /// An `http::Method` without naming the type.
    ///
    /// `http` is not a direct dependency of `gea` — `gitea_core::http` re-exports what the
    /// binary needs and deliberately not `Method`, which appears only in `FakeTransport`'s test
    /// API. A macro sidesteps the missing name: the literal is parsed into whatever the
    /// parameter wants. A `fn` would have to declare a return type, and there is none to write.
    macro_rules! method {
        ($name:literal) => {
            $name.parse().expect("a valid HTTP method")
        };
    }

    fn slug() -> RepoSlug {
        RepoSlug::new("perf3ct", "gea")
    }

    fn cx(
        fake: Arc<FakeTransport>,
        buf: &Arc<Mutex<Vec<u8>>>,
        globals: &GlobalOpts,
        term: crate::output::Term,
    ) -> Cx {
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(fake)
            .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
            .build()
            .expect("a well-formed base URL");
        let out = Out::to_buffer(globals, fields::FIELDS_ISSUE, buf);
        Cx::for_test(gitea_client::Api::new(client), Some(slug()), term, out)
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8 output")
    }

    /// Issue #42, with its timestamps a fixed distance *behind now* rather than at a fixed
    /// date.
    ///
    /// `timeago` is relative, so a fixture with an absolute `created_at` would render
    /// "about 11 days ago" today and "about 12 days ago" tomorrow — every snapshot below would
    /// expire on its own. Two hours ago always renders as "about 2 hours ago".
    ///
    /// `r##` rather than `r#`: the body contains `"#`, which would close a single-hash raw
    /// string in the middle of the JSON.
    fn issue_42() -> String {
        let ago =
            |hours: i64| (jiff::Timestamp::now() - jiff::Span::new().hours(hours)).to_string();
        format!(
            r##"{{
        "id": 918273, "number": 42, "title": "It broke", "state": "open",
        "body": "# Steps\n\n- one\n- two\n",
        "html_url": "https://git.example.org/perf3ct/gea/issues/42",
        "created_at": "{}", "updated_at": "{}",
        "comments": 1, "pin_order": 0,
        "user": {{"login": "alice"}},
        "labels": [{{"id": 4, "name": "bug", "color": "e11d21"}}],
        "milestone": {{"id": 7, "title": "1.0"}},
        "assignees": [{{"login": "bob"}}]
    }}"##,
            ago(2),
            ago(1)
        )
    }

    #[tokio::test]
    async fn view_uses_the_issue_index_not_the_database_id() {
        // Bug this prevents: sending `Issue.id` (918273) where `{index}` belongs. The request
        // would 404 at best and, on a large instance, address a different real issue.
        let fake = Arc::new(FakeTransport::new().on(
            method!("GET"),
            "/api/v1/repos/perf3ct/gea/issues/42",
            Canned::json(200, issue_42()),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = ViewArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            comments: false,
            web: false,
        };
        view(&cx, &args).await.unwrap();

        assert_eq!(fake.calls()[0].path, "/api/v1/repos/perf3ct/gea/issues/42");
        assert!(
            !fake.calls()[0].path.contains("918273"),
            "the database id must not reach the path"
        );
        insta::assert_snapshot!("view_human", text(&buf));
    }

    #[tokio::test]
    async fn view_json_projects_the_requested_fields() {
        let fake = Arc::new(FakeTransport::new().on(
            method!("GET"),
            "/api/v1/repos/perf3ct/gea/issues/42",
            Canned::json(200, issue_42()),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals =
            GlobalOpts { json: Some("number,title,state".to_owned()), ..GlobalOpts::default() };
        let cx = cx(fake, &buf, &globals, crate::output::Term::piped());
        let args = ViewArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            comments: false,
            web: false,
        };
        view(&cx, &args).await.unwrap();
        insta::assert_snapshot!("view_json", text(&buf));
    }

    /// Bug this prevents: `--add-label` sending `PUT /labels` (replace) or putting labels in
    /// the issue patch, either of which discards a label somebody else added.
    #[tokio::test]
    async fn add_label_adds_and_never_replaces() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("POST"),
                    "/api/v1/repos/perf3ct/gea/issues/42/labels",
                    Canned::json(200, "[]"),
                )
                .on(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42",
                    Canned::json(200, issue_42()),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = EditArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            title: None,
            body: None,
            body_file: None,
            editor: false,
            add_label: Vec::from(["ci".to_owned()]),
            remove_label: Vec::new(),
            add_assignee: Vec::new(),
            remove_assignee: Vec::new(),
            milestone: None,
            remove_milestone: false,
        };
        edit(&cx, &args).await.unwrap();

        let posts = fake.calls_to(&method!("POST"), "/api/v1/repos/perf3ct/gea/issues/42/labels");
        assert_eq!(posts.len(), 1, "one additive call");
        assert_eq!(posts[0].body_str(), r#"{"labels":["ci"]}"#);
        assert!(
            fake.calls().iter().all(|c| c.method.as_str() != "PUT"),
            "PUT /labels replaces the set and must never be used by an edit"
        );
        // And no PATCH at all, because nothing else changed: an empty patch is not sent.
        assert!(fake.calls().iter().all(|c| c.method.as_str() != "PATCH"), "{:?}", fake.calls());
    }

    /// Bug this prevents: closing an issue with the generated `EditIssueOption`, which also
    /// sends `body:""` (erasing the body) and `assignees:[]` (unassigning everybody).
    #[tokio::test]
    async fn close_sends_only_the_state() {
        let fake = Arc::new(FakeTransport::new().on(
            method!("PATCH"),
            "/api/v1/repos/perf3ct/gea/issues/42",
            Canned::json(200, issue_42()),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        close(&cx, &TargetArgs { number: IssueIndex::new(42) }, None).await.unwrap();
        assert_eq!(fake.calls()[0].body_str(), r#"{"state":"closed"}"#);
    }

    /// Bug this prevents: `gea issue list` showing pull requests, because
    /// `GET /repos/{o}/{r}/issues` returns both unless `type=issues` is sent.
    #[tokio::test]
    async fn list_asks_for_issues_only() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                // `on_sequence`, not `on`: a fake that answers every page with the same
                // non-empty body makes the paginator walk forever, because with no `Link`
                // header it can only stop on a short page. A real server answers page 2 with
                // `[]`, and so must the fake.
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues",
                    Vec::from([
                        Canned::json(200, format!("[{}]", issue_42())),
                        Canned::json(200, "[]".to_owned()),
                    ]),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::tty(100));
        let args = ListArgs {
            state: State::Open,
            assignee: None,
            author: None,
            label: Vec::from(["bug".to_owned()]),
            milestone: Some("1.0".to_owned()),
            limit: None,
            search: None,
            web: false,
        };
        list(&cx, &globals, &args).await.unwrap();

        let call = fake
            .calls()
            .into_iter()
            .find(|c| c.path == "/api/v1/repos/perf3ct/gea/issues")
            .expect("the list request");
        assert!(call.query.contains("type=issues"), "{}", call.query);
        assert!(call.query.contains("state=open"), "{}", call.query);
        assert!(call.query.contains("labels=bug"), "{}", call.query);
        assert!(call.query.contains("milestones=1.0"), "{}", call.query);
        insta::assert_snapshot!("list_human", text(&buf));
    }

    /// Bug this prevents — the reported one. Against a real Gitea with 43 open issues the
    /// banner said `Showing 30 open issues`, hiding the 13 it had dropped, while `X-Total-Count`
    /// sat unread in the response.
    ///
    /// The fix cannot come from the list request itself: `ItemStream` is a bare `Stream`, so the
    /// `x-total-count` its pages carried is consumed by the paginator and has nowhere to surface.
    /// Switching the whole list onto the `_page` twin would read the header for free but would
    /// re-introduce the clamped-page data loss the stream exists to avoid, so the total comes
    /// from a separate one-item probe — and only when the list came back full.
    #[tokio::test]
    async fn a_truncated_list_asks_the_collection_how_many_there_were() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues",
                    Vec::from([
                        // The list itself, filled to the `-L 1` cap...
                        Canned::json(200, format!("[{}]", issue_42())),
                        // ...then the probe, whose only job is the header.
                        Canned::json(200, format!("[{}]", issue_42()))
                            .with_header("x-total-count", "43"),
                    ]),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::tty(100));
        let args = ListArgs {
            state: State::Open,
            assignee: None,
            author: None,
            label: Vec::new(),
            milestone: None,
            limit: Some(1),
            search: None,
            web: false,
        };
        list(&cx, &globals, &args).await.unwrap();

        let out = text(&buf);
        assert!(
            out.starts_with("Showing 1 of 43 open issues in perf3ct/gea\n"),
            "the banner must admit what it withheld: {out}"
        );

        // The probe is one item wide: it is paying for a header, not for rows.
        let probe = fake
            .calls()
            .into_iter()
            .rfind(|c| c.path == "/api/v1/repos/perf3ct/gea/issues")
            .expect("the probe request");
        assert!(
            probe.query.contains("limit=1"),
            "the probe must not refetch the list: {}",
            probe.query
        );
        assert!(
            probe.query.contains("state=open"),
            "the probe must count the same set: {}",
            probe.query
        );
    }

    /// ...and a list that fits under its cap pays for no probe at all: it already is the whole
    /// collection, so there is nothing to ask anybody.
    #[tokio::test]
    async fn a_complete_list_costs_no_extra_request() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues",
                    Vec::from([
                        Canned::json(200, format!("[{}]", issue_42())),
                        Canned::json(200, "[]".to_owned()),
                    ]),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::tty(100));
        let args = ListArgs {
            state: State::Open,
            assignee: None,
            author: None,
            label: Vec::new(),
            milestone: None,
            limit: None,
            search: None,
            web: false,
        };
        list(&cx, &globals, &args).await.unwrap();

        assert!(text(&buf).starts_with("Showing 1 open issue in perf3ct/gea\n"), "{}", text(&buf));
        // Two: the one page of issues, and the `[]` that ends the walk. No third.
        let hits = fake
            .calls()
            .into_iter()
            .filter(|c| c.path == "/api/v1/repos/perf3ct/gea/issues")
            .count();
        assert_eq!(hits, 2, "a complete list must not probe for a total it already knows");
    }

    /// An issue that is already pinned and is not being moved has nothing written to it, so the
    /// trailing re-read has nothing to learn. `pin_issue` and `move_issue_pin` answer with no
    /// body, which is the only reason that second GET exists at all.
    #[tokio::test]
    async fn pinning_an_already_pinned_issue_costs_one_request() {
        let pinned = issue_42().replace(r#""pin_order": 0"#, r#""pin_order": 3"#);
        let fake = Arc::new(FakeTransport::new().on(
            method!("GET"),
            "/api/v1/repos/perf3ct/gea/issues/42",
            Canned::json(200, pinned),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = PinArgs { target: TargetArgs { number: IssueIndex::new(42) }, position: None };

        pin(&cx, &args).await.unwrap();
        assert_eq!(
            fake.call_count(),
            1,
            "nothing was written, so nothing to re-read: {:?}",
            fake.calls()
        );
    }

    /// `/dependencies` and `/blocks` are independent collections keyed by the same index, so
    /// `depends list` walks them at the same time. Matched on **path**, never on position: the
    /// two now race, and a positional assertion would go flaky rather than fail honestly.
    #[tokio::test]
    async fn depends_list_walks_both_directions() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42/dependencies",
                    Vec::from([
                        Canned::json(200, format!("[{}]", issue_42())),
                        Canned::json(200, "[]".to_owned()),
                    ]),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42/blocks",
                    Vec::from([Canned::json(200, "[]".to_owned())]),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::tty(100));

        depends_list(&cx, &TargetArgs { number: IssueIndex::new(42) }).await.unwrap();

        for direction in ["dependencies", "blocks"] {
            let path = format!("/api/v1/repos/perf3ct/gea/issues/42/{direction}");
            assert!(
                fake.calls().iter().any(|c| c.path == path),
                "{direction} was never walked: {:?}",
                fake.calls()
            );
        }
        // Blockers first, then what the issue blocks — the order `push_dependencies` is called
        // in, which concurrency must not be allowed to reshuffle.
        let out = text(&buf);
        assert!(out.contains("blocked by"), "{out}");
    }

    /// Bug this prevents: `--json` and `--web` paying for comments neither one prints. `-c` is a
    /// display flag; both of those paths return before any comment is rendered, so the fetch has
    /// to be gated on all three and not just on `-c`.
    #[tokio::test]
    async fn view_fetches_comments_only_when_it_will_print_them() {
        let routes = || {
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42",
                    Canned::json(200, issue_42()),
                )
                .on(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42/comments",
                    Canned::json(200, "[]"),
                )
        };
        let args = || ViewArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            comments: true,
            web: false,
        };

        // Human `-c`: both reads happen, and they happen together.
        let fake = Arc::new(routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let human = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        view(&human, &args()).await.unwrap();
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
        // Matched on path, not position: the two arms race, and an index would be flaky.
        for path in
            ["/api/v1/repos/perf3ct/gea/issues/42", "/api/v1/repos/perf3ct/gea/issues/42/comments"]
        {
            assert!(fake.calls().iter().any(|c| c.path == path), "{path} was never read");
        }

        // `-c --json`: the issue alone, because `--json` emits the issue and returns.
        let fake = Arc::new(routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts { json: Some("number".to_owned()), ..GlobalOpts::default() };
        let machine = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        view(&machine, &args()).await.unwrap();
        assert_eq!(fake.call_count(), 1, "--json must not pay for comments: {:?}", fake.calls());
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/perf3ct/gea/issues/42");
    }

    /// Bug this prevents: `--blocked-by` and `--blocks` being wired to each other's endpoint.
    /// Both return 200 and an Issue, so nothing fails — the dependency is simply backwards.
    #[tokio::test]
    async fn dependency_directions_hit_their_own_endpoints() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("POST"),
                    "/api/v1/repos/perf3ct/gea/issues/42/dependencies",
                    Canned::json(200, issue_42()),
                )
                .on(
                    method!("POST"),
                    "/api/v1/repos/perf3ct/gea/issues/42/blocks",
                    Canned::json(200, issue_42()),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());

        let blocked_by = DependsArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            blocked_by: Some(IssueIndex::new(7)),
            blocks: None,
        };
        depends_write(&cx, &blocked_by, true).await.unwrap();
        let blocks = DependsArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            blocked_by: None,
            blocks: Some(IssueIndex::new(9)),
        };
        depends_write(&cx, &blocks, true).await.unwrap();

        let calls = fake.calls();
        assert_eq!(calls[0].path, "/api/v1/repos/perf3ct/gea/issues/42/dependencies");
        assert!(calls[0].body_str().contains(r#""index":7"#), "{}", calls[0].body_str());
        assert_eq!(calls[1].path, "/api/v1/repos/perf3ct/gea/issues/42/blocks");
        assert!(calls[1].body_str().contains(r#""index":9"#), "{}", calls[1].body_str());
        insta::assert_snapshot!("depends_human", text(&buf));
    }

    /// Bug this prevents — the reported one. `gea issue pin 42 --position 0` sent `POST
    /// .../pin`, pinned the issue, and *then* exited 2 saying "--position counts from 1". The
    /// user reads a usage error as "nothing happened" and the issue is pinned anyway, which is
    /// exactly the half-written state `create` refuses at the top of this file.
    #[tokio::test]
    async fn a_position_below_one_is_refused_before_anything_is_pinned() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/issues/42",
                    Canned::json(200, issue_42()),
                )
                .on(method!("POST"), "/api/v1/repos/perf3ct/gea/issues/42/pin", Canned::new(204)),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args =
            PinArgs { target: TargetArgs { number: IssueIndex::new(42) }, position: Some(0) };

        assert_eq!(pin(&cx, &args).await.unwrap_err().exit_code(), 2);
        assert_eq!(
            fake.call_count(),
            0,
            "a rejected --position must not pin first: {:?}",
            fake.calls()
        );
    }

    #[tokio::test]
    async fn an_issue_cannot_depend_on_itself() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = DependsArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            blocked_by: Some(IssueIndex::new(42)),
            blocks: None,
        };
        assert_eq!(depends_write(&cx, &args, true).await.unwrap_err().exit_code(), 2);
        assert_eq!(fake.call_count(), 0, "a self-dependency is rejected before any request");
    }

    /// Bug this prevents: a comment edit addressed by issue index. `/issues/comments/{id}`
    /// takes a **comment** id; sending 42 there edits comment 42, which belongs to some other
    /// issue entirely.
    #[tokio::test]
    async fn editing_a_comment_uses_the_comment_id_route() {
        let fake = Arc::new(FakeTransport::new().on(
            method!("PATCH"),
            "/api/v1/repos/perf3ct/gea/issues/comments/555",
            Canned::json(
                200,
                r#"{"id":555,"body":"fixed","html_url":"https://git.example.org/c/555"}"#,
            ),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = CommentArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            body: Some("fixed".to_owned()),
            body_file: None,
            editor: false,
            edit: Some(CommentId::new(555)),
            web: false,
        };
        comment(&cx, &args).await.unwrap();
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/perf3ct/gea/issues/comments/555");
        assert_eq!(text(&buf), "https://git.example.org/c/555\n");
    }

    #[tokio::test]
    async fn an_empty_list_is_success_with_an_empty_table() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on(method!("GET"), "/api/v1/repos/perf3ct/gea/issues", Canned::json(200, "[]")),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, crate::output::Term::piped());
        let args = ListArgs {
            state: State::Open,
            assignee: None,
            author: None,
            label: Vec::new(),
            milestone: None,
            limit: None,
            search: None,
            web: false,
        };
        list(&cx, &globals, &args).await.unwrap();
        assert_eq!(text(&buf), "", "an empty table, not a message, on stdout");
    }

    /// Bug this prevents: `edit` with no flags silently doing nothing (or worse, sending an
    /// empty patch that Gitea reads as "clear everything").
    #[tokio::test]
    async fn edit_with_no_flags_is_a_usage_error() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, crate::output::Term::piped());
        let args = EditArgs {
            target: TargetArgs { number: IssueIndex::new(42) },
            title: None,
            body: None,
            body_file: None,
            editor: false,
            add_label: Vec::new(),
            remove_label: Vec::new(),
            add_assignee: Vec::new(),
            remove_assignee: Vec::new(),
            milestone: None,
            remove_milestone: false,
        };
        let e = edit(&cx, &args).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--add-label"), "{e}");
        assert_eq!(fake.call_count(), 0);
    }
}

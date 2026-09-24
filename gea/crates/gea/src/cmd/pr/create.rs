//! `gea pr create` — including AGit, the thing `gh` structurally cannot do.
//!
//! # AGit
//!
//! Every forge in the GitHub lineage requires a *branch* to open a pull request from, and — if you
//! cannot push to the repository — a *fork* to put that branch in. Gitea also accepts pull
//! requests over **AGit**, which needs neither. You push your commits to a magic ref:
//!
//! ```text
//! git push origin HEAD:refs/for/main/my-topic
//! ```
//!
//! and the server creates a pull request against `main` from those commits. Nothing is added to the
//! repository's branch namespace, no fork exists, and a contributor with no write access can do it
//! against a repository that allows it. `git push` is the entire protocol, which is why this is
//! structural rather than a feature `gh` merely lacks: there is no GitHub API call that would do it.
//!
//! Three rules the implementation has to respect, because Gitea enforces them:
//!
//! 1. **A topic is mandatory.** `refs/for/<base>` with no topic is refused. `gea` defaults the
//!    topic to the current branch's name, which is both memorable and stable — and stability is what
//!    matters, because…
//! 2. **Updating means pushing the same topic again.** A different topic opens a *second* pull
//!    request. This is why the default is the branch name rather than, say, a timestamp.
//! 3. **An amended or rebased history needs `--force-push`,** since the update must otherwise be a
//!    fast-forward. That is the same rule as any other push, arriving in a place people do not
//!    expect it.
//!
//! Reference: <https://about.gitea.com/docs/latest/user/agit-support/>.
//!
//! The push itself — the refspec, the push options, and the advice a refusal carries — lives in
//! [`gitea_core::context::git::agit`], because `git push` *is* the protocol and there is no API
//! call to pair it with. This module supplies the title, the body and the topic, and reads back
//! the pull request the server says it created.
//!
//! ## Where a refusal is printed
//!
//! [`gitea_core::context::git::GitCtx::push_agit`] folds git's stderr into the error it returns
//! rather than letting this module `eprint!` it first. That is a deliberate choice between the two
//! shapes `push_agit`'s own documentation offers, and the reason is redaction: a remote URL can
//! carry an embedded credential, git repeats that URL verbatim in a push failure, and only
//! `gitea-core` can scrub it. Echoing the raw stderr here and *then* erroring would publish a
//! token into whatever the user pastes into a bug report. Nothing is lost — the error carries
//! every line git said, under `git says:` — and the successful path still relays git's stderr as
//! it arrives, which is where Gitea's `remote:` lines with the pull request URL show up.
//!
//! # `--recover`
//!
//! A `create` that fails after the user wrote three paragraphs in `$EDITOR` has destroyed work. So
//! the title and body are written to a recovery file *before* the request goes out, the failure
//! message names it, and `--recover` replays it. Small feature; it is the difference between
//! shrugging and swearing.

use std::path::PathBuf;

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::context::git::{AgitPush, AgitRef, GitCtx};
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{CreatePullRequestOption, PullRequest, PullReviewRequestOptions};

use super::common;
use crate::cmd::support::{self, BodyOpts};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Create a pull request.

--base defaults to the repository's default branch; --head defaults to the current
branch. The server links references such as Fixes #123 in the body.

--agit pushes to refs/for/<base>/<topic> without creating a fork or branch.
The topic defaults to the current branch name. Reuse it to update the same pull
request; use --force-push after amending or rebasing commits.

Use --title for the title; -t is reserved for output templates.

  gea pr create --fill
  gea pr create --title 'Fix the thing' -b 'Fixes #12' -l bug -r alice
  gea pr create -e --draft
  gea pr create --agit --topic fix-parser --fill
  gea pr create --agit --force-push        # after amending
  gea pr create --dry-run --fill")]
pub struct Args {
    /// Title. There is no `-t`: that is the global `--template`
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,

    #[command(flatten)]
    pub body: BodyOpts,

    /// Open the compare page in a browser instead of creating anything
    #[arg(short = 'w', long)]
    pub web: bool,

    /// Take the title and body from the commits on this branch
    #[arg(short = 'f', long, conflicts_with = "fill_first")]
    pub fill: bool,

    /// Take the title and body from the first commit only
    #[arg(long)]
    pub fill_first: bool,

    /// Open as a draft. Gitea marks drafts with a `WIP:` title prefix
    #[arg(short = 'd', long)]
    pub draft: bool,

    /// Branch to merge into. Defaults to the repository's default branch
    #[arg(short = 'B', long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Branch to merge from. Defaults to the current branch
    #[arg(short = 'H', long, value_name = "BRANCH", conflicts_with = "agit")]
    pub head: Option<String>,

    /// Assign a user. Repeatable; `@me` is you
    #[arg(short = 'a', long, value_name = "USER")]
    pub assignee: Vec<String>,

    /// Add a label by name. Repeatable
    #[arg(short = 'l', long, value_name = "NAME")]
    pub label: Vec<String>,

    /// Milestone, by title
    #[arg(short = 'm', long, value_name = "TITLE")]
    pub milestone: Option<String>,

    /// Request a review. Repeatable; `@me` is you
    #[arg(short = 'r', long, value_name = "USER")]
    pub reviewer: Vec<String>,

    /// Show what would be sent, and send nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Reuse the title and body saved by a run that failed
    #[arg(long)]
    pub recover: bool,

    /// Open the pull request over AGit: no branch, no fork
    #[arg(long)]
    pub agit: bool,

    /// AGit topic. Defaults to the current branch name; the same topic updates the same pull request
    #[arg(long, value_name = "TOPIC", requires = "agit")]
    pub topic: Option<String>,

    /// Force the AGit push, for a history you amended or rebased
    #[arg(long, requires = "agit")]
    pub force_push: bool,

    /// Git remote to push through. Defaults to whichever remote names the base repository
    #[arg(long, value_name = "NAME")]
    pub remote: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = rt.repo(globals)?.slug.clone();
        // The base branch falls back to the repository's own default, which is one request and
        // the difference between `gea pr create --fill` working and needing `-B main` every
        // time. Asked for only when it is actually the answer: this used to be unconditional,
        // and `--base` did not save the request, so `gea pr create --agit --dry-run --base main`
        // — a command that changes nothing and was told the one thing the request would have
        // told it — still needed the network and a working token, and exited 6 without one.
        //
        // It is `?`, and it is the FIRST thing the command does, which is why it is worth being
        // careful about. For `--agit` the push is the whole protocol; a hard failure here means
        // no push happened at all, so the visible result is a pull request that never appeared
        // with nothing on stdout to say why. That is exactly how a scheme-guessing bug in the
        // integration harness read as "AGit is broken" for a full CI round trip.
        let base = match args.base.clone() {
            Some(base) => base,
            None => api.repo().get(&slug.owner, &slug.name).await?.default_branch,
        };
        // Resolved once: `--fill` needs it to find the base's remote-tracking ref, and `--agit`
        // needs it to know where to push.
        let remote = args.remote.clone().or(crate::cmd::repo::remote_for(&rt, &slug)?);

        let draft = Draft::assemble(&rt, args, &slug, &base, remote.as_deref())?;

        if args.web {
            return open_compare(&rt, &slug, &base, &head_for(&rt, args)?);
        }

        // Written before anything is sent, so a 422 five lines below cannot destroy an editor
        // session. Removed again on success.
        let recovery = draft.save(&rt, &slug);

        let created = if args.agit {
            agit(&rt, &api, args, &slug, &base, &draft, remote.as_deref()).await
        } else {
            classic(&rt, &api, args, &slug, &base, &draft).await
        };

        let pr = match created {
            Ok(pr) => pr,
            Err(e) => {
                if let Some(path) = &recovery {
                    eprintln!(
                        "your title and body were saved; re-run with --recover to reuse them \
                         ({})",
                        path.display()
                    );
                }
                return Err(e);
            }
        };
        if let Some(path) = recovery {
            let _ = std::fs::remove_file(path);
        }

        request_reviews(&api, args, &slug, &pr).await?;
        common::emit_or(&rt, globals, &wanted, &pr, || {
            println!("{}", pr.html_url);
            Ok(())
        })
    })
}

// -------------------------------------------------------------------------------- title and body

/// What the pull request will say.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Draft {
    pub title: String,
    pub body: String,
}

impl Draft {
    /// Resolve the title and body from every source, in precedence order.
    ///
    /// `--recover` first because it is a replay of a previous decision; then the explicit flags;
    /// then `--fill`; then a prompt. The editor is last among the flags because its *first line is
    /// the title* and that has to be able to override a `--fill`ed one.
    fn assemble(
        rt: &Runtime,
        args: &Args,
        slug: &RepoSlug,
        base: &str,
        remote: Option<&str>,
    ) -> Result<Self> {
        let mut draft = if args.recover { load(rt, slug)? } else { Self::default() };

        if args.fill || args.fill_first {
            let filled = fill(rt.git(), args, base, remote)?;
            if draft.title.is_empty() {
                draft.title = filled.title;
            }
            if draft.body.is_empty() {
                draft.body = filled.body;
            }
        }

        let supplied = args.body.resolve(rt, &draft.body)?;
        if let Some(title) = supplied.title {
            draft.title = title;
        }
        if let Some(body) = supplied.body {
            draft.body = body;
        }
        // The explicit flag wins over everything, including the editor's first line: somebody who
        // passed both said the quiet part out loud.
        if let Some(title) = &args.title {
            draft.title = title.clone();
        }

        if draft.title.trim().is_empty() {
            if !support::can_prompt(rt) {
                return Err(support::missing(
                    "--title (or -f/--fill, or -e/--editor)",
                    "a pull request title",
                ));
            }
            draft.title = support::interact::ask("Title", None)?;
        }
        if draft.title.trim().is_empty() {
            return Err(Error::new(ErrorKind::Cancelled));
        }
        if args.draft {
            draft.title = common::add_wip(&draft.title);
        }
        Ok(draft)
    }

    /// Write the recovery file, returning where it went. Failure is not fatal.
    fn save(&self, rt: &Runtime, slug: &RepoSlug) -> Option<PathBuf> {
        let path = recovery_path(rt, slug);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(self).ok()?).ok()?;
        Some(path)
    }
}

/// One recovery file per repository, so two half-finished pull requests do not overwrite each
/// other. The slug is flattened rather than nested, so there are no stray directories to clean up.
fn recovery_path(rt: &Runtime, slug: &RepoSlug) -> PathBuf {
    let safe: String =
        slug.to_string().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    rt.config().dir().join("recover").join(format!("pr-create-{safe}.json"))
}

fn load(rt: &Runtime, slug: &RepoSlug) -> Result<Draft> {
    let path = recovery_path(rt, slug);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        Error::new(ErrorKind::Usage(format!(
            "--recover found nothing saved for {slug} at {}: {e}",
            path.display()
        )))
    })?;
    serde_json::from_str(&text).map_err(|e| {
        Error::new(ErrorKind::Usage(format!("{} is not a saved draft: {e}", path.display())))
    })
}

/// `--fill` / `--fill-first`: a title and body from the commits on this branch.
///
/// One commit means the title is its subject and the body its message — which is what everybody
/// means by "just use my commit". Several commits mean the subjects become a list, because gluing
/// four commit messages together produces something no human would have written.
fn fill(git: &dyn GitCtx, args: &Args, base: &str, remote: Option<&str>) -> Result<Draft> {
    let range = fill_range(git, base, remote)?;

    if args.fill_first {
        let first = git.first_commit_message(&range)?.ok_or_else(|| empty_range(&range))?;
        return Ok(Draft { title: first.subject, body: first.body });
    }

    let subjects = git.commit_subjects(&range)?;
    match subjects.len() {
        0 => Err(empty_range(&range)),
        1 => {
            let first = git.first_commit_message(&range)?.ok_or_else(|| empty_range(&range))?;
            Ok(Draft { title: first.subject, body: first.body })
        }
        _ => {
            let body =
                subjects.iter().map(|s| format!("* {s}")).collect::<Vec<_>>().join("\n") + "\n";
            Ok(Draft { title: subjects[0].clone(), body })
        }
    }
}

fn empty_range(range: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "no commits in {range} to use for --fill. Add a commit or pass --title."
    )))
}

/// The revision range `--fill` reads: everything on HEAD that the base does not have.
///
/// `<remote>/<base>..HEAD` when a remote-tracking ref exists, because that is what the *server*
/// will compare against. A local `<base>` is the fallback for a checkout that has never fetched,
/// and it can be stale — which is exactly why it is second.
fn fill_range(git: &dyn GitCtx, base: &str, remote: Option<&str>) -> Result<String> {
    let mut tried = Vec::new();
    if let Some(remote) = remote {
        tried.push(format!("{remote}/{base}"));
    }
    tried.push(base.to_owned());
    for candidate in &tried {
        if git.rev_exists(candidate)? {
            return Ok(format!("{candidate}..HEAD"));
        }
    }
    Err(Error::new(ErrorKind::Usage(format!(
        "--fill needs to know which commits are new, and none of {} exists in this checkout; \
         fetch the base branch, or pass --title",
        tried.join(", ")
    ))))
}

// ------------------------------------------------------------------------------- the classic path

async fn classic(
    rt: &Runtime,
    api: &Api,
    args: &Args,
    slug: &RepoSlug,
    base: &str,
    draft: &Draft,
) -> Result<PullRequest> {
    let head = head_for(rt, args)?;
    let mut body = CreatePullRequestOption {
        title: Some(draft.title.clone()),
        body: Some(draft.body.clone()),
        base: Some(base.to_owned()),
        head: Some(head.clone()),
        ..CreatePullRequestOption::default()
    };
    // Three read-only lookups that depend on nothing but the command line, so they run at the
    // same time instead of back to back.
    //
    // `join!` with a fixed unwrap order rather than `try_join!`, and that is a correctness
    // choice, not a stylistic one: `try_join!` returns the first error to *occur*, so
    // `-a nobody -l nope` would report a different problem depending on which response the
    // network delivered first. Unwrapped in the order the requests used to be made in, the
    // command reports exactly what it always reported.
    let resolve_assignees = support::resolve_me(api, &args.assignee);
    let resolve_labels = common::label_ids(api, slug, &args.label);
    let resolve_milestone = async {
        match &args.milestone {
            Some(title) => common::milestone_id(api, slug, title).await.map(Some),
            None => Ok(None),
        }
    };
    let resolved = futures::join!(resolve_assignees, resolve_labels, resolve_milestone);
    body.assignees = Some(resolved.0?);
    body.labels = Some(resolved.1?);
    if let Some(id) = resolved.2? {
        body.milestone = Some(id);
    }

    if args.dry_run {
        describe(slug, &body);
        // A `--dry-run` that then went on to print a pull request URL would be lying, and a
        // `Default` pull request is the honest shape of "nothing was created".
        return Ok(PullRequest::default());
    }
    api.repo().create_pull_request(&slug.owner, &slug.name, &body).await
}

/// The head branch, as the API wants it.
///
/// Plain `branch` for a pull request inside one repository. `owner:branch` when the branch lives in
/// a *fork* — which is the normal contributor shape, and the one Gitea cannot infer, because the
/// request is made against the base repository and there may be many forks with that branch name.
fn head_for(rt: &Runtime, args: &Args) -> Result<String> {
    if let Some(head) = &args.head {
        return Ok(head.clone());
    }
    rt.git().current_branch()?.ok_or_else(|| {
        Error::new(ErrorKind::Usage("HEAD is detached; specify a branch with -H/--head".to_owned()))
    })
}

async fn request_reviews(api: &Api, args: &Args, slug: &RepoSlug, pr: &PullRequest) -> Result<()> {
    if args.reviewer.is_empty() || args.dry_run || pr.number.get() == 0 {
        return Ok(());
    }
    // A separate endpoint: `CreatePullRequestOption` has no reviewers field, so this is the second
    // call that makes `-r` work at all — the kind of orchestration layer 3 exists for.
    let body = PullReviewRequestOptions {
        reviewers: Some(support::resolve_me(api, &args.reviewer).await?),
        team_reviewers: Some(Vec::new()),
    };
    api.repo()
        .create_pull_review_requests(&slug.owner, &slug.name, pr.number.get(), &body)
        .await
        .map(|_| ())
}

fn describe(slug: &RepoSlug, body: &CreatePullRequestOption) {
    println!("POST /repos/{}/{}/pulls", slug.owner, slug.name);
    match serde_json::to_string_pretty(body) {
        Ok(json) => println!("{json}"),
        Err(e) => println!("(could not render the body: {e})"),
    }
}

// ------------------------------------------------------------------------------------------ AGit

async fn agit(
    rt: &Runtime,
    api: &Api,
    args: &Args,
    slug: &RepoSlug,
    base: &str,
    draft: &Draft,
    remote: Option<&str>,
) -> Result<PullRequest> {
    let topic = topic_for(rt, args)?;
    let remote = remote.ok_or_else(|| {
        Error::new(ErrorKind::Usage(format!(
            "an AGit pull request is created by pushing, and no git remote in this checkout points \
             at {slug}; add one, or pass --remote"
        )))
    })?;
    // The refspec, the push options and the three rules above all live in
    // `gitea_core::context::git::agit`, so the shape of an AGit push is described in one place
    // and a test can assert on the argv rather than on a real server's answer.
    let push = AgitPush::new(remote, AgitRef::new(base, &topic)?)
        .with_title(&draft.title)
        .with_body(&draft.body)
        .forced(args.force_push);

    if !args.assignee.is_empty() || !args.label.is_empty() || args.milestone.is_some() {
        // Said once, plainly, rather than silently dropping the flags: AGit carries only the title
        // and description, and the rest have to be applied afterwards.
        support::note(
            rt.term(),
            "AGit push options carry only the title and description; apply assignees, labels and \
             the milestone with `gea pr edit` once the pull request exists",
        );
    }

    let argv = render_argv(&push.to_push_spec().argv());
    if args.dry_run {
        println!("git -C . {argv}");
        return Ok(PullRequest::default());
    }

    rt.trace(&format!("agit: git {argv}"));
    let outcome = rt.git().push_agit(&push)?;
    // Relayed verbatim on success. Gitea's answer to an accepted AGit push — the pull request
    // URL, the "Processed 1 references" line — arrives on git's stderr as `remote:` lines, and a
    // wrapper that swallowed them would be the `tea` bug in a new place. A *refusal* is relayed
    // too, but through the error `push_agit` returns rather than ahead of it, so that its text is
    // redacted first — see "Where a refusal is printed" in the module docs.
    if !outcome.stderr.trim().is_empty() {
        eprint!("{}", outcome.stderr);
    }

    // Everything past this point is a READ, and the write already succeeded. Nothing here may
    // fail the command.
    //
    // `locate` used to be `?`, so any error reading the pull request back — a 401, a decode, a
    // dropped connection — exited non-zero for a push the server had already accepted. Measured
    // with an invalid API token against a live Gitea (git auth is separate, so the push still
    // lands): exit 4, "refused your token", and the caller above then printing "your title and
    // body were saved; re-run with --recover" — inviting a retry of work that was done. The
    // pull request URL is in git's own output three lines further up.
    //
    // This is the bug the AGit integration test's `#[ignore]` was originally about: a null
    // `merge_commit_sha` failed the decode of a pull request that had just been created
    // successfully. That was fixed in the decoder, and the same shape reappeared one call
    // further out, where every other possible error reaches it too. Fixing the decoder fixed one
    // error; this fixes the position.
    match locate(api, slug, outcome.pull_index, base, &topic).await {
        Ok(Some(pr)) => Ok(pr),
        // The push succeeded, so the pull request exists; we just could not identify which one.
        // Reporting that honestly beats failing a command that did what it was asked.
        Ok(None) => {
            support::warn("pushed; could not identify the pull request the server created");
            Ok(PullRequest::default())
        }
        // Distinct from the case above and worth its own words: there we could not work out
        // WHICH pull request, here we know which and could not read it. Neither fails.
        Err(e) => {
            support::warn(&match outcome.pull_index {
                Some(n) => format!(
                    "push succeeded and pull request #{n} was created, but fetching it failed. No result on stdout: {e}"
                ),
                None => format!(
                    "push succeeded and the pull request exists, but fetching it failed. No result on stdout: {e}"
                ),
            });
            Ok(PullRequest::default())
        }
    }
}

/// An argv rendered for a human to read, which is all `--dry-run` and `--debug` need.
///
/// Not shell-quoted: nothing here is ever run through a shell, and quoting it would suggest the
/// line could be pasted into one.
fn render_argv(argv: &[std::ffi::OsString]) -> String {
    argv.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
}

/// The topic: `--topic`, or the current branch's name.
///
/// The branch name is a deliberate default rather than a convenience. Pushing the *same* topic again
/// updates the same pull request and a different topic opens a second one, so the default has to be
/// stable across invocations — a branch name is, a timestamp or a random word would not be.
fn topic_for(rt: &Runtime, args: &Args) -> Result<String> {
    if let Some(topic) = &args.topic {
        let topic = topic.trim();
        if topic.is_empty() {
            return Err(Error::new(ErrorKind::Usage(
                "an AGit topic cannot be empty: Gitea refuses refs/for/<base> with no topic"
                    .to_owned(),
            )));
        }
        return Ok(topic.to_owned());
    }
    rt.git().current_branch()?.ok_or_else(|| {
        Error::new(ErrorKind::Usage(
            "an AGit pull request needs a topic, and HEAD is detached so there is no branch name \
             to use as one; pass --topic. Use the same topic again to update the pull request"
                .to_owned(),
        ))
    })
}

/// Find the pull request an AGit push created.
///
/// The server tells us: it prints the URL as a `remote:` line, which
/// [`gitea_core::context::git::agit::pull_index`] reads back — far more reliable than guessing
/// from the topic. Listing is the fallback for an instance whose message differs from the one we
/// know.
async fn locate(
    api: &Api,
    slug: &RepoSlug,
    reported: Option<i64>,
    base: &str,
    topic: &str,
) -> Result<Option<PullRequest>> {
    if let Some(index) = reported {
        return api.repo().get_pull_request(&slug.owner, &slug.name, index).await.map(Some);
    }
    let query = gitea_client::query::RepoListPullRequestsQuery::default()
        .with_state("open")
        .with_base_branch(base);
    let mut stream = api.repo().list_pull_requests(&slug.owner, &slug.name, &query).take(50);
    let mut best: Option<PullRequest> = None;
    while let Some(item) = stream.next().await {
        let pr = item?;
        let agit = common::is_agit(&pr);
        // Gitea blanks an AGit head's branch name in the API, so the topic is usually not there
        // to match on; an empty label is then the best evidence available, and the newest AGit
        // pull request against this base is the one the push just made.
        let matches_topic = pr.head.as_ref().is_some_and(|h| {
            h.label.is_empty() || h.r#ref.contains(topic) || h.label.contains(topic)
        });
        if agit && matches_topic && best.as_ref().is_none_or(|b| pr.number.get() > b.number.get()) {
            best = Some(pr);
        }
    }
    Ok(best)
}

// ------------------------------------------------------------------------------------------- misc

fn open_compare(rt: &Runtime, slug: &RepoSlug, base: &str, head: &str) -> Result<()> {
    let url =
        format!("{}/{slug}/compare/{base}...{head}", rt.client().web_base().trim_end_matches('/'));
    support::open_web(rt, &url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::context::git::{FakeGit, GitOutput, agit};

    /// `locate` asks for the number the *push* reported, and this is the behaviour it depends on:
    /// the pull request URL wins over the compare URL Gitea prints before it. Reading the first
    /// match would fetch pull request 0.
    #[test]
    fn the_pull_request_number_is_read_out_of_gits_output() {
        let output = "remote: \n\
             remote: Create a new pull request for 'main':\n\
             remote:   http://localhost:3000/me/proj/compare/main...me/topic\n\
             remote: \n\
             remote: Processed 1 references in total\n\
             remote: http://localhost:3000/me/proj/pulls/7\n";
        assert_eq!(agit::pull_index(output), Some(7));
        assert_eq!(agit::pull_index("remote: https://x/o/r/pull/3."), Some(3));
        assert_eq!(agit::pull_index("nothing here"), None);
    }

    /// The argv an AGit push produces, asserted against a fake rather than a server. The refspec
    /// *is* the protocol: `refs/for/<base>/<topic>` with the title and description as push
    /// options, and `--force` when the history was amended.
    #[test]
    fn an_agit_push_is_a_push_to_the_magic_ref_with_the_title_as_a_push_option() {
        let git = FakeGit::repo();
        let push = AgitPush::new("origin", AgitRef::new("main", "fix-parser").unwrap())
            .with_title("Fix the parser")
            .with_body("Fixes #12")
            .forced(true);
        git.push_agit(&push).expect("the fake accepts the push");
        assert_eq!(
            git.command_lines(),
            [
                "push --force origin HEAD:refs/for/main/fix-parser -o title=Fix the parser -o description=Fixes #12"
            ]
        );
    }

    /// Bug this prevents: a refusal reaching the user as "push failed" with the server's reason —
    /// and the `--force-push` remedy — dropped. It arrives inside the error rather than on stderr
    /// ahead of it, so that `gitea-core` can redact a credential embedded in the remote URL.
    ///
    /// Asserted on the **variant** and on what `error::render` makes of it, not on
    /// `Display`. `push_agit` now returns `ErrorKind::AgitRefused { refspec, remedy, stderr }`
    /// rather than a pre-formatted `Usage` string, so `to_string()` is only the headline and the
    /// advice lives where every other error's advice lives. That is the point of the variant:
    /// the remedy is classified once, from the scrubbed stderr, and the two can never disagree.
    #[test]
    fn a_refused_agit_push_carries_both_the_advice_and_gits_own_words() {
        use gitea_core::error::{AgitRemedy, ErrorKind, render};

        let git = FakeGit::repo().with_response(GitOutput::failure(
            1,
            "! [remote rejected] HEAD -> refs/for/main/t (non-fast-forward)",
        ));
        let push = AgitPush::new("origin", AgitRef::new("main", "t").unwrap()).with_title("x");
        let err = git.push_agit(&push).unwrap_err();

        let ErrorKind::AgitRefused { refspec, remedy, stderr } = &*err.kind else {
            panic!("a refused AGit push must be AgitRefused, not {:?}", err.kind())
        };
        assert_eq!(refspec, "HEAD:refs/for/main/t");
        assert_eq!(*remedy, AgitRemedy::ForcePush);
        assert!(stderr.contains("non-fast-forward"), "the server's own words: {stderr}");

        // ...and the rendered diagnostic still names this command's own flag, which is the
        // contract between the two crates.
        let shown = render::render(&err, render::Color::Never);
        assert!(shown.contains("--force-push"), "{shown}");
        assert!(shown.contains("non-fast-forward"), "{shown}");
    }

    /// Bug this prevents: a `--dry-run` body missing the head or base, which is the one thing a
    /// dry run is for.
    #[test]
    fn a_dry_run_body_carries_the_base_and_head() {
        let body = CreatePullRequestOption {
            title: Some("t".to_owned()),
            base: Some("main".to_owned()),
            head: Some("me:feature".to_owned()),
            ..CreatePullRequestOption::default()
        };
        let json = serde_json::to_value(&body).expect("serialisable");
        assert_eq!(json["base"], "main");
        assert_eq!(json["head"], "me:feature");
    }

    /// `--head` and `--agit` are contradictory: AGit has no head branch at all.
    #[test]
    fn agit_and_an_explicit_head_are_mutually_exclusive() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        assert!(
            <Harness as clap::Parser>::try_parse_from(["gea", "--agit", "-H", "branch"]).is_err()
        );
        // ...and --topic/--force-push are meaningless without it.
        assert!(<Harness as clap::Parser>::try_parse_from(["gea", "--topic", "t"]).is_err());
        assert!(<Harness as clap::Parser>::try_parse_from(["gea", "--force-push"]).is_err());
    }

    /// A draft is a title prefix on Gitea. If this stops holding, `--draft` silently creates a
    /// perfectly ordinary pull request.
    #[test]
    fn draft_is_expressed_as_a_title_prefix() {
        assert_eq!(common::add_wip("Add the thing"), "WIP: Add the thing");
        assert!(
            serde_json::to_value(CreatePullRequestOption::default())
                .expect("serialisable")
                .get("draft")
                .is_none(),
            "if CreatePullRequestOption gains a `draft` field, use it instead of the prefix"
        );
    }
}

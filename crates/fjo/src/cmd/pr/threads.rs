//! `fjo pr threads`, `reply`, `resolve` and `unresolve` — the inline half of a review.
//!
//! # What a thread is
//!
//! Forgejo has no thread object. Its web UI groups the code comments on a pull request into
//! "conversations" by **review, path and line**: two reviews commenting on the same line are two
//! conversations, and a comment added to a review on a line that review already commented on joins
//! that conversation (measured on 16.0.5, by reading the `conversation-holder`s the files page
//! renders). [`group`] rebuilds exactly that grouping from the per-review comment lists, so a
//! thread here is a conversation there.
//!
//! # Two routes Forgejo does not have
//!
//! Forgejo 16 has neither a reply route nor a resolve route in its API. Both are still possible,
//! because each is really something else:
//!
//! * **A reply is a comment added to the thread's review, on the thread's line.**
//!   `POST /pulls/{n}/reviews/{id}/comments` accepts one from any account that can comment — the
//!   pull request's author adding to the reviewer's review included — and the UI renders it inside
//!   the existing conversation. That is what the web UI's own reply form does too: it posts the
//!   review's id as `reply`.
//! * **Resolving is a web route**, `POST /{owner}/{repo}/issues/resolve_conversation`, the one the
//!   "Resolve conversation" button posts to. It needs a signed-in web session rather than a token
//!   (`fjo auth login --with-password`), and goes through [`crate::web`] as `fjo project` does.
//!   Forgejo 16 has no CSRF token there: its cross-origin protection allows a request with no
//!   `Origin`, which is what a CLI sends.
//!
//! # Resolving is a property of the first comment
//!
//! The resolve route accepts *any* code comment, and resolving a reply records a resolver on that
//! reply and nothing else — the conversation stays open, because the UI only reads the first
//! comment's resolver (its button always posts the first comment's id). `resolve` and
//! `reply --resolve` therefore map the id they were given to its thread's first comment, so any
//! comment in a thread names the thread.
//!
//! # Why the pull request is needed at all
//!
//! Finding a thread's first comment, its review and its line needs the pull request's comments. So
//! the pull request comes from `--pr`, from a pasted comment URL (`…/pulls/42#issuecomment-7`), or
//! from the branch you are on — the same inference every other `fjo pr` command makes.

use std::io::Write;

use clap::Args as ClapArgs;
use forgejo_client::Api;
use forgejo_core::http::Method;
use forgejo_core::types::RepoSlug;
use forgejo_core::types::ids::CommentId;
use forgejo_core::web::WebBody;
use forgejo_core::{Error, ErrorKind, Result};
use forgejo_model::{CreatePullReviewComment, PullReviewComment, ReviewStateType, User};
use futures::StreamExt;

use super::common;
use super::diff::sanitize;
use crate::cmd::support::{self, BodyOpts};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::{paint, style_by_name};
use crate::runtime::Runtime;

// ---------------------------------------------------------------------------------------- threads

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List the inline review threads on a pull request.

Each thread is headed by its location and the id of its first comment. Pass that id,
or the id of any comment in the thread, to `fjo pr reply` and `fjo pr resolve`.

  fjo pr threads
  fjo pr threads 42 --unresolved
  fjo pr threads --json id,path,position,body,resolver")]
pub struct ThreadsArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Only threads nobody has resolved
    #[arg(long, conflicts_with = "resolved")]
    pub unresolved: bool,

    /// Only resolved threads
    #[arg(long)]
    pub resolved: bool,
}

pub fn run_threads(globals: &GlobalOpts, args: &ThreadsArgs) -> Result<()> {
    let wanted =
        support::machine::plan(globals, forgejo_client::fields::FIELDS_PULL_REVIEW_COMMENT)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        let threads: Vec<Thread> = load(&api, &found.slug, found.index())
            .await?
            .into_iter()
            .filter(|t| keep(t, args))
            .collect();

        match &wanted {
            // The API's own comment objects, thread by thread: a script groups them by
            // `pull_request_review_id`, `path` and `position` the same way this file does, and
            // invented thread fields would be the one shape `--json` promises never to produce.
            support::machine::Wanted::Machine(m) => {
                let flat: Vec<&PullReviewComment> =
                    threads.iter().flat_map(|t| &t.comments).collect();
                support::machine::emit(&rt, globals, m, support::to_value(&flat)?)
            }
            _ => {
                if threads.is_empty() {
                    support::empty_note(rt.term(), "review threads");
                }
                let mut out = std::io::stdout().lock();
                out.write_all(render(&threads, rt.term()).as_bytes())?;
                out.flush()?;
                Ok(())
            }
        }
    })
}

fn keep(thread: &Thread, args: &ThreadsArgs) -> bool {
    let resolved = thread.resolver().is_some();
    !(args.unresolved && resolved || args.resolved && !resolved)
}

// ------------------------------------------------------------------------------------------ reply

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Reply to an inline review comment.

COMMENT is a comment id from `fjo pr threads`, or a comment URL copied from the web UI.
The reply joins that comment's thread. With --resolve, the thread is resolved afterwards,
which needs a web session: see `fjo auth login --with-password`.

  fjo pr reply 123 -b 'fixed in 4f2e1c9'
  fjo pr reply 123 --resolve -b 'renamed, as suggested'
  fjo pr reply https://git.example.org/o/r/pulls/42#issuecomment-123 -F reply.md")]
pub struct ReplyArgs {
    /// Comment id, or a comment URL
    #[arg(value_name = "COMMENT")]
    pub comment: String,

    /// The pull request the comment is on. Defaults to the URL's, then the branch you are on
    #[arg(long, value_name = "PR")]
    pub pr: Option<String>,

    #[command(flatten)]
    pub body: BodyOpts,

    /// Resolve the thread after replying
    #[arg(long)]
    pub resolve: bool,
}

pub fn run_reply(globals: &GlobalOpts, args: &ReplyArgs) -> Result<()> {
    let wanted =
        support::machine::plan(globals, forgejo_client::fields::FIELDS_PULL_REVIEW_COMMENT)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let target = CommentRef::parse(&args.comment)?;
        let selector = selector(args.pr.as_deref(), std::slice::from_ref(&target))?;
        let mut rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, selector.as_deref()).await?;
        let threads = load(&api, &found.slug, found.index()).await?;
        let thread =
            thread_of(&threads, target.id).ok_or_else(|| not_a_thread(target.id, &found))?;

        let supplied = args.body.resolve(&rt, "")?;
        let body = match supplied.into_text() {
            Some(b) if !b.trim().is_empty() => b,
            _ if support::can_prompt(&rt) => support::interact::ask("Reply", None)?,
            _ => return Err(support::missing("-b/--body or -F/--body-file", "a reply body")),
        };
        if body.trim().is_empty() {
            return Err(Error::new(ErrorKind::Cancelled));
        }

        let root = thread.root();
        let reply = api
            .repo()
            .create_pull_review_comment(
                &found.slug.owner,
                &found.slug.name,
                found.index(),
                root.pull_request_review_id,
                &reply_to(root, body),
            )
            .await?;

        if args.resolve {
            set_resolved(&mut rt, &found.slug, thread, true).await?;
        }

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&reply)?)
            }
            _ => {
                println!("{}", reply.html_url);
                Ok(())
            }
        }
    })
}

// ------------------------------------------------------------------------------ resolve/unresolve

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Resolve inline review threads.

Each COMMENT is a comment id from `fjo pr threads` or a comment URL. Any comment in a
thread names the whole thread. Forgejo has no API route for this, so it needs a web
session: see `fjo auth login --with-password`.

  fjo pr resolve 123
  fjo pr resolve 123 130 141 --pr 42")]
pub struct ResolveArgs {
    /// Comment ids or comment URLs, one per thread
    #[arg(value_name = "COMMENT", required = true)]
    pub comments: Vec<String>,

    /// The pull request the comments are on. Defaults to the URLs', then the branch you are on
    #[arg(long, value_name = "PR")]
    pub pr: Option<String>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Reopen resolved inline review threads.

  fjo pr unresolve 123
  fjo pr unresolve 123 --pr 42")]
pub struct UnresolveArgs {
    #[command(flatten)]
    pub inner: ResolveArgs,
}

pub fn run_resolve(globals: &GlobalOpts, args: &ResolveArgs) -> Result<()> {
    resolve_all(globals, args, true)
}

pub fn run_unresolve(globals: &GlobalOpts, args: &UnresolveArgs) -> Result<()> {
    resolve_all(globals, &args.inner, false)
}

fn resolve_all(globals: &GlobalOpts, args: &ResolveArgs, resolve: bool) -> Result<()> {
    crate::runtime::block_on(async move {
        let targets =
            args.comments.iter().map(|c| CommentRef::parse(c)).collect::<Result<Vec<_>>>()?;
        let selector = selector(args.pr.as_deref(), &targets)?;
        let mut rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, selector.as_deref()).await?;
        let threads = load(&api, &found.slug, found.index()).await?;

        // Every id is checked before anything is written, so a typo in the third id does not leave
        // the first two resolved and the user unsure which were.
        let mut chosen: Vec<&Thread> = Vec::new();
        for target in &targets {
            let thread =
                thread_of(&threads, target.id).ok_or_else(|| not_a_thread(target.id, &found))?;
            if !chosen.iter().any(|t| t.root().id == thread.root().id) {
                chosen.push(thread);
            }
        }
        for thread in chosen {
            set_resolved(&mut rt, &found.slug, thread, resolve).await?;
        }
        Ok(())
    })
}

/// A reply: a comment in the thread's review, on the thread's line and side.
pub(crate) fn reply_to(root: &PullReviewComment, body: String) -> CreatePullReviewComment {
    let new_side = root.position > 0;
    let line = |n: u64| i64::try_from(n).ok();
    CreatePullReviewComment {
        path: Some(root.path.clone()),
        body: Some(body),
        new_position: if new_side { line(root.position) } else { None },
        old_position: if new_side { None } else { line(root.original_position) },
        extra_lines_count: None,
    }
}

/// Resolve or reopen `thread` through its first comment, the only one the server's state reads.
async fn set_resolved(
    rt: &mut Runtime,
    slug: &RepoSlug,
    thread: &Thread,
    resolve: bool,
) -> Result<()> {
    let root = thread.root();
    let place = location(root);
    if thread.resolver().is_some() == resolve {
        let already = if resolve { "already resolved" } else { "not resolved" };
        support::note(rt.term(), &format!("{place} (comment {}) is {already}", root.id));
        return Ok(());
    }
    let path = format!("{}/{}/issues/resolve_conversation", slug.owner, slug.name);
    let form = WebBody::Form(resolve_form(root.id, resolve));
    let resp = crate::web::request(rt, Method::POST, &path, form).await?;
    // Success is the re-rendered conversation, as HTML; a refusal is a 400 or 500 page. A lapsed
    // session has already been renewed, or reported, by `web::request`.
    if !resp.status.is_success() {
        return Err(Error::new(ErrorKind::Usage(format!(
            "Forgejo answered {} to {path} for comment {}",
            resp.status, root.id
        ))));
    }
    let done = if resolve { "Resolved" } else { "Reopened" };
    support::note(rt.term(), &format!("{done} {place} (comment {})", root.id));
    Ok(())
}

/// The form the "Resolve conversation" button posts.
///
/// `origin` only chooses which partial the server renders back; `diff` is what the files page
/// sends. `action` is case-sensitive: anything but `Resolve` or `UnResolve` is a 400.
pub(crate) fn resolve_form(comment: i64, resolve: bool) -> Vec<(String, String)> {
    let action = if resolve { "Resolve" } else { "UnResolve" };
    [("origin", "diff"), ("action", action), ("comment_id", &comment.to_string())]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn not_a_thread(id: CommentId, found: &common::Found) -> Error {
    support::not_found("review comment", format!("{id} on #{}", found.pr.number), &found.slug)
}

// ---------------------------------------------------------------------------------- the grouping

/// One conversation: comments on one line, in one review, oldest first.
#[derive(Debug)]
pub(crate) struct Thread {
    pub comments: Vec<PullReviewComment>,
}

impl Thread {
    /// The first comment, which carries the thread's resolved state.
    pub fn root(&self) -> &PullReviewComment {
        &self.comments[0]
    }

    pub fn resolver(&self) -> Option<&User> {
        self.root().resolver.as_ref()
    }
}

/// Every inline thread on the pull request, oldest first.
///
/// `PENDING` reviews are skipped: they are the caller's own unsubmitted draft, nobody else can
/// see them, and a thread in one cannot be replied to or resolved. A review with no comments costs
/// no request.
pub(crate) async fn load(api: &Api, slug: &RepoSlug, index: i64) -> Result<Vec<Thread>> {
    let query = forgejo_client::query::RepoListPullReviewsQuery::default();
    let mut stream = api.repo().list_pull_reviews(&slug.owner, &slug.name, index, &query);
    let mut comments = Vec::new();
    while let Some(review) = stream.next().await {
        let review = review?;
        if review.state == ReviewStateType::Pending || review.comments_count == 0 {
            continue;
        }
        comments.extend(
            api.repo()
                .get_pull_review_comments(&slug.owner, &slug.name, index, review.id.get())
                .await?,
        );
    }
    Ok(group(comments))
}

/// Group comments the way Forgejo's UI does: by review, path and line.
///
/// `position` and `original_position` together are the line: Forgejo reports a comment on the new
/// side in the first and one on the old side in the second, so the pair distinguishes line 7 of
/// the new file from line 7 of the old.
pub(crate) fn group(mut comments: Vec<PullReviewComment>) -> Vec<Thread> {
    comments.sort_by_key(|c| c.id);
    let mut threads: Vec<Thread> = Vec::new();
    for comment in comments {
        let same = |t: &&mut Thread| {
            let r = t.root();
            r.pull_request_review_id == comment.pull_request_review_id
                && r.path == comment.path
                && r.position == comment.position
                && r.original_position == comment.original_position
        };
        match threads.iter_mut().find(same) {
            Some(thread) => thread.comments.push(comment),
            None => threads.push(Thread { comments: vec![comment] }),
        }
    }
    threads
}

/// The thread that comment `id` belongs to.
pub(crate) fn thread_of(threads: &[Thread], id: CommentId) -> Option<&Thread> {
    threads.iter().find(|t| t.comments.iter().any(|c| c.id == id.get()))
}

/// `src/lib.rs:12`, or `src/lib.rs:7 (old)` for a comment on a removed line.
pub(crate) fn location(comment: &PullReviewComment) -> String {
    let path = sanitize(&comment.path, false);
    match (comment.position, comment.original_position) {
        (0, 0) => path,
        (0, old) => format!("{path}:{old} (old)"),
        (new, _) => format!("{path}:{new}"),
    }
}

// --------------------------------------------------------------------------------- the selectors

/// A comment as the user named it: a bare id, or a URL that also names the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentRef {
    pub id: CommentId,
    /// The pull request URL the comment URL was under, without its fragment.
    pub pr_url: Option<String>,
}

impl CommentRef {
    /// `123`, `#123`, or `https://host/o/r/pulls/42#issuecomment-123`.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.contains("://") {
            let (pr_url, fragment) = raw.split_once('#').unwrap_or((raw, ""));
            let id = fragment.strip_prefix("issuecomment-").and_then(|n| n.parse().ok());
            return match (id, common::parse_url(pr_url)) {
                (Some(id), Some(_)) => Ok(Self { id, pr_url: Some(pr_url.to_owned()) }),
                _ => Err(support::usage(format!(
                    "{raw:?} is not a review comment URL; expected one ending in \
                     /pulls/<n>#issuecomment-<id>"
                ))),
            };
        }
        raw.parse()
            .map(|id| Self { id, pr_url: None })
            .map_err(|_| support::usage(format!("{raw:?} is not a comment id or a comment URL")))
    }
}

/// The pull request selector `common::find` should be given.
///
/// `--pr` and a URL both name the pull request, so passing both is refused rather than letting one
/// silently win; so are URLs that disagree with each other.
pub(crate) fn selector(flag: Option<&str>, refs: &[CommentRef]) -> Result<Option<String>> {
    let mut urls = refs.iter().filter_map(|r| r.pr_url.as_deref());
    let Some(first) = urls.next() else { return Ok(flag.map(str::to_owned)) };
    if flag.is_some() {
        return Err(support::usage(
            "--pr and a comment URL both name the pull request; pass one or the other",
        ));
    }
    let wanted = common::parse_url(first);
    if urls.any(|u| common::parse_url(u) != wanted) {
        return Err(support::usage("the comment URLs are on different pull requests"));
    }
    Ok(Some(first.to_owned()))
}

// ------------------------------------------------------------------------------------- rendering

/// Threads, each headed by its location, its first comment's id and its state.
///
/// Not a `Table`, for the reason `pr view --comments` gives: the bodies are prose, and a table
/// would truncate the text the command is run to read. Bodies and paths are sanitised the way
/// `pr diff` sanitises a diff — both are text somebody else chose.
pub(crate) fn render(threads: &[Thread], term: &Term) -> String {
    let dim = style_by_name("gray").unwrap_or_default();
    let bold = style_by_name("bold").unwrap_or_default();
    let green = style_by_name("green").unwrap_or_default();
    let yellow = style_by_name("yellow").unwrap_or_default();
    let mut out = String::new();
    for (n, thread) in threads.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let root = thread.root();
        out.push_str(&paint(term, bold, &location(root)));
        out.push_str(&paint(term, dim, &format!(" · comment {} · ", root.id)));
        out.push_str(&match thread.resolver() {
            Some(who) => paint(term, green, &format!("resolved by {}", who.login)),
            None => paint(term, yellow, "unresolved"),
        });
        out.push('\n');
        for line in hunk_tail(&root.diff_hunk) {
            out.push_str(&paint(term, dim, &format!("  │ {}\n", sanitize(line, false))));
        }
        for comment in &thread.comments {
            let who = comment.user.as_ref().map(|u| u.login.as_str()).unwrap_or_default();
            let when = comment
                .created_at
                .map(|ts| crate::output::template::funcs::timeago(&ts.to_string()))
                .unwrap_or_default();
            let meta = if when.is_empty() {
                format!(" • comment {}\n", comment.id)
            } else {
                format!(" • {when} • comment {}\n", comment.id)
            };
            out.push_str(&format!("  {}", paint(term, bold, who)));
            out.push_str(&paint(term, dim, &meta));
            for line in sanitize(&comment.body, false).lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    out
}

/// The last three lines of a hunk — the line commented on, and two of context above it.
///
/// Forgejo's hunk ends at the commented line, so the tail is the part worth showing; the whole hunk
/// can run to dozens of lines.
fn hunk_tail(hunk: &str) -> Vec<&str> {
    let lines: Vec<&str> = hunk.lines().filter(|l| !l.starts_with("@@")).collect();
    lines[lines.len().saturating_sub(3)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use forgejo_core::http::transport::Canned;

    fn comment(
        id: i64,
        review: i64,
        path: &str,
        new: u64,
        old: u64,
        who: &str,
    ) -> PullReviewComment {
        PullReviewComment {
            id,
            pull_request_review_id: review,
            path: path.to_owned(),
            position: new,
            original_position: old,
            body: format!("comment {id}"),
            user: Some(User { login: who.to_owned(), ..User::default() }),
            ..PullReviewComment::default()
        }
    }

    fn resolved(mut c: PullReviewComment, by: &str) -> PullReviewComment {
        c.resolver = Some(User { login: by.to_owned(), ..User::default() });
        c
    }

    /// Bug this prevents: a reply by the pull request's author shown as a thread of its own. It
    /// lands in the reviewer's review on the same line, and that is what makes it one conversation.
    #[test]
    fn a_reply_joins_its_thread_and_a_second_review_starts_another() {
        let threads = group(vec![
            comment(6, 1, "f.txt", 2, 0, "bob"),
            comment(2, 1, "f.txt", 2, 0, "bob"),
            comment(4, 1, "f.txt", 2, 0, "alice"),
            comment(8, 2, "f.txt", 2, 0, "carol"),
            comment(9, 1, "f.txt", 3, 0, "bob"),
        ]);
        let ids: Vec<Vec<i64>> =
            threads.iter().map(|t| t.comments.iter().map(|c| c.id).collect()).collect();
        assert_eq!(ids, vec![vec![2, 4, 6], vec![8], vec![9]]);
    }

    /// Line 7 of the old file and line 7 of the new one are different lines.
    #[test]
    fn the_old_and_new_side_of_the_same_number_are_different_threads() {
        let threads =
            group(vec![comment(1, 1, "f.txt", 7, 0, "bob"), comment(2, 1, "f.txt", 0, 7, "bob")]);
        assert_eq!(threads.len(), 2);
        assert_eq!(location(threads[0].root()), "f.txt:7");
        assert_eq!(location(threads[1].root()), "f.txt:7 (old)");
    }

    /// Bug this prevents: resolving a reply sets a resolver on the reply and leaves the
    /// conversation open, because the UI reads the first comment only (measured on 16.0.5). Any id in a
    /// thread must find the thread, and the thread's state must be its first comment's.
    #[test]
    fn any_comment_names_the_thread_and_only_the_first_carries_its_state() {
        let threads = group(vec![
            comment(2, 1, "f.txt", 2, 0, "bob"),
            resolved(comment(4, 1, "f.txt", 2, 0, "alice"), "alice"),
        ]);
        let thread = thread_of(&threads, CommentId::new(4)).expect("the reply's thread");
        assert_eq!(thread.root().id, 2);
        assert!(thread.resolver().is_none(), "a resolved reply does not resolve the thread");
        assert!(thread_of(&threads, CommentId::new(99)).is_none());
    }

    #[test]
    fn a_comment_is_named_by_id_or_by_the_url_the_web_ui_gives_it() {
        assert_eq!(
            CommentRef::parse("#123").unwrap(),
            CommentRef { id: CommentId::new(123), pr_url: None }
        );
        let url = CommentRef::parse("https://git.example.org/o/r/pulls/42#issuecomment-7").unwrap();
        assert_eq!(url.id, CommentId::new(7));
        assert_eq!(url.pr_url.as_deref(), Some("https://git.example.org/o/r/pulls/42"));
        for bad in ["abc", "https://git.example.org/o/r/pulls/42", "https://x/o/r#issuecomment-1"] {
            assert_eq!(CommentRef::parse(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
    }

    /// `--pr` and a URL naming different pull requests must not quietly pick one.
    #[test]
    fn the_pull_request_is_named_once() {
        let id = CommentRef::parse("5").unwrap();
        let a = CommentRef::parse("https://h/o/r/pulls/1#issuecomment-2").unwrap();
        let b = CommentRef::parse("https://h/o/r/pulls/3#issuecomment-4").unwrap();
        assert_eq!(selector(Some("9"), std::slice::from_ref(&id)).unwrap().as_deref(), Some("9"));
        assert_eq!(selector(None, std::slice::from_ref(&id)).unwrap(), None);
        assert_eq!(
            selector(None, &[id, a.clone()]).unwrap().as_deref(),
            Some("https://h/o/r/pulls/1")
        );
        assert!(selector(Some("9"), std::slice::from_ref(&a)).is_err());
        assert!(selector(None, &[a, b]).is_err());
    }

    /// Bug this prevents: `-e` losing the first line of a reply, which an editor session splits
    /// off as a title that a comment has nowhere to put.
    #[test]
    fn an_editor_reply_keeps_its_first_line() {
        let text = support::editor::split_editor_text("Good catch.\n\nFixed in 4f2e1c9.\n");
        assert_eq!(text.into_text().as_deref(), Some("Good catch.\n\nFixed in 4f2e1c9."));
        let one = support::editor::split_editor_text("thanks\n");
        assert_eq!(one.into_text().as_deref(), Some("thanks"));
    }

    /// A reply is placed by the thread's first comment: same path, same line, same side. On the
    /// wrong side it would start a new conversation instead of joining this one.
    #[test]
    fn a_reply_lands_on_its_threads_line_and_side() {
        let new = reply_to(&comment(2, 1, "f.txt", 7, 0, "bob"), "ok".to_owned());
        assert_eq!(
            serde_json::to_value(&new).unwrap(),
            serde_json::json!({"path": "f.txt", "body": "ok", "new_position": 7})
        );
        let old = reply_to(&comment(3, 1, "f.txt", 0, 4, "bob"), "ok".to_owned());
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({"path": "f.txt", "body": "ok", "old_position": 4})
        );
    }

    #[test]
    fn the_resolve_form_is_the_one_the_button_posts() {
        let pairs = |v: Vec<(String, String)>| -> Vec<String> {
            v.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
        };
        assert_eq!(pairs(resolve_form(2, true)), ["origin=diff", "action=Resolve", "comment_id=2"]);
        assert_eq!(pairs(resolve_form(2, false))[1], "action=UnResolve");
    }

    #[test]
    fn the_filters_read_the_threads_state() {
        let open = Thread { comments: vec![comment(1, 1, "f", 1, 0, "bob")] };
        let done = Thread { comments: vec![resolved(comment(2, 1, "f", 2, 0, "bob"), "al")] };
        let args = |unresolved, resolved| ThreadsArgs { pr: None, unresolved, resolved };
        assert!(keep(&open, &args(false, false)) && keep(&done, &args(false, false)));
        assert!(keep(&open, &args(true, false)) && !keep(&done, &args(true, false)));
        assert!(!keep(&open, &args(false, true)) && keep(&done, &args(false, true)));
    }

    /// Skips the draft and the comment-less reviews without a request, and asks for the rest.
    #[tokio::test]
    async fn only_submitted_reviews_with_comments_are_fetched() {
        let reviews = r#"[
            {"id":1,"state":"COMMENT","comments_count":2},
            {"id":2,"state":"PENDING","comments_count":1},
            {"id":3,"state":"APPROVED","comments_count":0}]"#;
        let comments = r#"[
            {"id":11,"pull_request_review_id":1,"path":"a.rs","position":3,"body":"why"},
            {"id":12,"pull_request_review_id":1,"path":"a.rs","position":3,"body":"because"}]"#;
        let fake = testing::on(
            testing::on(
                testing::transport(),
                "GET",
                "/api/v1/repos/o/r/pulls/42/reviews",
                testing::one_page(reviews),
            ),
            "GET",
            "/api/v1/repos/o/r/pulls/42/reviews/1/comments",
            Canned::json(200, comments),
        );
        let fake = std::sync::Arc::new(fake);
        let threads =
            load(&testing::api(fake.clone()), &RepoSlug::new("o", "r"), 42).await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].comments.len(), 2);
        let asked: Vec<String> =
            fake.calls().into_iter().map(|c| c.path).filter(|p| p.contains("/reviews")).collect();
        assert_eq!(
            asked,
            ["/api/v1/repos/o/r/pulls/42/reviews", "/api/v1/repos/o/r/pulls/42/reviews/1/comments"]
        );
    }

    #[test]
    fn threads_snapshot_for_a_terminal() {
        let mut root = comment(2, 1, "src/lib.rs", 12, 0, "bob");
        root.body = "this unwrap panics on an empty input".to_owned();
        root.diff_hunk =
            "@@ -10,2 +10,3 @@\n fn parse(s: &str) {\n     let x = 1;\n+    s.parse().unwrap()\n"
                .to_owned();
        let mut reply = comment(4, 1, "src/lib.rs", 12, 0, "alice");
        reply.body = "fixed in 4f2e1c9".to_owned();
        let mut old = resolved(comment(8, 2, "README.md", 0, 3, "carol"), "alice");
        old.body = "why remove this?".to_owned();
        let threads = group(vec![root, reply, old]);
        insta::assert_snapshot!(render(&threads, &Term::tty(80)));
    }

    /// Review text is somebody else's, and this prints it to a terminal.
    #[test]
    fn bodies_and_paths_are_sanitised() {
        let mut hostile = comment(1, 1, "a\x1b]0;x\x07.rs", 1, 0, "eve");
        hostile.body = "\x1b[2Jgotcha".to_owned();
        let out = render(&group(vec![hostile]), &Term::tty(80));
        assert!(!out.contains('\x1b'), "{out:?}");
    }
}

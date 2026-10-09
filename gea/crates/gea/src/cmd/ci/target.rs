//! What `gea ci` is asked about, and the runs that answer it.
//!
//! # Every target is a commit
//!
//! `gea ci [TARGET]` accepts a pull request, a branch, a commit, or nothing ("where I am"), and
//! every one of them is reduced to **one full SHA** before a single run is listed. That is the
//! point of the design, not an implementation detail: Gitea leaves `head_branch` empty on
//! `pull_request` runs, so `run list --branch X` silently misses every run a pull request
//! started, while `GET …/actions/runs?head_sha=<sha>` finds runs from every event alike.
//!
//! | you type | kind | the SHA is |
//! | --- | --- | --- |
//! | nothing | here | the current branch's, as the server has it (detached: local `HEAD`) |
//! | `42`, `#42`, a pull request URL | pull request | its `head.sha` |
//! | 7–40 hex characters with a letter in them | commit | that commit, expanded if short |
//! | anything else | branch | its open pull request's head, else the branch's tip |
//!
//! A branch prefers its **open pull request** because that is what CI ran against: a branch with
//! a pull request has `pull_request` runs keyed on the pull request's head, which is the same
//! commit, and saying "PR #2626" in the label tells the user which checks they are looking at.
//!
//! The commit rule demands a letter so that `1234567` stays a pull request number. A branch
//! whose name is all hex (`defaced`) reads as a commit first; when no such commit exists, the
//! lookup falls back to the branch rather than dead-ending.
//!
//! # Here means the server's view
//!
//! With no target, the branch is the checkout's, but the SHA is **the server's** for that
//! branch: CI ran on what was pushed, not on what is in the working tree. When the two differ, one
//! note says so, because "CI is green" about a commit that is not the one on screen is how
//! unpushed work gets merged untested.

use futures::StreamExt;
use gitea_client::{Api, query};
use gitea_core::context::RepoSource;
use gitea_core::context::git::GitCtx;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::{ActionWorkflowRun, PullRequest};

use crate::cmd::pr::common;
use crate::cmd::run::{is_finished, workflow_file};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// How many open pull requests to scan for one whose head is a branch.
///
/// Open ones only, so this is the open count rather than the repository's history; a repository
/// with more than this many open pull requests falls back to the branch's tip, which is the same
/// commit and only loses the "PR #N" in the label.
const OPEN_PR_SCAN: usize = 200;

/// Runs per page when listing a commit's runs. A commit has a handful of runs; this only bounds
/// how a pathological one is paged.
const RUNS_PAGE_SIZE: i32 = 50;

/// How many pages of runs a commit's run listing reads before giving up.
///
/// A commit has a handful of runs, so a server that honours `head_sha` answers in one page. The
/// cap is for one that ignores the filter and answers with every run in the repository: without
/// it, every `gea ci` — and every poll of `gea ci watch` — would page the repository's entire
/// history to keep the few runs that match. Hitting the cap is reported, since the list may then
/// be incomplete.
const RUNS_SCAN_PAGES: i32 = 10;

/// What the user asked about, before any request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// No positional: the current checkout.
    Here,
    /// A pull request selector, as `pr::common::find` takes it: `42`, `#42`, or a URL.
    Pr(String),
    /// A commit SHA, lower-cased; 7 to 40 hex characters.
    Commit(String),
    /// A branch name.
    Branch(String),
}

impl Target {
    /// Classify the optional positional. See the module docs for the table.
    ///
    /// Purely syntactic: nothing here asks the server or git, so the classification is the same
    /// on every machine and testable as a table.
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw else { return Ok(Target::Here) };
        let s = raw.trim();
        if s.is_empty() {
            return Err(support::usage(
                "the target is empty; pass a pull request (42), a branch, or a commit SHA — or \
                 omit it for the branch you are on",
            ));
        }
        let number = s.strip_prefix('#').unwrap_or(s);
        if (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
            || common::parse_url(s).is_some()
        {
            return Ok(Target::Pr(s.to_owned()));
        }
        if looks_like_sha(s) {
            return Ok(Target::Commit(s.to_ascii_lowercase()));
        }
        Ok(Target::Branch(s.to_owned()))
    }
}

/// 7 to 40 hex characters, at least one of them a letter.
fn looks_like_sha(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_hexdigit())
        && s.bytes().any(|b| b.is_ascii_alphabetic())
}

/// A full, 40-character hex SHA — the only thing the `head_sha` filter matches.
fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The first seven characters of a SHA, for labels and notes.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// A target, resolved to the commit whose runs answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The repository the runs live in — a pull request URL can name a different one from the
    /// checkout's.
    pub slug: RepoSlug,
    /// The full 40-hex SHA.
    pub sha: String,
    /// For humans: `PR #2626 (renovate/zip-9.x)`, `branch main`, `commit f36d1a7`.
    pub label: String,
    /// What to put after `gea ci <verb>` in a hint so it resolves to the same thing: the target
    /// as the user gave it, or `None` for "here", which must stay implicit.
    pub selector: Option<String>,
}

/// Resolve the optional positional of a `gea ci` subcommand.
///
/// Errors: a pull request, branch or commit that does not exist is `ResourceNotFound` (exit 5);
/// no positional outside a checkout is a usage error naming the positional.
pub(crate) async fn resolve_target(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    raw: Option<&str>,
) -> Result<Resolved> {
    match Target::parse(raw)? {
        Target::Pr(sel) => {
            let found = common::find(rt, globals, api, Some(&sel)).await?;
            // A number is handed back as the number; a URL as the URL, because it may name a
            // repository other than the one `-R`/the checkout resolves to.
            let selector =
                if common::parse_url(&sel).is_some() { sel } else { found.index().to_string() };
            from_pr(found.slug, &found.pr, Some(selector))
        }
        Target::Here => {
            // Before the repository: outside a checkout, "you did not say what" is the problem,
            // and a repository-resolution error would bury it.
            let here = locate_here(rt.git())?;
            let slug = rt.repo(globals)?.slug.clone();
            resolve_here(rt.git(), api, &slug, here).await
        }
        Target::Branch(b) => {
            let slug = rt.repo(globals)?.slug.clone();
            resolve_branch(api, &slug, &b, Some(b.clone())).await
        }
        Target::Commit(c) => {
            let ctx = rt.repo(globals)?;
            // The checkout's objects only say anything about the checkout's repository.
            let local = from_checkout(&ctx.source).then(|| rt.git());
            resolve_commit(local, api, &ctx.slug, &c).await
        }
    }
}

/// A pull request's head commit.
fn from_pr(slug: RepoSlug, pr: &PullRequest, selector: Option<String>) -> Result<Resolved> {
    let head = pr.head.as_ref();
    let sha = head.map(|h| h.sha.to_ascii_lowercase()).unwrap_or_default();
    if !is_full_sha(&sha) {
        return Err(support::usage(format!(
            "pull request #{} reports no head commit, so there is no CI to look up for it",
            pr.number
        )));
    }
    // The branch as people know it. An AGit pull request has no branch, only `refs/pull/N/head`,
    // and a label of that reads worse than none.
    let branch = head
        .map(|h| if h.label.is_empty() { h.r#ref.clone() } else { h.label.clone() })
        .filter(|b| !b.is_empty() && !b.starts_with("refs/"));
    let label = match branch {
        Some(b) => format!("PR #{} ({b})", pr.number),
        None => format!("PR #{}", pr.number),
    };
    Ok(Resolved { slug, sha, label, selector })
}

/// A branch: its open pull request's head when it has one, else its tip.
async fn resolve_branch(
    api: &Api,
    slug: &RepoSlug,
    branch: &str,
    selector: Option<String>,
) -> Result<Resolved> {
    if let Some(pr) = open_pr_for_branch(api, slug, branch).await? {
        return from_pr(slug.clone(), &pr, selector);
    }
    let found = match api.repo().get_branch(&slug.owner, &slug.name, branch).await {
        Ok(b) => b,
        Err(e) if is_missing(&e) => return Err(support::not_found("branch", branch, slug)),
        Err(e) => return Err(e),
    };
    let sha = found.commit.map(|c| c.id.to_ascii_lowercase()).unwrap_or_default();
    if !is_full_sha(&sha) {
        return Err(support::not_found("branch", branch, slug));
    }
    Ok(Resolved { slug: slug.clone(), label: format!("branch {branch}"), sha, selector })
}

/// The newest open pull request whose head is `branch` **in this repository**, when there is one.
///
/// A fork's pull request is never "the branch's pull request": a fork opening a PR from its own
/// `main` has a head labelled `main` too, and matching on the name alone made `gea ci main` report
/// the fork's commit as the base repository's `main`. See [`head_is_base_repo`].
async fn open_pr_for_branch(
    api: &Api,
    slug: &RepoSlug,
    branch: &str,
) -> Result<Option<PullRequest>> {
    let open = query::RepoListPullRequestsQuery::default().with_state("open");
    let mut stream =
        api.repo().list_pull_requests(&slug.owner, &slug.name, &open).take(OPEN_PR_SCAN);
    let mut best: Option<PullRequest> = None;
    while let Some(item) = stream.next().await {
        let pr = item?;
        if common::head_matches(&pr, branch)
            && head_is_base_repo(&pr, slug)
            && best.as_ref().is_none_or(|b| pr.number.get() > b.number.get())
        {
            best = Some(pr);
        }
    }
    Ok(best)
}

/// Whether the pull request's head branch lives in the repository it targets — not a fork's.
///
/// Repository ids when the server sends both (a renamed or transferred repository keeps its id);
/// otherwise the head repository's `full_name` against `slug`, case-insensitively as Gitea's names
/// are. When neither is known the answer is no: the cost of a wrong no is only the "PR #N" in the
/// label (the branch's tip is the same commit), while a wrong yes reports another repository's CI.
fn head_is_base_repo(pr: &PullRequest, slug: &RepoSlug) -> bool {
    let Some(head) = &pr.head else { return false };
    let base_id = pr.base.as_ref().map_or(0, |b| b.repo_id);
    let head_id = head.repo.as_ref().map_or(head.repo_id, |r| r.id.get());
    if base_id != 0 && head_id != 0 {
        return base_id == head_id;
    }
    head.repo
        .as_ref()
        .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&format!("{}/{}", slug.owner, slug.name)))
}

/// Whether the repository was inferred from the current checkout's remotes, so that the
/// checkout's objects are that repository's objects.
///
/// `-R` and `$GEA_REPO` can name any repository; expanding a short SHA against the local clone
/// of a different one finds the wrong commit (or one the named repository does not have).
fn from_checkout(source: &RepoSource) -> bool {
    matches!(source, RepoSource::Remote { .. } | RepoSource::GitConfig { .. })
}

/// A commit, expanded to its full SHA.
///
/// A full SHA is taken as it is: listing runs for it is the existence check, and an empty list
/// is an honest answer. A short one is expanded locally when `git` is given — the repository is
/// the checkout's — (no request), and through the API otherwise. A short SHA no commit has falls
/// back to a branch of that name, for the `defaced` case the module docs describe.
async fn resolve_commit(
    git: Option<&dyn GitCtx>,
    api: &Api,
    slug: &RepoSlug,
    given: &str,
) -> Result<Resolved> {
    let full = if is_full_sha(given) {
        given.to_owned()
    } else {
        match expand_sha(git, api, slug, given).await {
            Ok(full) => full,
            Err(e) if is_missing(&e) => {
                return match resolve_branch(api, slug, given, Some(given.to_owned())).await {
                    Ok(r) => Ok(r),
                    // Neither: report the commit, which is what the input looked like.
                    Err(b) if is_missing(&b) => Err(support::not_found("commit", given, slug)),
                    Err(b) => Err(b),
                };
            }
            Err(e) => return Err(e),
        }
    };
    Ok(Resolved {
        slug: slug.clone(),
        label: format!("commit {}", short(&full)),
        sha: full,
        selector: Some(given.to_owned()),
    })
}

/// A short SHA as a full one: local `git` first (when given), the server second.
///
/// `<sha>^{commit}` rather than the bare SHA, because `git rev-parse --verify` succeeds for any
/// well-formed 40-hex string whether or not the object exists, and for a short one it would
/// happily resolve a tree or a blob. A local miss is not final — the commit may simply not have
/// been fetched — so it falls through to the server rather than failing.
///
/// Both answers must **start with what was typed**. `rev-parse` (and the server's commit lookup)
/// resolve ref names too, so a hex-looking branch `deadbeef` would otherwise come back as that
/// branch's tip — locally, possibly an unpushed one — labelled "commit deadbeef". A result that
/// is not a prefix match is not this commit; the caller then tries the branch.
async fn expand_sha(
    git: Option<&dyn GitCtx>,
    api: &Api,
    slug: &RepoSlug,
    short: &str,
) -> Result<String> {
    let short = short.to_ascii_lowercase();
    let short = short.as_str();
    let expands = |full: &str| is_full_sha(full) && full.starts_with(short);
    if let Some(git) = git
        && git.git_dir()?.is_some()
    {
        let out = git.try_run(&["rev-parse", "--verify", &format!("{short}^{{commit}}")])?;
        let full = out.stdout.trim().to_ascii_lowercase();
        if out.ok && expands(&full) {
            return Ok(full);
        }
    }
    // Only the SHA is wanted, so skip the parts of the answer that cost the server most.
    let q = query::RepoGetSingleCommitQuery {
        files: Some(false),
        stat: Some(false),
        verification: Some(false),
    };
    let commit = api.repo().get_single_commit(&slug.owner, &slug.name, short, &q).await?;
    let full = commit.sha.to_ascii_lowercase();
    if expands(&full) { Ok(full) } else { Err(support::not_found("commit", short, slug)) }
}

/// Where "here" is, locally, before anything is asked of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Here {
    /// On a branch.
    Branch(String),
    /// Detached, at this full SHA.
    Detached(String),
}

fn locate_here(git: &dyn GitCtx) -> Result<Here> {
    if git.git_dir()?.is_none() {
        return Err(support::usage(
            "not inside a git checkout, so there is no branch to show CI for; pass a [TARGET]: \
             a pull request (42), a branch, or a commit SHA",
        ));
    }
    if let Some(branch) = git.current_branch()? {
        return Ok(Here::Branch(branch));
    }
    let head = local_head(git).ok_or_else(|| {
        support::usage(
            "HEAD is detached and does not resolve to a commit; pass a [TARGET]: a pull request \
             (42), a branch, or a commit SHA",
        )
    })?;
    Ok(Here::Detached(head))
}

/// `git rev-parse HEAD`, when it names a commit — `None` in a repository with no commits yet.
fn local_head(git: &dyn GitCtx) -> Option<String> {
    let out = git.try_run(&["rev-parse", "HEAD"]).ok()?;
    let sha = out.stdout.trim().to_ascii_lowercase();
    (out.ok && is_full_sha(&sha)).then_some(sha)
}

async fn resolve_here(
    git: &dyn GitCtx,
    api: &Api,
    slug: &RepoSlug,
    here: Here,
) -> Result<Resolved> {
    match here {
        Here::Branch(branch) => {
            let resolved = resolve_branch(api, slug, &branch, None).await?;
            if let Some(local) = local_head(git).filter(|l| *l != resolved.sha) {
                support::warn(&format!(
                    "note: local HEAD {} is not what the server has for {branch} ({}); \
                         showing the server's. Push to run CI on your commits.",
                    short(&local),
                    short(&resolved.sha)
                ));
            }
            Ok(resolved)
        }
        Here::Detached(sha) => Ok(Resolved {
            slug: slug.clone(),
            label: format!("commit {} (detached HEAD)", short(&sha)),
            sha,
            selector: None,
        }),
    }
}

/// A 404 that means "no such branch/commit", in either shape `http::classify` gives it.
fn is_missing(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ResourceNotFound { .. } | ErrorKind::RepoNotFound { probed: false, .. }
    )
}

// ------------------------------------------------------------------------------------- runs

/// Every Actions run for one commit.
///
/// The server filters on `head_sha`; the result is filtered again here. Not redundancy for its
/// own sake: an instance that does not know the parameter ignores it and answers with every run
/// in the repository, and `gea ci` would then report some other commit's red run as this one's.
/// On such an instance the scan is bounded (see [`scan_runs_for_sha`]), and hitting the bound is
/// said once on stderr rather than passed off as the complete list.
pub(crate) async fn runs_for_sha(
    api: &Api,
    slug: &RepoSlug,
    sha: &str,
) -> Result<Vec<ActionWorkflowRun>> {
    let scan = scan_runs_for_sha(api, slug, sha).await?;
    if scan.capped {
        support::warn(&format!(
            "warning: {slug} appears to ignore the head_sha filter; read {RUNS_SCAN_PAGES} pages \
             of runs and stopped, so runs for {} may be missing",
            short(sha)
        ));
    }
    Ok(scan.runs)
}

/// What [`scan_runs_for_sha`] found, and whether it stopped at [`RUNS_SCAN_PAGES`].
#[derive(Debug)]
struct RunScan {
    runs: Vec<ActionWorkflowRun>,
    capped: bool,
}

/// The paging behind [`runs_for_sha`], separate so the stop rules are testable without stderr.
///
/// Stops at the first of: an empty page or the collection's end; a page with no run for `sha`
/// once one has been seen (runs are listed newest first, so a commit's runs are contiguous enough
/// that a page past them means they are over — this is what keeps a filter-ignoring server cheap
/// in the common case of a recent commit); [`RUNS_SCAN_PAGES`] pages, reported as `capped`.
async fn scan_runs_for_sha(api: &Api, slug: &RepoSlug, sha: &str) -> Result<RunScan> {
    let mut runs: Vec<ActionWorkflowRun> = Vec::new();
    let mut listed: i64 = 0;
    for page in 1..=RUNS_SCAN_PAGES {
        let q = query::GetWorkflowRunsQuery {
            head_sha: Some(sha.to_owned()),
            page: Some(page),
            limit: Some(RUNS_PAGE_SIZE),
            ..Default::default()
        };
        let resp = api.run().list(&slug.owner, &slug.name, &q).await?;
        let got = resp.workflow_runs.len();
        listed += got as i64;
        let before = runs.len();
        runs.extend(
            resp.workflow_runs.into_iter().filter(|r| r.head_sha.eq_ignore_ascii_case(sha)),
        );
        let matched_here = runs.len() > before;
        if got == 0 || listed >= resp.total_count || (before > 0 && !matched_here) {
            return Ok(RunScan { runs, capped: false });
        }
    }
    Ok(RunScan { runs, capped: true })
}

/// Where a run stands, in the order `gea ci` lists runs: what needs attention first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CiState {
    /// Finished with `failure`, `cancelled` or `timed_out`.
    Failed,
    /// Not finished: queued, waiting or in progress.
    Pending,
    /// Finished with `success` — or `skipped`, which blocks nothing and so counts as passing.
    Passed,
    /// Finished with a conclusion this build does not recognise.
    Other,
}

/// Classify one run. The exit-code mapping (`CiFailed`/`CiPending`) and the listing order both
/// read this, so they cannot disagree about what "failed" means.
pub(crate) fn ci_state(run: &ActionWorkflowRun) -> CiState {
    if !is_finished(&run.status) {
        return CiState::Pending;
    }
    match run.conclusion.as_str() {
        "failure" | "cancelled" | "timed_out" => CiState::Failed,
        "success" | "skipped" => CiState::Passed,
        _ => CiState::Other,
    }
}

/// The newest run of each workflow, failed first.
///
/// One commit collects several runs of the same workflow — a re-push of the same SHA, a manual
/// re-trigger, a cancelled duplicate — and only the newest says anything about the commit now:
/// reporting a superseded red run next to its green replacement is how a CI summary cries wolf.
/// "Same workflow" is `(path, event)`, so a workflow that runs on both `push` and `pull_request`
/// keeps one run of each; the highest id wins, ids being allocated in creation order.
///
/// Ordered failed → pending → passed → other, then by workflow file, so the line a reader needs
/// is the first one.
pub(crate) fn latest_per_workflow(runs: &[ActionWorkflowRun]) -> Vec<&ActionWorkflowRun> {
    let mut latest: Vec<&ActionWorkflowRun> = Vec::new();
    for run in runs {
        match latest.iter_mut().find(|r| r.path == run.path && r.event == run.event) {
            Some(slot) if slot.id.get() < run.id.get() => *slot = run,
            Some(_) => {}
            None => latest.push(run),
        }
    }
    sort_for_display(&mut latest);
    latest
}

/// [`latest_per_workflow`]'s order, for `--all`, which skips the de-duplication but not the
/// ordering.
pub(crate) fn sort_for_display(runs: &mut [&ActionWorkflowRun]) {
    runs.sort_by(|a, b| {
        (ci_state(a), workflow_file(&a.path), &a.event, std::cmp::Reverse(a.id.get())).cmp(&(
            ci_state(b),
            workflow_file(&b.path),
            &b.event,
            std::cmp::Reverse(b.id.get()),
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing::{self, on, on_fn, paged};
    use gitea_core::context::git::{FakeGit, GitOutput};
    use gitea_core::http::transport::Canned;
    use gitea_core::types::ids::RunId;
    use std::sync::Arc;

    const SHA: &str = "f36d1a7c0ffee0ddba11f00dfeedface0b5e55ed";
    const OTHER: &str = "0123456789abcdef0123456789abcdef01234567";

    fn slug() -> RepoSlug {
        RepoSlug::new("o", "r")
    }

    #[test]
    fn every_target_form_is_classified() {
        let cases: &[(Option<&str>, Target)] = &[
            (None, Target::Here),
            (Some("42"), Target::Pr("42".into())),
            (Some("#42"), Target::Pr("#42".into())),
            (
                Some("https://git.example.org/o/r/pulls/42"),
                Target::Pr("https://git.example.org/o/r/pulls/42".into()),
            ),
            // All digits is a pull request number even at SHA length: the letter rule exists so
            // a number never turns into a commit.
            (Some("1234567"), Target::Pr("1234567".into())),
            (Some("f36d1a7"), Target::Commit("f36d1a7".into())),
            (Some("F36D1A7"), Target::Commit("f36d1a7".into())),
            (Some(SHA), Target::Commit(SHA.into())),
            (Some("main"), Target::Branch("main".into())),
            (Some("renovate/zip-9.x"), Target::Branch("renovate/zip-9.x".into())),
            // Too short for a SHA, and hex: still a branch.
            (Some("cafe"), Target::Branch("cafe".into())),
            // Too long for a SHA.
            (Some(&"a".repeat(41)), Target::Branch("a".repeat(41))),
            (Some("#main"), Target::Branch("#main".into())),
        ];
        for (raw, want) in cases {
            assert_eq!(Target::parse(*raw).as_ref().ok(), Some(want), "{raw:?}");
        }
    }

    #[test]
    fn an_empty_target_is_refused() {
        for raw in ["", "  "] {
            let err = Target::parse(Some(raw)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{raw:?}");
        }
    }

    /// A same-repository pull request, as Gitea sends it: base and head share a repository id.
    fn pr_json(number: i64, branch: &str, sha: &str) -> String {
        pr_json_from(number, branch, sha, 1)
    }

    /// A pull request whose head lives in the repository `head_repo_id` (the base is id 1).
    fn pr_json_from(number: i64, branch: &str, sha: &str, head_repo_id: i64) -> String {
        format!(
            r#"{{"number":{number},"state":"open","base":{{"ref":"main","repo_id":1}},"head":{{"ref":"{branch}","label":"{branch}","sha":"{sha}","repo_id":{head_repo_id}}}}}"#
        )
    }

    fn with_settings(fake: gitea_core::http::FakeTransport) -> gitea_core::http::FakeTransport {
        on(fake, "GET", "/api/v1/settings/api", Canned::json(200, r#"{"max_response_items":50}"#))
    }

    /// The design's regression: a branch with a pull request resolves to the pull request, so
    /// its `pull_request` runs — which carry no branch — are the ones found.
    #[tokio::test]
    async fn a_branch_with_an_open_pull_request_resolves_to_that_pull_request() {
        let list =
            format!("[{},{}]", pr_json(7, "other", OTHER), pr_json(2626, "renovate/zip-9.x", SHA));
        let fake = Arc::new(on_fn(
            with_settings(testing::transport()),
            "GET",
            "/api/v1/repos/o/r/pulls",
            move |c| paged(c, &list),
        ));
        let r = resolve_branch(
            &testing::api(fake.clone()),
            &slug(),
            "renovate/zip-9.x",
            Some("renovate/zip-9.x".into()),
        )
        .await
        .unwrap();
        assert_eq!(r.sha, SHA);
        assert_eq!(r.label, "PR #2626 (renovate/zip-9.x)");
        assert_eq!(r.selector.as_deref(), Some("renovate/zip-9.x"));
        let listed = testing::calls(&fake, "GET", "/api/v1/repos/o/r/pulls");
        assert!(listed[0].query.contains("state=open"), "{:?}", listed[0].query);
    }

    /// Bug this prevents: a fork's pull request from *its* `main` matched `gea ci main` by head
    /// name, so the base repository's `main` was reported as the fork's commit.
    #[tokio::test]
    async fn a_forks_pull_request_from_a_same_named_branch_is_not_the_branchs() {
        let list = format!("[{}]", pr_json_from(9, "main", OTHER, 2));
        let fake = Arc::new(on(
            on_fn(
                with_settings(testing::transport()),
                "GET",
                "/api/v1/repos/o/r/pulls",
                move |c| paged(c, &list),
            ),
            "GET",
            "/api/v1/repos/o/r/branches/main",
            Canned::json(200, format!(r#"{{"name":"main","commit":{{"id":"{SHA}"}}}}"#)),
        ));
        let r = resolve_branch(&testing::api(fake), &slug(), "main", Some("main".into()))
            .await
            .unwrap();
        assert_eq!((r.sha.as_str(), r.label.as_str()), (SHA, "branch main"));
    }

    /// The fork test's other half: without ids, the head repository's name decides, and it is
    /// compared case-insensitively as Gitea's names are.
    #[test]
    fn the_head_repository_is_matched_by_id_else_by_name() {
        let pr = |json: &str| serde_json::from_str::<PullRequest>(json).unwrap();
        let same = pr(&pr_json(1, "main", SHA));
        let fork = pr(&pr_json_from(1, "main", SHA, 2));
        let named = pr(r#"{"number":1,"head":{"ref":"x","repo":{"full_name":"O/R"}}}"#);
        let other = pr(r#"{"number":1,"head":{"ref":"x","repo":{"full_name":"them/r"}}}"#);
        let unknown = pr(r#"{"number":1,"head":{"ref":"x"}}"#);
        assert!(head_is_base_repo(&same, &slug()));
        assert!(!head_is_base_repo(&fork, &slug()));
        assert!(head_is_base_repo(&named, &slug()));
        assert!(!head_is_base_repo(&other, &slug()));
        assert!(!head_is_base_repo(&unknown, &slug()), "unknown is not ours");
    }

    #[tokio::test]
    async fn a_branch_without_a_pull_request_resolves_to_its_tip() {
        let fake = Arc::new(on(
            on_fn(with_settings(testing::transport()), "GET", "/api/v1/repos/o/r/pulls", |c| {
                paged(c, "[]")
            }),
            "GET",
            "/api/v1/repos/o/r/branches/main",
            Canned::json(200, format!(r#"{{"name":"main","commit":{{"id":"{SHA}"}}}}"#)),
        ));
        let r = resolve_branch(&testing::api(fake), &slug(), "main", Some("main".into()))
            .await
            .unwrap();
        assert_eq!((r.sha.as_str(), r.label.as_str()), (SHA, "branch main"));
    }

    #[tokio::test]
    async fn a_branch_that_does_not_exist_is_not_found() {
        let fake = Arc::new(on(
            on_fn(with_settings(testing::transport()), "GET", "/api/v1/repos/o/r/pulls", |c| {
                paged(c, "[]")
            }),
            "GET",
            "/api/v1/repos/o/r/branches/nope",
            Canned::json(404, r#"{"message":"branch does not exist"}"#),
        ));
        let err = resolve_branch(&testing::api(fake), &slug(), "nope", None).await.unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
        assert!(err.to_string().contains("branch nope"), "{err}");
    }

    /// Bug this prevents (and a remembered gotcha): `git rev-parse --verify <sha>` without
    /// `^{commit}` succeeds for objects that are not commits, and for any well-formed full SHA.
    #[tokio::test]
    async fn a_short_sha_is_expanded_locally_as_a_commit() {
        let git =
            FakeGit::repo().with_response(GitOutput::success().with_stdout(format!("{SHA}\n")));
        let fake = Arc::new(testing::transport());
        let r = resolve_commit(Some(&git), &testing::api(fake.clone()), &slug(), "f36d1a7")
            .await
            .unwrap();
        assert_eq!(r.sha, SHA);
        assert_eq!(r.label, "commit f36d1a7");
        assert_eq!(r.selector.as_deref(), Some("f36d1a7"));
        assert_eq!(git.command_lines(), ["rev-parse --verify f36d1a7^{commit}"]);
        assert_eq!(fake.call_count(), 0, "a local expansion costs no request");
    }

    #[tokio::test]
    async fn a_short_sha_outside_a_checkout_is_expanded_by_the_server() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/git/commits/f36d1a7",
            Canned::json(200, format!(r#"{{"sha":"{SHA}"}}"#)),
        ));
        let r = resolve_commit(
            Some(&FakeGit::not_a_repo()),
            &testing::api(fake.clone()),
            &slug(),
            "f36d1a7",
        )
        .await
        .unwrap();
        assert_eq!(r.sha, SHA);
        let q = &fake.calls()[0].query;
        assert!(q.contains("files=false") && q.contains("stat=false"), "{q}");
    }

    /// Bug this prevents: `rev-parse deadbeef^{commit}` resolves a local *branch* named
    /// `deadbeef` to its (possibly unpushed) tip, which was then labelled "commit deadbeef". A
    /// result that does not start with the input is not that commit; the branch is tried instead.
    #[tokio::test]
    async fn a_local_expansion_that_is_not_a_prefix_match_is_a_branch_name() {
        let git =
            FakeGit::repo().with_response(GitOutput::success().with_stdout(format!("{OTHER}\n")));
        let fake = Arc::new(on(
            on(
                on_fn(with_settings(testing::transport()), "GET", "/api/v1/repos/o/r/pulls", |c| {
                    paged(c, "[]")
                }),
                "GET",
                "/api/v1/repos/o/r/git/commits/deadbeef",
                Canned::json(200, format!(r#"{{"sha":"{OTHER}"}}"#)),
            ),
            "GET",
            "/api/v1/repos/o/r/branches/deadbeef",
            Canned::json(200, format!(r#"{{"name":"deadbeef","commit":{{"id":"{SHA}"}}}}"#)),
        ));
        let r = resolve_commit(Some(&git), &testing::api(fake), &slug(), "deadbeef").await.unwrap();
        // The server's commit lookup resolved the ref name as well; that is refused the same way.
        assert_eq!((r.sha.as_str(), r.label.as_str()), (SHA, "branch deadbeef"));
    }

    /// Bug this prevents: with `-R other/repo`, the short SHA was expanded against the local
    /// checkout — a different repository's objects.
    #[tokio::test]
    async fn a_repository_not_from_the_checkout_expands_on_the_server_only() {
        let git =
            FakeGit::repo().with_response(GitOutput::success().with_stdout(format!("{OTHER}\n")));
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/git/commits/f36d1a7",
            Canned::json(200, format!(r#"{{"sha":"{SHA}"}}"#)),
        ));
        let r = resolve_commit(None, &testing::api(fake), &slug(), "f36d1a7").await.unwrap();
        assert_eq!(r.sha, SHA);
        assert!(git.command_lines().is_empty());
        assert!(!from_checkout(&RepoSource::Flag));
        assert!(!from_checkout(&RepoSource::Env { var: "GEA_REPO" }));
        assert!(from_checkout(&RepoSource::Remote { remote: "origin".into(), score: 3 }));
    }

    /// A full SHA is used as it is: no git, no request.
    #[tokio::test]
    async fn a_full_sha_needs_no_lookup() {
        let git = FakeGit::repo();
        let fake = Arc::new(testing::transport());
        let r =
            resolve_commit(Some(&git), &testing::api(fake.clone()), &slug(), SHA).await.unwrap();
        assert_eq!(r.sha, SHA);
        assert!(git.command_lines().is_empty());
        assert_eq!(fake.call_count(), 0);
    }

    /// `defaced` is a valid branch name and a valid short SHA. When no commit has it, the
    /// branch is what was meant.
    #[tokio::test]
    async fn a_hex_branch_name_falls_back_from_commit_to_branch() {
        let fake = Arc::new(on(
            on(
                on_fn(with_settings(testing::transport()), "GET", "/api/v1/repos/o/r/pulls", |c| {
                    paged(c, "[]")
                }),
                "GET",
                "/api/v1/repos/o/r/git/commits/defaced",
                Canned::json(404, r#"{"message":"sha not found"}"#),
            ),
            "GET",
            "/api/v1/repos/o/r/branches/defaced",
            Canned::json(200, format!(r#"{{"name":"defaced","commit":{{"id":"{SHA}"}}}}"#)),
        ));
        let r =
            resolve_commit(Some(&FakeGit::not_a_repo()), &testing::api(fake), &slug(), "defaced")
                .await
                .unwrap();
        assert_eq!(r.label, "branch defaced");
    }

    #[test]
    fn here_outside_a_checkout_is_a_usage_error_naming_the_positional() {
        let err = locate_here(&FakeGit::not_a_repo()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("[TARGET]"), "{err}");
    }

    #[test]
    fn here_on_a_detached_head_is_the_local_commit() {
        let git =
            FakeGit::repo().with_response(GitOutput::success().with_stdout(format!("{SHA}\n")));
        assert_eq!(locate_here(&git).unwrap(), Here::Detached(SHA.into()));
        let git = FakeGit::repo().with_branch("main");
        assert_eq!(locate_here(&git).unwrap(), Here::Branch("main".into()));
    }

    /// Here resolves to the *server's* commit for the branch, not the working tree's.
    #[tokio::test]
    async fn here_uses_the_servers_commit_for_the_branch() {
        let git = FakeGit::repo()
            .with_branch("main")
            .with_response(GitOutput::success().with_stdout(format!("{OTHER}\n")));
        let fake = Arc::new(on(
            on_fn(with_settings(testing::transport()), "GET", "/api/v1/repos/o/r/pulls", |c| {
                paged(c, "[]")
            }),
            "GET",
            "/api/v1/repos/o/r/branches/main",
            Canned::json(200, format!(r#"{{"name":"main","commit":{{"id":"{SHA}"}}}}"#)),
        ));
        let here = locate_here(&git).unwrap();
        let r = resolve_here(&git, &testing::api(fake), &slug(), here).await.unwrap();
        assert_eq!(r.sha, SHA);
        assert_eq!(r.selector, None, "hints for here must not name a target");
        assert!(
            git.command_lines().iter().any(|l| l == "rev-parse HEAD"),
            "{:?}",
            git.command_lines()
        );
    }

    // ------------------------------------------------------------------------------- runs

    fn run(id: i64, path: &str, event: &str, status: &str, conclusion: &str) -> ActionWorkflowRun {
        ActionWorkflowRun {
            id: RunId::new(id),
            path: path.into(),
            event: event.into(),
            status: status.into(),
            conclusion: conclusion.into(),
            head_sha: SHA.into(),
            ..Default::default()
        }
    }

    /// Every page is read, the SHA goes to the server as `head_sha`, and a run for another
    /// commit (an instance ignoring the filter) is dropped.
    #[tokio::test]
    async fn runs_for_a_sha_reads_every_page_and_keeps_only_that_sha() {
        let fake = Arc::new(on_fn(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            |c| {
                let page: i64 = c.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let body = match page {
                    1 => format!(
                        r#"{{"total_count":3,"workflow_runs":[{{"id":3,"head_sha":"{SHA}"}},{{"id":2,"head_sha":"{OTHER}"}}]}}"#
                    ),
                    _ => format!(
                        r#"{{"total_count":3,"workflow_runs":[{{"id":1,"head_sha":"{SHA}"}}]}}"#
                    ),
                };
                Canned::json(200, &body)
            },
        ));
        let runs = runs_for_sha(&testing::api(fake.clone()), &slug(), SHA).await.unwrap();
        assert_eq!(runs.iter().map(|r| r.id.get()).collect::<Vec<_>>(), [3, 1]);
        assert_eq!(fake.call_count(), 2);
        assert!(fake.calls()[0].query.contains(&format!("head_sha={SHA}")), "{:?}", fake.calls());
    }

    /// A full page of runs, none of them for `SHA` — what a server ignoring `head_sha` sends.
    fn foreign_page(page: i64) -> String {
        let runs: Vec<String> = (0..RUNS_PAGE_SIZE as i64)
            .map(|i| format!(r#"{{"id":{},"head_sha":"{OTHER}"}}"#, 100_000 - page * 100 - i))
            .collect();
        format!(r#"{{"total_count":100000,"workflow_runs":[{}]}}"#, runs.join(","))
    }

    /// Bug this prevents: on a server that ignores `head_sha`, every `gea ci` (and every watch
    /// poll) paged the repository's entire history. The scan stops at the cap and says so.
    #[tokio::test]
    async fn a_server_ignoring_head_sha_is_scanned_up_to_the_cap() {
        let fake =
            Arc::new(on_fn(testing::transport(), "GET", "/api/v1/repos/o/r/actions/runs", |c| {
                let page: i64 = c.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                Canned::json(200, foreign_page(page))
            }));
        let scan = scan_runs_for_sha(&testing::api(fake.clone()), &slug(), SHA).await.unwrap();
        assert!(scan.capped);
        assert!(scan.runs.is_empty());
        assert_eq!(fake.call_count(), RUNS_SCAN_PAGES as usize);
    }

    /// Runs are newest first, so once a commit's runs have been seen, a page without any means
    /// they are over — the common case on a filter-ignoring server costs a page or two, not ten.
    #[tokio::test]
    async fn the_scan_stops_at_the_first_page_without_a_match_after_one() {
        let fake = Arc::new(on_fn(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            |c| {
                let page: i64 = c.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let body = match page {
                    1 => format!(
                        r#"{{"total_count":100000,"workflow_runs":[{{"id":9,"head_sha":"{SHA}"}},{{"id":8,"head_sha":"{OTHER}"}}]}}"#
                    ),
                    _ => foreign_page(page),
                };
                Canned::json(200, &body)
            },
        ));
        let scan = scan_runs_for_sha(&testing::api(fake.clone()), &slug(), SHA).await.unwrap();
        assert!(!scan.capped);
        assert_eq!(scan.runs.iter().map(|r| r.id.get()).collect::<Vec<_>>(), [9]);
        assert_eq!(fake.call_count(), 2);
    }

    /// Bug this prevents: a cancelled duplicate or a superseded failure being reported next to
    /// the run that replaced it, so a green commit reads as red.
    #[test]
    fn superseded_runs_are_dropped_and_the_rest_are_ordered_by_what_needs_attention() {
        let runs = vec![
            run(10, "ci.yml@refs/heads/main", "push", "completed", "failure"),
            run(14, "ci.yml@refs/heads/main", "push", "completed", "success"),
            run(11, "release.yml@refs/heads/main", "push", "completed", "failure"),
            run(12, "lint.yml@refs/heads/main", "push", "in_progress", ""),
            run(13, "ci.yml@refs/pull/4/merge", "pull_request", "completed", "cancelled"),
            run(15, "zz.yml@refs/heads/main", "push", "completed", "skipped"),
            run(16, "aa.yml@refs/heads/main", "push", "completed", "neutral"),
        ];
        let got: Vec<(i64, CiState)> =
            latest_per_workflow(&runs).iter().map(|r| (r.id.get(), ci_state(r))).collect();
        assert_eq!(
            got,
            [
                (13, CiState::Failed),  // ci.yml, pull_request: a different workflow key
                (11, CiState::Failed),  // release.yml
                (12, CiState::Pending), // lint.yml
                (14, CiState::Passed),  // ci.yml push: superseded run 10 is gone
                (15, CiState::Passed),  // skipped counts as passing
                (16, CiState::Other),
            ]
        );
    }
}

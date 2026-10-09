//! What `fjo ci` is asked about, and the runs that answer it.
//!
//! # Every target is a commit
//!
//! `fjo ci [TARGET]` accepts a pull request, a branch, a commit, or nothing ("where I am"), and
//! every one of them is reduced to **one full SHA** before a single run is listed. That is the
//! point of the design, not an implementation detail: Forgejo files a `pull_request` run under
//! the pull request rather than its branch (measured: `prettyref` reads `#12`, and
//! `?ref=refs/heads/<branch>` finds only the branch's `push` runs), so `run list --ref` silently
//! misses every run a pull request started, while `GET …/actions/runs?head_sha=<sha>` finds runs
//! from every event alike.
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

use forgejo_client::{Api, query};
use forgejo_core::context::git::GitCtx;
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::types::RepoSlug;
use forgejo_model::{ActionRun, PullRequest};
use futures::StreamExt;

use crate::cmd::pr::common;
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
    /// What to put after `fjo ci <verb>` in a hint so it resolves to the same thing: the target
    /// as the user gave it, or `None` for "here", which must stay implicit.
    pub selector: Option<String>,
}

/// Resolve the optional positional of a `fjo ci` subcommand.
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
            let slug = rt.repo(globals)?.slug.clone();
            resolve_commit(rt.git(), api, &slug, &c).await
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

/// The newest open pull request whose head is `branch`, when there is one.
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
            && best.as_ref().is_none_or(|b| pr.number.get() > b.number.get())
        {
            best = Some(pr);
        }
    }
    Ok(best)
}

/// A commit, expanded to its full SHA.
///
/// A full SHA is taken as it is: listing runs for it is the existence check, and an empty list
/// is an honest answer. A short one is expanded locally when there is a checkout (no request),
/// and through the API otherwise. A short SHA no commit has falls back to a branch of that name,
/// for the `defaced` case the module docs describe.
async fn resolve_commit(
    git: &dyn GitCtx,
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

/// A short SHA as a full one: local `git` first, the server second.
///
/// `<sha>^{commit}` rather than the bare SHA, because `git rev-parse --verify` succeeds for any
/// well-formed 40-hex string whether or not the object exists, and for a short one it would
/// happily resolve a tree or a blob. A local miss is not final — the commit may simply not have
/// been fetched — so it falls through to the server rather than failing.
async fn expand_sha(git: &dyn GitCtx, api: &Api, slug: &RepoSlug, short: &str) -> Result<String> {
    if git.git_dir()?.is_some() {
        let out = git.try_run(&["rev-parse", "--verify", &format!("{short}^{{commit}}")])?;
        let full = out.stdout.trim().to_ascii_lowercase();
        if out.ok && is_full_sha(&full) {
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
    if is_full_sha(&full) { Ok(full) } else { Err(support::not_found("commit", short, slug)) }
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

/// Every Actions run for one commit, all pages.
///
/// The server filters on `head_sha`; the result is filtered again here (on the run's
/// `commit_sha`). Not redundancy for its own sake: an instance that does not know the parameter
/// ignores it and answers with every run in the repository, and `fjo ci` would then report some
/// other commit's red run as this one's.
pub(crate) async fn runs_for_sha(api: &Api, slug: &RepoSlug, sha: &str) -> Result<Vec<ActionRun>> {
    let mut runs: Vec<ActionRun> = Vec::new();
    for page in 1.. {
        let q = query::ListActionRunsQuery {
            head_sha: Some(sha.to_owned()),
            page: Some(page),
            limit: Some(RUNS_PAGE_SIZE),
            ..Default::default()
        };
        let resp = api.run().list(&slug.owner, &slug.name, &q).await?;
        let got = resp.workflow_runs.len();
        runs.extend(resp.workflow_runs);
        if got == 0 || runs.len() as i64 >= resp.total_count {
            break;
        }
    }
    runs.retain(|r| r.commit_sha.eq_ignore_ascii_case(sha));
    Ok(runs)
}

/// Where a run stands, in the order `fjo ci` lists runs: what needs attention first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CiState {
    /// `failure` or `cancelled`.
    Failed,
    /// Not finished: `waiting`, `running`, `blocked`, `unknown`.
    Pending,
    /// `success` — or `skipped`, which blocks nothing and so counts as passing.
    Passed,
    /// A status this build does not recognise.
    Other,
}

/// Classify one run. The exit-code mapping (`CiFailed`/`CiPending`) and the listing order both
/// read this, so they cannot disagree about what "failed" means.
///
/// Forgejo's `status` *is* the outcome — there is no `completed` + `conclusion` pair as on
/// Gitea. And a run reads `failure` as soon as one job fails, while its other jobs may still be
/// running (measured); that is the right answer here, because no later job can turn it green.
pub(crate) fn ci_state(run: &ActionRun) -> CiState {
    match run.status.as_str() {
        "failure" | "cancelled" => CiState::Failed,
        "success" | "skipped" => CiState::Passed,
        "waiting" | "running" | "blocked" | "unknown" => CiState::Pending,
        _ => CiState::Other,
    }
}

/// The newest run of each workflow, failed first.
///
/// One commit collects several runs of the same workflow — a re-push of the same SHA, a manual
/// re-trigger, a cancelled duplicate — and only the newest says anything about the commit now:
/// reporting a superseded red run next to its green replacement is how a CI summary cries wolf.
/// "Same workflow" is `(workflow_id, event)` — Forgejo's `workflow_id` is the file name — so a
/// workflow that runs on both `push` and `pull_request` keeps one run of each; the highest id
/// wins, ids being allocated in creation order.
///
/// Ordered failed → pending → passed → other, then by workflow file, so the line a reader needs
/// is the first one.
pub(crate) fn latest_per_workflow(runs: &[ActionRun]) -> Vec<&ActionRun> {
    let mut latest: Vec<&ActionRun> = Vec::new();
    for run in runs {
        match latest.iter_mut().find(|r| r.workflow_id == run.workflow_id && r.event == run.event) {
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
pub(crate) fn sort_for_display(runs: &mut [&ActionRun]) {
    runs.sort_by(|a, b| {
        (ci_state(a), &a.workflow_id, &a.event, std::cmp::Reverse(a.id.get())).cmp(&(
            ci_state(b),
            &b.workflow_id,
            &b.event,
            std::cmp::Reverse(b.id.get()),
        ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing::{self, on, on_fn, paged};
    use forgejo_core::context::git::{FakeGit, GitOutput};
    use forgejo_core::http::transport::Canned;
    use forgejo_core::types::ids::RunId;
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

    fn pr_json(number: i64, branch: &str, sha: &str) -> String {
        format!(
            r#"{{"number":{number},"state":"open","head":{{"ref":"{branch}","label":"{branch}","sha":"{sha}"}}}}"#
        )
    }

    fn with_settings(fake: forgejo_core::http::FakeTransport) -> forgejo_core::http::FakeTransport {
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
        let r =
            resolve_commit(&git, &testing::api(fake.clone()), &slug(), "f36d1a7").await.unwrap();
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
        let r =
            resolve_commit(&FakeGit::not_a_repo(), &testing::api(fake.clone()), &slug(), "f36d1a7")
                .await
                .unwrap();
        assert_eq!(r.sha, SHA);
        let q = &fake.calls()[0].query;
        assert!(q.contains("files=false") && q.contains("stat=false"), "{q}");
    }

    /// A full SHA is used as it is: no git, no request.
    #[tokio::test]
    async fn a_full_sha_needs_no_lookup() {
        let git = FakeGit::repo();
        let fake = Arc::new(testing::transport());
        let r = resolve_commit(&git, &testing::api(fake.clone()), &slug(), SHA).await.unwrap();
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
        let r = resolve_commit(&FakeGit::not_a_repo(), &testing::api(fake), &slug(), "defaced")
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

    fn run(id: i64, workflow: &str, event: &str, status: &str) -> ActionRun {
        ActionRun {
            id: RunId::new(id),
            workflow_id: workflow.into(),
            event: event.into(),
            status: status.into(),
            commit_sha: SHA.into(),
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
                        r#"{{"total_count":3,"workflow_runs":[{{"id":3,"commit_sha":"{SHA}"}},{{"id":2,"commit_sha":"{OTHER}"}}]}}"#
                    ),
                    _ => format!(
                        r#"{{"total_count":3,"workflow_runs":[{{"id":1,"commit_sha":"{SHA}"}}]}}"#
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

    /// Bug this prevents: a cancelled duplicate or a superseded failure being reported next to
    /// the run that replaced it, so a green commit reads as red.
    #[test]
    fn superseded_runs_are_dropped_and_the_rest_are_ordered_by_what_needs_attention() {
        let runs = vec![
            run(10, "ci.yml", "push", "failure"),
            run(14, "ci.yml", "push", "success"),
            run(11, "release.yml", "push", "failure"),
            run(12, "lint.yml", "push", "running"),
            run(13, "ci.yml", "pull_request", "cancelled"),
            run(15, "zz.yml", "push", "skipped"),
            run(16, "aa.yml", "push", "neutral"),
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

    /// Every status Forgejo documents lands in exactly one bucket. Bug this prevents: `waiting`
    /// (no runner has it yet) or `blocked` (an unmet `needs:`) reading as passed, so `fjo ci`
    /// exits 0 for CI that has not run.
    #[test]
    fn every_forgejo_status_is_classified() {
        let state = |s: &str| ci_state(&run(1, "ci.yml", "push", s));
        assert_eq!(state("failure"), CiState::Failed);
        assert_eq!(state("cancelled"), CiState::Failed);
        for s in ["waiting", "running", "blocked", "unknown"] {
            assert_eq!(state(s), CiState::Pending, "{s}");
        }
        assert_eq!(state("success"), CiState::Passed);
        assert_eq!(state("skipped"), CiState::Passed);
        assert_eq!(state("something-new"), CiState::Other);
    }
}

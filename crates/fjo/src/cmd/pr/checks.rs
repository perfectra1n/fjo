//! `fjo pr checks` and `fjo pr status`.
//!
//! # Exit 8 when checks are pending
//!
//! `gh pr checks` exits **0** when everything passed, **1** when something failed, and **8** when
//! checks are still running. That third code is the whole reason the command is scriptable:
//!
//! ```sh
//! fjo pr checks; case $? in 0) merge;; 8) sleep 60;; *) investigate;; esac
//! ```
//!
//! `fjo` matches it, through the taxonomy rather than around it:
//! [`forgejo_core::ErrorKind::ChecksPending`] exits 8 and renders "nothing has failed — the checks
//! simply have not finished", with the still-running check names as facts. Exit codes come from
//! [`forgejo_core::ErrorKind::exit_code`] and from nowhere else, so the one table cannot drift
//! from `docs/output.md`.
//!
//! [`Verdict::Failed`] goes through the taxonomy too, as
//! [`forgejo_core::ErrorKind::ChecksFailed`], which exits **1**. It used to be the last
//! `std::process::exit` in the tree, for want of a variant: `ChecksPending` is deliberately *not*
//! a flavour of "checks failed", and every other exit-1 variant would print advice about
//! something that did not happen — `Conflict` and `StateConflict` announce a server refusal that
//! never came, and `RunFailed` points at `fjo run view <run>`, which is wrong for any check
//! posted through the commit-status API by external CI. `ChecksFailed` adds no prose the table
//! did not already print; its whole job is that the exit code comes from
//! [`forgejo_core::ErrorKind::exit_code`] like every other status, where a test can observe it
//! without spawning a process.
//!
//! # A pull request with no CI at all
//!
//! There is a standing complaint that fetching a pull request's status errors out on repositories
//! with no CI. `GET /repos/{owner}/{repo}/commits/{sha}/status` on such a commit answers with an
//! object whose `state` is the empty string and whose `statuses` list is empty — which is not an
//! error, and must not be reported as a decode failure or as "checks failed". Here it is reported as
//! *no checks*, with exit 0, because a repository that runs no CI has not failed anything.

use std::io::Write;

use clap::Args as ClapArgs;
use forgejo_client::Api;
use forgejo_core::context::{RepoContext, RepoSource};
use forgejo_core::error::FailedCheck;
use forgejo_core::types::RepoSlug;
use forgejo_core::{Error, ErrorKind, Result};
use forgejo_model::{CombinedStatus, CommitStatus};

use super::common;
use crate::cmd::run_ref::parse_run_url;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::autocolor;
use crate::output::table::Table;
use crate::output::template::funcs::timeago;
use crate::runtime::Runtime;

/// `gh`'s exit code for "the checks have not finished yet".
pub const EXIT_PENDING: i32 = 8;

/// `gh`'s exit code for "a check failed".
pub const EXIT_FAILED: i32 = 1;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show CI checks for a pull request.

Exit codes: 0 for passed or no checks, 1 for failed checks, 8 for pending checks.

  fjo pr checks
  fjo pr checks 42
  fjo pr checks --json state,statuses
  fjo pr checks; case $? in 0) echo green;; 8) echo waiting;; *) echo red;; esac")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Open the pull request's checks in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a pull request's state, checks, and merge status.

Without an argument, uses the pull request for the current branch.

  fjo pr status
  fjo pr status 42")]
pub struct StatusArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, forgejo_client::fields::FIELDS_COMBINED_STATUS)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        if args.web {
            return support::open_web(&rt, &format!("{}/checks", found.pr.html_url));
        }

        let Some(sha) = head_sha(&found.pr) else {
            support::note(
                rt.term(),
                &format!("#{} has no head commit, so there is nothing to check", found.pr.number),
            );
            return Ok(());
        };
        let status = combined(&api, &found.slug, &sha).await?;

        if let support::machine::Wanted::Machine(m) = &wanted {
            write_report(
                Some(m),
                &status,
                rt.term(),
                &found,
                false,
                &mut support::writer(globals)?,
                &mut std::io::sink(),
            )?;
        } else {
            let scoped = needs_repo_flag(rt.repo(globals).ok(), &found.slug);
            write_report(
                None,
                &status,
                rt.term(),
                &found,
                scoped,
                &mut std::io::stdout().lock(),
                &mut std::io::stderr().lock(),
            )?;
            if status.statuses.is_empty() {
                support::empty_note(rt.term(), "checks");
            }
        }
        finish(verdict(&status), &found.slug, &found.pr.number.to_string(), &status)
    })
}

pub fn run_status(globals: &GlobalOpts, args: &StatusArgs) -> Result<()> {
    let wanted = support::machine::plan(globals, forgejo_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        let checks =
            status_for(&api, &found, !matches!(wanted, support::machine::Wanted::Machine(_))).await;

        common::emit_or(&rt, globals, &wanted, &found.pr, || {
            let mut out = std::io::stdout().lock();
            out.write_all(common::detail(&found.pr, rt.term(), false).as_bytes())?;
            let mut table = Table::new(rt.term());
            table.row([
                "mergeable".to_owned(),
                if found.pr.merged {
                    "already merged".to_owned()
                } else if found.pr.mergeable {
                    "yes".to_owned()
                } else {
                    "no (conflict or merge rule)".to_owned()
                },
            ]);
            table.row([
                "checks".to_owned(),
                match &checks {
                    Some(s) if s.statuses.is_empty() => "none configured".to_owned(),
                    Some(s) => summary(s),
                    None => "unknown".to_owned(),
                },
            ]);
            table.row(["comments".to_owned(), found.pr.comments.to_string()]);
            table.row(["review comments".to_owned(), found.pr.review_comments.to_string()]);
            out.write_all(b"\n")?;
            out.write_all(table.render_to_string().as_bytes())?;
            out.flush()?;
            Ok(())
        })
    })
}

/// The combined status, but only when something is going to print it.
///
/// Two rules, both load-bearing:
///
/// * Best-effort. `pr status` is a summary, and a repository whose status endpoint is unavailable
///   should still get the pull request's state and mergeability rather than an error — which is
///   why this answers `Option` and not `Result`.
/// * Bug this prevents: `fjo pr status --json state` paying for a status request that only the
///   human table reads. `emit_or` renders the pull request alone on the machine path, and
///   `pr status --json` is precisely the shape a script polls in a loop, so that wasted call was
///   being made once a second.
///
/// Takes a bare [`Api`] rather than the [`Runtime`] it comes from, so a `FakeTransport` test can
/// assert the request this does *not* make without a network or a config file.
async fn status_for(api: &Api, found: &common::Found, wanted: bool) -> Option<CombinedStatus> {
    if !wanted {
        return None;
    }
    let sha = head_sha(&found.pr)?;
    combined(api, &found.slug, &sha).await.ok()
}

/// Write what `pr checks` prints: the document through `machine` when there is one, otherwise the
/// table, the summary line and the `next:` hint.
///
/// One function for both paths so a test can hold the machine bytes against the human ones: the
/// summary and the hint are human output only, and a script parsing `--json` must see exactly
/// the document it always did.
///
/// Where the summary and hint go depends on the layout, the same switch that makes the table TSV:
/// on a terminal they follow the table on `out`; piped, `out` stays pure TSV (`fjo pr checks |
/// cut -f2` keeps working, as it does with `gh`) and they go to `err` instead, which an agent
/// capturing both streams still reads.
fn write_report(
    machine: Option<&support::machine::Triad>,
    status: &CombinedStatus,
    term: &Term,
    found: &common::Found,
    scoped: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    if let Some(m) = machine {
        return m.render(support::to_value(status)?, term, out);
    }
    out.write_all(render(status, term).as_bytes())?;
    let footer = footer(status, &found.slug, found.index(), scoped);
    if term.tty {
        out.write_all(footer.as_bytes())?;
        out.flush()?;
    } else {
        out.flush()?;
        // No blank separator line on stderr: there is no table above it to set the summary off from.
        err.write_all(footer.trim_start_matches('\n').as_bytes())?;
        err.flush()?;
    }
    Ok(())
}

/// The commit the checks are attached to.
fn head_sha(pr: &forgejo_model::PullRequest) -> Option<String> {
    pr.head.as_ref().map(|h| h.sha.clone()).filter(|s| !s.is_empty())
}

/// Fetch the combined status, treating "this repository has no CI" as an empty result.
///
/// A 404 on the status endpoint means the *commit* is not known to the instance — which happens for
/// an AGit pull request whose commits have been garbage-collected, and for a fork whose objects were
/// never mirrored. Neither is a check failure, so both come back as an empty status rather than as an
/// error the user cannot act on.
async fn combined(api: &Api, slug: &RepoSlug, sha: &str) -> Result<CombinedStatus> {
    let query = forgejo_client::query::RepoGetCombinedStatusByRefQuery::default();
    match api.repo().get_combined_status_by_ref(&slug.owner, &slug.name, sha, &query).await {
        Ok(status) => Ok(status),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. }
            ) =>
        {
            Ok(CombinedStatus { sha: sha.to_owned(), ..CombinedStatus::default() })
        }
        Err(e) => Err(e),
    }
}

/// What the checks add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    None,
    Passed,
    Pending,
    Failed,
}

/// Decide from the individual statuses, not from `state`.
///
/// `CombinedStatus.state` is one value, and on a repository with no CI it is the empty string, which
/// no enum arm means. Reading the statuses list is unambiguous: nothing there is `None`, any failure
/// is `Failed` (a failure outranks a pending sibling, because waiting for a run that already failed
/// is a wasted loop), any pending is `Pending`, otherwise `Passed`.
pub fn verdict(status: &CombinedStatus) -> Verdict {
    if status.statuses.is_empty() {
        return Verdict::None;
    }
    let mut pending = false;
    for check in &status.statuses {
        match check.status.as_str() {
            "failure" | "error" => return Verdict::Failed,
            "pending" | "" => pending = true,
            _ => {}
        }
    }
    if pending { Verdict::Pending } else { Verdict::Passed }
}

/// Turn a verdict into this command's result.
///
/// Pending is an `Err(ChecksPending)`, which exits 8 through
/// [`forgejo_core::ErrorKind::exit_code`] like every other status in the tool, and carries the
/// names of the checks still running so the message can say what is being waited on.
///
/// Failed is an `Err(ChecksFailed)`, which exits 1 the same way, and carries the checks that
/// failed with their own `target_url`s — the only remedy a status check can offer, since anything
/// holding a token can post one and there is no run id to point at.
///
/// Neither arm prints anything itself. The table rendered directly above already names every
/// check; the error's job is that the exit code comes from the one table in
/// [`forgejo_core::ErrorKind::exit_code`], where a unit test can read it without spawning a
/// process. This function used to end in `std::process::exit`, the last such call in the tree.
fn finish(verdict: Verdict, slug: &RepoSlug, pr: &str, status: &CombinedStatus) -> Result<()> {
    match verdict {
        // Nothing to wait for and nothing broken.
        Verdict::None | Verdict::Passed => Ok(()),
        Verdict::Pending => Err(Error::new(ErrorKind::ChecksPending {
            slug: Some(slug.to_string()),
            pr: pr.to_owned(),
            pending: pending_names(status),
        })),
        Verdict::Failed => Err(Error::new(ErrorKind::ChecksFailed {
            slug: Some(slug.to_string()),
            pr: pr.to_owned(),
            failed: failed_checks(status),
        })),
    }
}

/// The checks that failed, with their own URLs — what `ChecksFailed` prints.
fn failed_checks(status: &CombinedStatus) -> Vec<FailedCheck> {
    status
        .statuses
        .iter()
        .filter(|c| matches!(c.status.as_str(), "failure" | "error"))
        .map(|c| FailedCheck::new(display_name(c), Some(c.target_url.clone())))
        .collect()
}

/// The checks still running, by name — what `ChecksPending` prints under `still running`.
fn pending_names(status: &CombinedStatus) -> Vec<String> {
    status
        .statuses
        .iter()
        .filter(|c| matches!(c.status.as_str(), "pending" | ""))
        .map(display_name)
        .collect()
}

pub(crate) fn render(status: &CombinedStatus, term: &Term) -> String {
    if status.statuses.is_empty() {
        return String::new();
    }
    let mut t = Table::new(term);
    t.headers(["CHECK", "STATE", "AGE", "DETAILS"]);
    for check in &status.statuses {
        t.row([
            display_name(check),
            autocolor(term, &state_word(check)),
            check.updated_at.map(|ts| timeago(&ts.to_string())).unwrap_or_default(),
            check.target_url.clone(),
        ]);
    }
    t.render_to_string()
}

/// How one check counts towards the summary line.
///
/// The same reading [`verdict`] uses, so the line can never disagree with the exit code:
/// `failure`/`error` failed, `pending` or empty (queued) is pending, `skipped` is skipped, and
/// everything else — `success`, `warning`, and any state a newer server invents — passed, because
/// that is what `verdict` treats it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tally {
    Passed,
    Failed,
    Pending,
    Skipped,
}

fn tally(check: &CommitStatus) -> Tally {
    match check.status.as_str() {
        "failure" | "error" => Tally::Failed,
        "pending" | "" => Tally::Pending,
        "skipped" => Tally::Skipped,
        _ => Tally::Passed,
    }
}

/// Whether a `next:` hint has to carry `-R owner/name` to reach the same repository.
///
/// It does unless the repository the hint would resolve on its own — from the checkout, `git
/// config` or `$FJO_REPO` — is the one the pull request lives in. `-R` itself does not survive
/// into the next command, and a pull request named by URL can live anywhere.
fn needs_repo_flag(context: Option<&RepoContext>, slug: &RepoSlug) -> bool {
    context.is_none_or(|ctx| matches!(ctx.source, RepoSource::Flag) || !same_repo(&ctx.slug, slug))
}

/// Whether a check is a Forgejo Actions run of `slug` — the only kind `fjo ci` can follow.
///
/// Parsed with the same function `RunRef` uses, so `pr checks` and `ci` cannot disagree about
/// which URLs are runs. A run of a *different* repository (a fork's, or a reusable workflow
/// elsewhere) is not this pull request's CI as `fjo ci <pr>` sees it.
fn is_own_actions_run(check: &CommitStatus, slug: &RepoSlug) -> bool {
    parse_run_url(&check.target_url).is_some_and(|u| same_repo(&u.slug, slug))
}

/// Forgejo's owner and repository names are case-insensitive: `Them/Proj` in a URL is `them/proj`.
fn same_repo(a: &RepoSlug, b: &RepoSlug) -> bool {
    a.owner.eq_ignore_ascii_case(&b.owner) && a.name.eq_ignore_ascii_case(&b.name)
}

/// What the human view prints after the table: the summary line and, when `fjo ci` can help,
/// one `next:` line.
///
/// On a terminal this follows the table on stdout; piped, [`write_report`] sends it to stderr so
/// stdout stays pure TSV. Printed before [`finish`] turns the verdict into an error, so a pending
/// or failed run still shows it. Empty when there are no checks, which keeps that path's output
/// as it was.
///
/// The hint is only for checks posted by this repository's Actions. A failure from external CI
/// has nothing `fjo ci logs` could print, and pointing at it would send someone to an empty page.
fn footer(status: &CombinedStatus, slug: &RepoSlug, pr: i64, scoped: bool) -> String {
    if status.statuses.is_empty() {
        return String::new();
    }
    let (mut passed, mut failed, mut pending, mut skipped) = (0, 0, 0, 0);
    let (mut actions_failed, mut actions_pending) = (false, false);
    for check in &status.statuses {
        let t = tally(check);
        match t {
            Tally::Passed => passed += 1,
            Tally::Failed => failed += 1,
            Tally::Pending => pending += 1,
            Tally::Skipped => skipped += 1,
        }
        let own = is_own_actions_run(check, slug);
        actions_failed |= own && t == Tally::Failed;
        actions_pending |= own && t == Tally::Pending;
    }
    let mut out =
        format!("\n{passed} passed, {failed} failed, {pending} pending, {skipped} skipped\n");
    let verb = if actions_failed {
        "logs"
    } else if actions_pending {
        "watch"
    } else {
        return out;
    };
    let scope = if scoped { format!(" -R {slug}") } else { String::new() };
    out.push_str(&format!("next: fjo ci {verb} {pr}{scope}\n"));
    out
}

/// The check's name, which is `context` — `description` is prose about the run.
fn display_name(check: &CommitStatus) -> String {
    if check.context.trim().is_empty() { check.description.clone() } else { check.context.clone() }
}

/// The state as a word `autocolor` recognises.
fn state_word(check: &CommitStatus) -> String {
    match check.status.as_str() {
        // An empty status is Forgejo's "queued but not started", and printing nothing there makes
        // the row look truncated.
        "" => "pending".to_owned(),
        other => other.to_owned(),
    }
}

fn summary(status: &CombinedStatus) -> String {
    let total = status.statuses.len();
    let failed =
        status.statuses.iter().filter(|c| matches!(c.status.as_str(), "failure" | "error")).count();
    let pending =
        status.statuses.iter().filter(|c| matches!(c.status.as_str(), "pending" | "")).count();
    match verdict(status) {
        Verdict::None => "none configured".to_owned(),
        Verdict::Passed => format!("all {total} passed"),
        Verdict::Pending => format!("{pending} of {total} still running"),
        Verdict::Failed => format!("{failed} of {total} failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use forgejo_model::CommitStatusState;

    fn check(context: &str, state: &str) -> CommitStatus {
        CommitStatus {
            context: context.to_owned(),
            status: CommitStatusState::from(state),
            target_url: format!("https://ci.example.org/{context}"),
            ..CommitStatus::default()
        }
    }

    fn status(checks: Vec<CommitStatus>) -> CombinedStatus {
        CombinedStatus {
            sha: "deadbeef".to_owned(),
            total_count: checks.len() as i64,
            statuses: checks,
            ..CombinedStatus::default()
        }
    }

    /// Bug this prevents — the reported one. `fjo pr status --json state` fetched the combined
    /// commit status and then threw it away: `emit_or` prints the pull request alone on the
    /// machine path, and the status only ever reaches the human table. `pr status --json` is
    /// exactly the shape a script polls in a loop, so that was a wasted request once a second.
    #[tokio::test]
    async fn the_machine_path_does_not_fetch_checks_it_will_not_print() {
        let head = forgejo_model::PrBranchInfo {
            sha: "deadbeef".to_owned(),
            ..forgejo_model::PrBranchInfo::default()
        };
        let found = common::Found {
            slug: RepoSlug::new("them", "proj"),
            pr: forgejo_model::PullRequest {
                number: forgejo_core::types::ids::IssueIndex::new(42),
                head: Some(head),
                ..forgejo_model::PullRequest::default()
            },
        };
        let fake = std::sync::Arc::new(testing::on(
            testing::transport(),
            "GET",
            "/api/v1/repos/them/proj/commits/deadbeef/status",
            forgejo_core::http::transport::Canned::json(200, r#"{"sha":"deadbeef","statuses":[]}"#),
        ));
        let api = testing::api_at(testing::EXAMPLE, fake.clone());

        assert!(status_for(&api, &found, false).await.is_none());
        assert_eq!(fake.call_count(), 0, "--json must not pay for checks: {:?}", fake.calls());

        // ...and the human table still gets them, in one request.
        assert!(status_for(&api, &found, true).await.is_some());
        assert_eq!(fake.call_count(), 1);
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/them/proj/commits/deadbeef/status");
    }

    /// **The exit-code test.** `gh pr checks` exits 8 while checks are running, and every wrapper
    /// script written against `gh` branches on it. This asserts the whole path end to end —
    /// verdict, error variant, and the code `crate::exit` would produce from it — because the
    /// value scripts depend on is the *process status*, not the enum.
    #[test]
    fn pending_checks_map_to_exit_eight() {
        let pending = status(vec![check("build", "pending"), check("test", "success")]);
        assert_eq!(verdict(&pending), Verdict::Pending);

        let err = finish(Verdict::Pending, &RepoSlug::new("them", "proj"), "42", &pending)
            .expect_err("pending checks are not a success");
        assert_eq!(err.exit_code(), 8, "gh's code for `still running`");
        assert_eq!(EXIT_PENDING, 8, "and the constant this module documents agrees");

        let ErrorKind::ChecksPending { slug, pr, pending } = err.kind() else {
            panic!("pending checks must not be reported as a rate limit: {:?}", err.kind())
        };
        assert_eq!(slug.as_deref(), Some("them/proj"));
        assert_eq!(pr, "42");
        // Only the ones actually still running: naming a check that already passed would send
        // someone looking at the wrong job.
        assert_eq!(pending, &["build"]);

        // A failure outranks a pending sibling: waiting for a run that already failed is a loop
        // that will never go green.
        let mixed = status(vec![check("build", "pending"), check("test", "failure")]);
        assert_eq!(verdict(&mixed), Verdict::Failed);
    }

    /// The bug this exists to prevent, now that a variant exists: a failed check leaving through
    /// `std::process::exit`, where the exit code is a second source of truth for
    /// `ErrorKind::exit_code`'s table and no unit test can see it at all. The names and URLs the
    /// error carries are the ones the table printed, so the two cannot disagree.
    #[test]
    fn a_green_run_is_success_and_a_red_one_is_exit_one() {
        assert_eq!(verdict(&status(vec![check("build", "success")])), Verdict::Passed);
        let red = status(vec![check("build", "success"), check("test", "error")]);
        assert_eq!(verdict(&red), Verdict::Failed);

        let err = finish(Verdict::Failed, &RepoSlug::new("them", "proj"), "42", &red).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_FAILED, "gh exits 1 for a failed check");
        let ErrorKind::ChecksFailed { slug, pr, failed } = &*err.kind else {
            panic!("a failed check must be ChecksFailed, not {:?}", err.kind())
        };
        assert_eq!(slug.as_deref(), Some("them/proj"));
        assert_eq!(pr, "42");
        // Only the ones that actually failed: naming a check that passed sends someone to the
        // wrong job, exactly as it would for `ChecksPending`.
        assert_eq!(failed.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["test"]);
    }

    /// Bug this prevents — the one there is an open upstream complaint about. A repository with no CI
    /// answers with an empty `statuses` list and an empty `state`, and reading `state` would produce
    /// either a decode error or a bogus "pending" that never resolves. It is exit 0: nothing failed.
    #[test]
    fn a_pull_request_with_no_ci_is_not_a_failure_and_not_pending() {
        let none = status(Vec::new());
        assert_eq!(verdict(&none), Verdict::None);
        let slug = RepoSlug::new("them", "proj");
        assert!(finish(Verdict::None, &slug, "42", &none).is_ok(), "no checks must exit 0");
        assert!(
            finish(Verdict::Passed, &slug, "42", &status(vec![check("build", "success")])).is_ok(),
            "and neither is a green run"
        );
        assert_eq!(render(&none, &Term::tty(80)), "", "and print no empty table");
        assert_eq!(summary(&none), "none configured");
    }

    /// A queued check arrives with an *empty* state rather than `pending`. Rendering that as a blank
    /// cell makes the row look truncated, and counting it as passed would be wrong.
    #[test]
    fn an_empty_state_is_treated_and_shown_as_pending() {
        let queued = status(vec![check("build", "")]);
        assert_eq!(verdict(&queued), Verdict::Pending);
        assert_eq!(state_word(&check("build", "")), "pending");
    }

    #[test]
    fn check_table_snapshots_for_a_terminal_and_a_pipe() {
        let s = status(vec![
            check("build", "success"),
            check("test", "failure"),
            check("lint", "pending"),
        ]);
        let mut report = String::from("== tty\n");
        report.push_str(&render(&s, &Term::tty(90)));
        report.push_str("== piped\n");
        report.push_str(&render(&s, &Term::piped()));
        insta::assert_snapshot!(report);
    }

    #[test]
    fn the_summary_line_counts_what_matters() {
        assert_eq!(
            summary(&status(vec![check("a", "success"), check("b", "success")])),
            "all 2 passed"
        );
        assert_eq!(
            summary(&status(vec![check("a", "success"), check("b", "pending")])),
            "1 of 2 still running"
        );
        assert_eq!(
            summary(&status(vec![check("a", "failure"), check("b", "success")])),
            "1 of 2 failed"
        );
    }

    /// The name column is `context`, not `description`: `description` is prose about the run
    /// ("Build finished in 4m") and makes a useless column header value.
    #[test]
    fn the_name_column_is_the_check_context() {
        let mut c = check("build / linux", "success");
        c.description = "finished in 4m".to_owned();
        assert_eq!(display_name(&c), "build / linux");
        // ...unless there is no context at all, in which case anything beats an empty cell.
        c.context = String::new();
        assert_eq!(display_name(&c), "finished in 4m");
    }

    /// A check posted by this repository's own Actions for a job of run number `run` — with the
    /// path-only `target_url` Forgejo 16.0.5 really posts
    /// (`/owner/name/actions/runs/<run number>/jobs/<job position>`, measured). The fixture used
    /// to carry a full URL, which no Actions status has, and so hid that the parser required a
    /// scheme and recognised no real check.
    fn actions(context: &str, state: &str, run: i64) -> CommitStatus {
        CommitStatus {
            target_url: format!("/them/proj/actions/runs/{run}/jobs/0"),
            ..check(context, state)
        }
    }

    fn found() -> common::Found {
        common::Found {
            slug: RepoSlug::new("them", "proj"),
            pr: forgejo_model::PullRequest {
                number: forgejo_core::types::ids::IssueIndex::new(42),
                ..forgejo_model::PullRequest::default()
            },
        }
    }

    /// The human view exactly as `pr checks` writes it to a terminal (no colour): stdout, and
    /// stderr, which must stay empty there.
    fn human(s: &CombinedStatus, scoped: bool) -> String {
        let (out, err) = streams(s, &Term::tty(120), scoped);
        assert_eq!(err, "", "on a terminal the summary and hint belong on stdout");
        out
    }

    /// `(stdout, stderr)` of the human path under `term`.
    fn streams(s: &CombinedStatus, term: &Term, scoped: bool) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        write_report(None, s, term, &found(), scoped, &mut out, &mut err).unwrap();
        (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    /// Bug this prevents: `fjo pr checks | cut -f2` choking on a summary line that is not TSV.
    /// Piped, stdout is exactly the table it was before the summary existed, and the summary and
    /// hint move to stderr, where an agent capturing both streams still finds them.
    #[test]
    fn piped_stdout_stays_pure_tsv_and_the_footer_goes_to_stderr() {
        let s = status(vec![
            actions("ci / build", "success", 6441),
            actions("ci / test", "failure", 6442),
            check("lint", "pending"),
        ]);
        let (out, err) = streams(&s, &Term::piped(), false);
        assert_eq!(
            out,
            render(&s, &Term::piped()),
            "stdout must be the pre-change TSV, byte for byte"
        );
        assert!(
            out.lines().all(|l| l.split('\t').count() == 4),
            "every stdout line is a row: {out:?}"
        );
        insta::assert_snapshot!(format!("== stdout\n{out}== stderr\n{err}"));
    }

    /// Piped with nothing to hint at: the summary alone, still on stderr.
    #[test]
    fn piped_all_green_puts_only_the_summary_on_stderr() {
        let s = status(vec![check("lint", "success")]);
        let (out, err) = streams(&s, &Term::piped(), false);
        assert_eq!(out, render(&s, &Term::piped()));
        assert_eq!(err, "1 passed, 0 failed, 0 pending, 0 skipped\n");
    }

    /// A failed Actions job is something `fjo ci logs` can print, so the output says so — on
    /// stdout, after the table, where an agent reading the result will see it.
    #[test]
    fn a_failed_actions_check_points_at_ci_logs() {
        let s = status(vec![
            actions("ci / build", "success", 6441),
            actions("ci / test", "failure", 6442),
            check("external", "pending"),
            check("docs", "skipped"),
        ]);
        insta::assert_snapshot!(human(&s, false));
    }

    #[test]
    fn a_pending_actions_check_points_at_ci_watch() {
        let s = status(vec![
            actions("ci / build", "success", 6441),
            actions("ci / test", "pending", 6442),
            actions("ci / queued", "", 6443),
        ]);
        insta::assert_snapshot!(human(&s, false));
    }

    /// Bug this prevents: `next: fjo ci logs 42` for a failure posted by external CI, which has
    /// no Actions run — `fjo ci logs` would find nothing failed and the hint would be a dead end.
    /// The same goes for a run URL of some *other* repository.
    #[test]
    fn an_external_failure_gets_a_summary_but_no_hint() {
        let mut elsewhere = actions("fork / test", "failure", 7);
        elsewhere.target_url = "https://forgejo.example.org/someone/fork/actions/runs/7".to_owned();
        let s = status(vec![check("jenkins", "failure"), elsewhere, check("lint", "success")]);
        let out = human(&s, false);
        assert!(!out.contains("next:"), "external CI has nothing for fjo ci to show: {out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn an_all_green_run_gets_a_summary_and_no_hint() {
        let s = status(vec![
            actions("ci / build", "success", 6441),
            check("lint", "success"),
            check("style", "warning"),
        ]);
        let out = human(&s, false);
        assert!(!out.contains("next:"));
        insta::assert_snapshot!(out);
    }

    /// A failure outranks a pending sibling, as it does in [`verdict`]: watching a run whose
    /// sibling already failed is a wait that ends red.
    #[test]
    fn logs_outranks_watch_and_the_hint_carries_the_repo_when_it_must() {
        let s = status(vec![actions("a", "pending", 1), actions("b", "error", 2)]);
        let out = human(&s, true);
        assert!(out.ends_with("next: fjo ci logs 42 -R them/proj\n"), "{out}");
    }

    /// `-R` survives into the hint exactly when the hint would otherwise land somewhere else: the
    /// repository came from `-R`, there was none to infer, or the pull request was named by a URL
    /// for a different repository than the checkout's.
    #[test]
    fn the_hint_names_the_repo_only_when_it_was_not_inferred() {
        let here = RepoSlug::new("them", "proj");
        let ctx = |source| RepoContext {
            host: forgejo_core::config::HostKey::parse("forgejo.example.org").unwrap(),
            login: None,
            slug: here.clone(),
            source,
        };
        let remote = ctx(RepoSource::Remote { remote: "origin".to_owned(), score: 3 });
        assert!(!needs_repo_flag(Some(&remote), &here));
        assert!(!needs_repo_flag(Some(&remote), &RepoSlug::new("Them", "Proj")));
        assert!(!needs_repo_flag(Some(&ctx(RepoSource::Env { var: "FJO_REPO" })), &here));
        assert!(needs_repo_flag(Some(&ctx(RepoSource::Flag)), &here));
        assert!(needs_repo_flag(Some(&remote), &RepoSlug::new("someone", "else")));
        assert!(needs_repo_flag(None, &here));
    }

    /// `--json` is a contract with scripts: the summary line and the hint are human output, and
    /// the machine bytes must be exactly the document `support::machine::emit` always wrote.
    #[test]
    fn json_output_carries_no_summary_and_no_hint() {
        let s = status(vec![actions("ci / test", "failure", 6442), check("lint", "pending")]);
        let globals = GlobalOpts::default();
        let triad = support::machine::Triad::compile(
            &globals,
            Some(vec!["state".to_owned(), "statuses".to_owned()]),
        )
        .unwrap();

        let mut via_report = Vec::new();
        let mut err = Vec::new();
        write_report(Some(&triad), &s, &Term::piped(), &found(), true, &mut via_report, &mut err)
            .unwrap();
        assert!(err.is_empty(), "--json writes no summary or hint to stderr either");
        let mut before = Vec::new();
        triad.render(support::to_value(&s).unwrap(), &Term::piped(), &mut before).unwrap();
        assert_eq!(via_report, before, "--json must be byte-identical to the old path");

        let text = String::from_utf8(via_report).unwrap();
        assert!(!text.contains("passed,") && !text.contains("next:"), "{text}");
    }

    /// No checks keeps printing nothing on stdout: the "no checks" note is stderr's job.
    #[test]
    fn no_checks_prints_no_summary() {
        assert_eq!(human(&status(Vec::new()), true), "");
        assert_eq!(
            streams(&status(Vec::new()), &Term::piped(), true),
            (String::new(), String::new())
        );
    }
}

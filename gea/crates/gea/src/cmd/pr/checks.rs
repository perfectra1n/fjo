//! `gea pr checks` and `gea pr status`.
//!
//! # Exit 8 when checks are pending
//!
//! `gh pr checks` exits **0** when everything passed, **1** when something failed, and **8** when
//! checks are still running. That third code is the whole reason the command is scriptable:
//!
//! ```sh
//! gea pr checks; case $? in 0) merge;; 8) sleep 60;; *) investigate;; esac
//! ```
//!
//! `gea` matches it, through the taxonomy rather than around it:
//! [`gitea_core::ErrorKind::ChecksPending`] exits 8 and renders "nothing has failed — the checks
//! simply have not finished", with the still-running check names as facts. Exit codes come from
//! [`gitea_core::ErrorKind::exit_code`] and from nowhere else, so the one table cannot drift
//! from `docs/output.md`.
//!
//! [`Verdict::Failed`] goes through the taxonomy too, as
//! [`gitea_core::ErrorKind::ChecksFailed`], which exits **1**. It used to be the last
//! `std::process::exit` in the tree, for want of a variant: `ChecksPending` is deliberately *not*
//! a flavour of "checks failed", and every other exit-1 variant would print advice about
//! something that did not happen — `Conflict` and `StateConflict` announce a server refusal that
//! never came, and `RunFailed` points at `gea run view <run>`, which is wrong for any check
//! posted through the commit-status API by external CI. `ChecksFailed` adds no prose the table
//! did not already print; its whole job is that the exit code comes from
//! [`gitea_core::ErrorKind::exit_code`] like every other status, where a test can observe it
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
use gitea_client::Api;
use gitea_core::error::FailedCheck;
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{CombinedStatus, CommitStatus};

use super::common;
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

  gea pr checks
  gea pr checks 42
  gea pr checks --json state,statuses
  gea pr checks; case $? in 0) echo green;; 8) echo waiting;; *) echo red;; esac")]
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

  gea pr status
  gea pr status 42")]
pub struct StatusArgs {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_COMBINED_STATUS)?;
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
            support::machine::emit(&rt, globals, m, support::to_value(&status)?)?;
        } else {
            let mut out = std::io::stdout().lock();
            out.write_all(render(&status, rt.term()).as_bytes())?;
            out.flush()?;
            if status.statuses.is_empty() {
                support::empty_note(rt.term(), "checks");
            }
        }
        finish(verdict(&status), &found.slug, &found.pr.number.to_string(), &status)
    })
}

pub fn run_status(globals: &GlobalOpts, args: &StatusArgs) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REQUEST)?;
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
/// * Bug this prevents: `gea pr status --json state` paying for a status request that only the
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

/// The commit the checks are attached to.
fn head_sha(pr: &gitea_model::PullRequest) -> Option<String> {
    pr.head.as_ref().map(|h| h.sha.clone()).filter(|s| !s.is_empty())
}

/// Fetch the combined status, treating "this repository has no CI" as an empty result.
///
/// A 404 on the status endpoint means the *commit* is not known to the instance — which happens for
/// an AGit pull request whose commits have been garbage-collected, and for a fork whose objects were
/// never mirrored. Neither is a check failure, so both come back as an empty status rather than as an
/// error the user cannot act on.
async fn combined(api: &Api, slug: &RepoSlug, sha: &str) -> Result<CombinedStatus> {
    let query = gitea_client::query::RepoGetCombinedStatusByRefQuery::default();
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
/// [`gitea_core::ErrorKind::exit_code`] like every other status in the tool, and carries the
/// names of the checks still running so the message can say what is being waited on.
///
/// Failed is an `Err(ChecksFailed)`, which exits 1 the same way, and carries the checks that
/// failed with their own `target_url`s — the only remedy a status check can offer, since anything
/// holding a token can post one and there is no run id to point at.
///
/// Neither arm prints anything itself. The table rendered directly above already names every
/// check; the error's job is that the exit code comes from the one table in
/// [`gitea_core::ErrorKind::exit_code`], where a unit test can read it without spawning a
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

/// The check's name, which is `context` — `description` is prose about the run.
fn display_name(check: &CommitStatus) -> String {
    if check.context.trim().is_empty() { check.description.clone() } else { check.context.clone() }
}

/// The state as a word `autocolor` recognises.
fn state_word(check: &CommitStatus) -> String {
    match check.status.as_str() {
        // An empty status is Gitea's "queued but not started", and printing nothing there makes
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
    use gitea_model::CommitStatusState;

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

    /// Bug this prevents — the reported one. `gea pr status --json state` fetched the combined
    /// commit status and then threw it away: `emit_or` prints the pull request alone on the
    /// machine path, and the status only ever reaches the human table. `pr status --json` is
    /// exactly the shape a script polls in a loop, so that was a wasted request once a second.
    #[tokio::test]
    async fn the_machine_path_does_not_fetch_checks_it_will_not_print() {
        let head = gitea_model::PrBranchInfo {
            sha: "deadbeef".to_owned(),
            ..gitea_model::PrBranchInfo::default()
        };
        let found = common::Found {
            slug: RepoSlug::new("them", "proj"),
            pr: gitea_model::PullRequest {
                number: gitea_core::types::ids::IssueIndex::new(42),
                head: Some(head),
                ..gitea_model::PullRequest::default()
            },
        };
        let fake = std::sync::Arc::new(testing::on(
            testing::transport(),
            "GET",
            "/api/v1/repos/them/proj/commits/deadbeef/status",
            gitea_core::http::transport::Canned::json(200, r#"{"sha":"deadbeef","statuses":[]}"#),
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
}

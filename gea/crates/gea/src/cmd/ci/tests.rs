//! `gea ci`'s tests: argument parsing, the requests each subcommand makes, the human and
//! `--json` views, the exit-code mapping, and `watch`'s no-spin guarantee.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use gitea_core::error::ErrorKind;
use gitea_core::http::FakeTransport;
use gitea_core::http::transport::Canned;
use gitea_core::types::ids::{JobId, RunId};
use gitea_model::ActionWorkflowJob;
use serde_json::json;

use super::view::{self, RunView};
use super::watch::{Poll, appeared, changes, follow, states};
use super::*;
use crate::cmd::support::testing::{self, on, on_fn};

const SHA: &str = "f36d1a7c0ffee0ddba11f00dfeedface0b5e55ed";
const RUNS: &str = "/api/v1/repos/o/r/actions/runs";

fn slug() -> RepoSlug {
    RepoSlug::new("o", "r")
}

fn resolved(selector: Option<&str>) -> Resolved {
    Resolved {
        slug: slug(),
        sha: SHA.into(),
        label: "PR #2626 (renovate/zip-9.x)".into(),
        selector: selector.map(str::to_owned),
    }
}

/// A run as the API sends one. Built from JSON so the timestamps go through the real
/// deserializer.
fn run(id: i64, file: &str, status: &str, conclusion: &str) -> ActionWorkflowRun {
    run_with(id, file, "pull_request", status, conclusion, json!({}))
}

fn run_with(
    id: i64,
    file: &str,
    event: &str,
    status: &str,
    conclusion: &str,
    extra: serde_json::Value,
) -> ActionWorkflowRun {
    let mut v = json!({
        "id": id,
        "run_number": if id > 3000 { id - 2819 } else { id },
        "path": format!("{file}@refs/pull/2626/merge"),
        "event": event,
        "status": status,
        "conclusion": conclusion,
        "head_sha": SHA,
        "html_url": format!("https://git.example.org/o/r/actions/runs/{id}"),
    });
    if let (Some(base), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        base.extend(more.clone());
    }
    serde_json::from_value(v).expect("a well-formed run")
}

fn job(id: i64, name: &str, status: &str, conclusion: &str) -> ActionWorkflowJob {
    ActionWorkflowJob {
        id: JobId::new(id),
        name: name.into(),
        status: status.into(),
        conclusion: conclusion.into(),
        ..Default::default()
    }
}

fn runs_body(runs: &[ActionWorkflowRun]) -> String {
    json!({"total_count": runs.len(), "workflow_runs": runs}).to_string()
}

fn jobs_body(jobs: &[ActionWorkflowJob]) -> String {
    json!({"total_count": jobs.len(), "jobs": jobs}).to_string()
}

fn jobs_path(run: i64) -> String {
    format!("{RUNS}/{run}/jobs")
}

// ------------------------------------------------------------------------------- arguments

#[derive(Parser)]
struct Harness {
    #[command(subcommand)]
    top: Top,
}

#[derive(clap::Subcommand)]
enum Top {
    Ci(Args),
}

fn parse(argv: &[&str]) -> std::result::Result<Args, clap::Error> {
    let mut full = vec!["gea", "ci"];
    full.extend(argv);
    Harness::try_parse_from(full).map(|h| match h.top {
        Top::Ci(a) => a,
    })
}

#[test]
fn bare_ci_is_status_and_takes_its_flags() {
    let a = parse(&["2626", "--all", "-W", "ci.yml"]).unwrap();
    assert!(a.command.is_none());
    assert_eq!(a.status.target.as_deref(), Some("2626"));
    assert!(a.status.all);
    assert_eq!(a.status.workflow.as_deref(), Some("ci.yml"));

    let a = parse(&[]).unwrap();
    assert!(a.command.is_none() && a.status.target.is_none());
}

/// A subcommand name in first position is the subcommand; `status` makes a branch of that name
/// reachable, which the long help documents.
#[test]
fn a_subcommand_wins_and_status_reaches_a_branch_named_like_one() {
    let a = parse(&["logs", "2626", "--job", "Fast Checks"]).unwrap();
    let Some(Cmd::Logs(l)) = a.command else { panic!("logs") };
    assert_eq!(l.target.as_deref(), Some("2626"));
    assert_eq!(l.job, Some(JobRef::Name("Fast Checks".into())));

    let a = parse(&["status", "logs"]).unwrap();
    let Some(Cmd::Status(s)) = a.command else { panic!("status") };
    assert_eq!(s.target.as_deref(), Some("logs"));

    // Once a bare-status flag is given, a later word is the target, not a subcommand
    // (`args_conflicts_with_subcommands`): `--all` never silently reaches `rerun`.
    let a = parse(&["--all", "rerun"]).unwrap();
    assert!(a.command.is_none());
    assert_eq!(a.status.target.as_deref(), Some("rerun"));
}

#[test]
fn logs_all_and_job_are_exclusive() {
    assert!(parse(&["logs", "--all", "--job", "build"]).is_err());
    assert!(parse(&["logs", "--all"]).is_ok());
}

/// Bug this prevents: a zero `--interval`, which would poll in a tight loop.
#[test]
fn the_watch_interval_cannot_be_zero_and_defaults_are_as_documented() {
    assert!(parse(&["watch", "--interval", "0"]).is_err());
    let Some(Cmd::Watch(w)) = parse(&["watch"]).unwrap().command else { panic!("watch") };
    assert_eq!((w.interval, w.appear_timeout), (5, 60));
    let Some(Cmd::Watch(w)) =
        parse(&["watch", "-i", "2", "--appear-timeout", "0"]).unwrap().command
    else {
        panic!("watch")
    };
    assert_eq!((w.interval, w.appear_timeout), (2, 0));
}

#[test]
fn every_subcommand_describes_itself() {
    let cmd = Harness::command();
    let ci = cmd.find_subcommand("ci").unwrap();
    for sub in ["status", "logs", "watch", "rerun"] {
        let s = ci.find_subcommand(sub).unwrap_or_else(|| panic!("{sub}"));
        assert!(s.get_about().is_some(), "{sub}");
        assert!(s.get_long_about().is_some(), "{sub}");
    }
}

/// Bare `--json` lists `status`'s fields without a runtime; the text-producing subcommands have
/// none, and say so before any request.
#[test]
fn json_fields_are_the_run_table_for_status_only() {
    assert!(matches!(fields_for(None), Fields::Op("GetWorkflowRun")));
    let status = Cmd::Status(StatusArgs::default());
    assert!(matches!(fields_for(Some(&status)), Fields::Op("GetWorkflowRun")));
    for cmd in [Cmd::Logs(LogsArgs::default()), Cmd::Rerun(RerunArgs::default())] {
        assert!(matches!(fields_for(Some(&cmd)), Fields::None));
        let err = emit::discover(&testing::json_globals(""), fields_for(Some(&cmd))).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}

// ------------------------------------------------------------------------------- selection

#[test]
fn selection_hides_superseded_runs_unless_all_and_filters_by_workflow_file() {
    let runs = vec![
        run(10, "ci.yml", "completed", "failure"),
        run(12, "ci.yml", "completed", "success"),
        run(11, "lint.yml", "in_progress", ""),
    ];
    let ids = |v: Vec<&ActionWorkflowRun>| v.iter().map(|r| r.id.get()).collect::<Vec<_>>();
    assert_eq!(ids(select(&runs, false, None)), [11, 12]);
    assert_eq!(ids(select(&runs, true, None)), [10, 11, 12]);
    assert_eq!(ids(select(&runs, false, Some("ci.yml"))), [12]);
    assert_eq!(ids(select(&runs, false, Some(".gitea/workflows/ci.yml"))), [12]);
    assert!(select(&runs, false, Some("nope.yml")).is_empty());
}

#[test]
fn hints_name_the_target_and_carry_an_explicit_repo() {
    let g = GlobalOpts::default();
    assert_eq!(hint("logs", &resolved(Some("2626")), &g), "gea ci logs 2626");
    assert_eq!(hint("watch", &resolved(None), &g), "gea ci watch");
    let g = GlobalOpts { repo: Some("o/r".parse().unwrap()), ..GlobalOpts::default() };
    assert_eq!(hint("logs", &resolved(Some("main")), &g), "gea ci logs main -R o/r");
    let url = "https://git.example.org/o/r/pulls/7";
    assert_eq!(hint("logs", &resolved(Some(url)), &g), format!("gea ci logs {url}"));
}

/// Bug this prevents: `ci.yml@refs/heads/main` and `ci.yml@refs/tags/v1`, both `push`, drawn
/// as two identical `ci.yml` lines. `latest_per_workflow` keeps both, so the names must differ.
#[test]
fn same_file_same_event_on_two_refs_is_told_apart_by_the_ref() {
    let at = |id: i64, file_ref: &str, event: &str| {
        run_with(id, "x", event, "completed", "success", json!({ "path": file_ref }))
    };
    let main = at(1, "ci.yml@refs/heads/main", "push");
    let tag = at(2, "ci.yml@refs/tags/v1", "push");
    let pr = at(3, "ci.yml@refs/pull/7/merge", "pull_request");
    let lint = at(4, "lint.yml@refs/heads/main", "push");
    assert_eq!(
        view::names(&[&main, &tag, &pr, &lint]),
        ["ci.yml (push, main)", "ci.yml (push, v1)", "ci.yml (pull_request)", "lint.yml"]
    );
    // One event per file needs no qualifier, and the event alone when that tells them apart.
    assert_eq!(view::names(&[&main, &pr]), ["ci.yml (push)", "ci.yml (pull_request)"]);
    assert_eq!(view::names(&[&main]), ["ci.yml"]);
}

#[test]
fn the_empty_note_says_why_and_names_the_workflows_a_filter_missed() {
    let note = no_runs_message(&resolved(Some("2626")), &[], None);
    assert!(
        note.starts_with("no Actions runs for f36d1a7 (PR #2626 (renovate/zip-9.x))"),
        "{note}"
    );
    assert!(note.contains("`on:` filters"), "{note}");

    let runs = vec![run(1, "ci.yml", "completed", "success"), run(2, "lint.yml", "queued", "")];
    let note = no_runs_message(&resolved(None), &runs, Some("nope.yml"));
    assert!(note.contains("no run of nope.yml") && note.ends_with("ci.yml, lint.yml"), "{note}");
}

// ---------------------------------------------------------------------------------- gather

/// Jobs are fetched for the failed and the pending run only — a run that passed or was skipped
/// costs nothing — and a waiting job is explained.
#[tokio::test]
async fn jobs_are_fetched_only_for_failed_and_pending_runs() {
    let runs = vec![
        run(6442, "ci.yml", "completed", "failure"),
        run(6443, "review.yml", "queued", ""),
        run(6444, "pr-title.yml", "completed", "success"),
        run(6445, "docs.yml", "completed", "skipped"),
    ];
    let fake = on(
        on(
            testing::transport(),
            "GET",
            &jobs_path(6442),
            Canned::json(
                200,
                jobs_body(&[
                    job(1, "Fast Checks", "completed", "failure"),
                    job(2, "Docs", "completed", "success"),
                ]),
            ),
        ),
        "GET",
        &jobs_path(6443),
        Canned::json(
            200,
            jobs_body(&[{
                let mut j = job(3, "review", "queued", "");
                j.labels = vec!["duck-runner-huge".into()];
                j
            }]),
        ),
    );
    let fake = Arc::new(on(
        fake,
        "GET",
        "/api/v1/repos/o/r/actions/runners",
        Canned::json(200, r#"{"runners":[]}"#),
    ));
    let selected = select(&runs, false, None);
    let views = view::gather(&testing::api(fake.clone()), &slug(), &selected, true).await.unwrap();

    let jobs_calls: Vec<String> =
        fake.calls().into_iter().map(|c| c.path).filter(|p| p.ends_with("/jobs")).collect();
    assert_eq!(jobs_calls, [jobs_path(6442), jobs_path(6443)]);
    assert_eq!(views[0].failed_jobs.iter().map(|j| j.id.get()).collect::<Vec<_>>(), [1]);
    assert!(views[1].waiting[0].contains("duck-runner-huge"), "{:?}", views[1].waiting);
    assert!(views[2].failed_jobs.is_empty() && views[2].waiting.is_empty());
}

/// `--json` prints no diagnosis, so it must not pay for one: only the failed run's jobs, which
/// the error names, are fetched.
#[tokio::test]
async fn without_a_diagnosis_a_pending_run_costs_nothing() {
    let runs = vec![run(6442, "ci.yml", "completed", "failure"), run(6443, "x.yml", "queued", "")];
    let fake = Arc::new(on(
        testing::transport(),
        "GET",
        &jobs_path(6442),
        Canned::json(200, jobs_body(&[job(1, "build", "completed", "failure")])),
    ));
    let selected = select(&runs, false, None);
    view::gather(&testing::api(fake.clone()), &slug(), &selected, false).await.unwrap();
    assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
}

// ------------------------------------------------------------------------------- the views

fn timed(id: i64, file: &str, conclusion: &str, seconds: u32) -> ActionWorkflowRun {
    run_with(
        id,
        file,
        "pull_request",
        "completed",
        conclusion,
        json!({
            "started_at": "2026-10-09T10:00:00Z",
            "completed_at": format!("2026-10-09T10:{:02}:{:02}Z", seconds / 60, seconds % 60),
        }),
    )
}

fn mixed_runs() -> Vec<ActionWorkflowRun> {
    vec![
        timed(6442, "ci.yml", "failure", 404),
        run(6443, "claude-code-review.yml", "queued", ""),
        timed(6444, "pr-title.yml", "success", 12),
        run(6445, "docs.yml", "completed", "skipped"),
        run_with(6446, "build.yml", "push", "in_progress", "", json!({})),
        run_with(6447, "build.yml", "pull_request", "completed", "cancelled", json!({})),
    ]
}

fn mixed_views(runs: &[ActionWorkflowRun]) -> Vec<RunView<'_>> {
    let mut views: Vec<RunView<'_>> =
        select(runs, false, None).into_iter().map(RunView::bare).collect();
    for v in &mut views {
        match v.run.id.get() {
            6442 => {
                v.failed_jobs = vec![
                    job(33898, "Fast Checks (fmt + static lints)", "completed", "failure"),
                    job(33900, "Build - workspace", "completed", "failure"),
                ]
            }
            6443 => {
                v.waiting = vec![
                    "job review requires runs-on duck-runner-huge, but no runner matches all \
                     labels. The job will wait indefinitely. no runner is visible to this \
                     repository"
                        .into(),
                ]
            }
            _ => {}
        }
    }
    views
}

/// The view the design draws: failed first with its failed jobs, then pending with its waiting
/// diagnosis, then passed (skipped among them), and the next command last. A workflow with runs
/// for two events is told apart by the event.
#[test]
fn status_human_output_golden() {
    let runs = mixed_runs();
    let views = mixed_views(&runs);
    let next =
        view::next_verb(&views).map(|v| hint(v, &resolved(Some("2626")), &GlobalOpts::default()));
    assert_eq!(next.as_deref(), Some("gea ci logs 2626"));
    insta::assert_snapshot!(view::render(
        &resolved(Some("2626")),
        &views,
        &testing::term(),
        next.as_deref()
    ));
}

#[test]
fn status_human_output_when_all_is_well_has_no_next_line() {
    let runs = vec![timed(1, "ci.yml", "success", 61), run(2, "docs.yml", "completed", "skipped")];
    let views: Vec<RunView<'_>> =
        select(&runs, false, None).into_iter().map(RunView::bare).collect();
    assert_eq!(view::next_verb(&views), None);
    insta::assert_snapshot!(view::render(&resolved(None), &views, &testing::term(), None));
}

#[test]
fn status_human_output_pending_suggests_watch() {
    let runs = vec![timed(1, "ci.yml", "success", 61), run(2, "lint.yml", "in_progress", "")];
    let views: Vec<RunView<'_>> =
        select(&runs, false, None).into_iter().map(RunView::bare).collect();
    let next =
        view::next_verb(&views).map(|v| hint(v, &resolved(Some("main")), &GlobalOpts::default()));
    insta::assert_snapshot!(view::render(
        &resolved(Some("main")),
        &views,
        &testing::term(),
        next.as_deref()
    ));
}

/// No runs: the header alone (the note goes to stderr), and exit 0.
#[test]
fn status_human_output_for_a_commit_with_no_runs() {
    let out = view::render(&resolved(Some("2626")), &[], &testing::term(), None);
    insta::assert_snapshot!(out);
    assert!(view::verdict(&resolved(Some("2626")), &[], None).is_ok());
}

fn json_of(selected: &[&ActionWorkflowRun], fields: &str) -> String {
    let mut buf = Vec::new();
    emit::list_to(
        &mut buf,
        &testing::term(),
        &testing::json_globals(fields),
        listing(selected).unwrap(),
        |_| {},
    )
    .unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn status_json_output_golden() {
    let runs = mixed_runs();
    let selected = select(&runs, false, None);
    insta::assert_snapshot!(json_of(&selected, "id,run_number,status,conclusion,event"));
    assert_eq!(json_of(&[], "id").trim(), "[]");
}

// ----------------------------------------------------------------------------- exit codes

#[test]
fn a_failed_run_is_ci_failed_exit_1_naming_its_failed_jobs() {
    let runs = mixed_runs();
    let views = mixed_views(&runs);
    let err = view::verdict(&resolved(Some("2626")), &views, None).unwrap_err();
    assert_eq!(err.exit_code(), 1);
    let ErrorKind::CiFailed { target, sha, failed, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!((target.as_deref(), sha.as_str()), (Some("2626"), SHA));
    // A failure outranks a pending sibling; cancelled is a failure too. In display order.
    assert_eq!(failed.iter().map(|f| f.id).collect::<Vec<_>>(), [6447, 6442]);
    assert_eq!(failed[1].failed_jobs, ["Fast Checks (fmt + static lints)", "Build - workspace"]);
    assert_eq!((failed[0].workflow.as_str(), failed[1].workflow.as_str()), ("build.yml", "ci.yml"));
}

#[test]
fn an_unfinished_run_is_ci_pending_exit_8() {
    let runs = [run(1, "ci.yml", "completed", "success"), run(2, "lint.yml", "waiting", "")];
    let views: Vec<RunView<'_>> = runs.iter().map(RunView::bare).collect();
    let err = view::verdict(&resolved(None), &views, None).unwrap_err();
    assert_eq!(err.exit_code(), 8);
    let ErrorKind::CiPending { target, pending, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!((target, pending.as_slice()), (&None, &["lint.yml".to_owned()][..]));
}

#[test]
fn skipped_counts_as_success() {
    let runs =
        [run(1, "ci.yml", "completed", "success"), run(2, "docs.yml", "completed", "skipped")];
    let views: Vec<RunView<'_>> = runs.iter().map(RunView::bare).collect();
    assert!(view::verdict(&resolved(None), &views, None).is_ok());
}

/// Bug this prevents: `--all`, a display flag, changing the exit code. A run that failed and
/// was rerun green is shown under `--all`, marked superseded, but the commit is green: exit 0
/// and no `next:` line, exactly as without `--all`.
#[test]
fn all_shows_a_superseded_failure_without_letting_it_into_the_verdict() {
    let runs = vec![timed(10, "ci.yml", "failure", 30), timed(12, "ci.yml", "success", 61)];
    let shown = select(&runs, true, None);
    let mut views: Vec<RunView<'_>> = shown.iter().copied().map(RunView::bare).collect();
    view::mark_superseded(&mut views, &runs, None);
    assert_eq!(
        views.iter().map(|v| (v.run.id.get(), v.superseded)).collect::<Vec<_>>(),
        [(10, true), (12, false)]
    );
    assert!(view::verdict(&resolved(None), &views, None).is_ok());
    assert_eq!(view::next_verb(&views), None);
    let out = view::render(&resolved(None), &views, &testing::term(), None);
    assert!(out.contains("ci.yml (superseded)"), "{out}");
    assert!(!out.contains("next:"), "{out}");
    insta::assert_snapshot!(out);
    // --json under --all lists every run, superseded included; the exit code above is unchanged.
    let json = json_of(&shown, "id,conclusion");
    assert!(json.contains("10") && json.contains("12"), "{json}");
}

/// The other direction: a green run superseded by a red rerun is still red under `--all`.
#[test]
fn all_still_fails_when_the_newest_run_failed() {
    let runs =
        vec![run(10, "ci.yml", "completed", "success"), run(12, "ci.yml", "completed", "failure")];
    let mut views: Vec<RunView<'_>> =
        select(&runs, true, None).into_iter().map(RunView::bare).collect();
    view::mark_superseded(&mut views, &runs, None);
    let err = view::verdict(&resolved(None), &views, None).unwrap_err();
    let ErrorKind::CiFailed { failed, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!(failed.iter().map(|f| f.id).collect::<Vec<_>>(), [12]);
    assert_eq!(view::next_verb(&views), Some("logs"));
}

// ------------------------------------------------------------------------------- -R advice

fn rendered(err: &gitea_core::error::Error) -> String {
    gitea_core::error::render::render(err, gitea_core::error::render::Color::Never)
}

fn red_views(runs: &[ActionWorkflowRun]) -> Vec<RunView<'_>> {
    runs.iter().map(RunView::bare).collect()
}

/// Bug this prevents: the `next:` line and the error under it naming two different commands —
/// `next: gea ci logs` above `gea ci logs -R o/r`, or `-R` pasted after a URL that already
/// names its repository. Both now follow [`repo_flag`].
#[test]
fn error_advice_carries_r_exactly_when_the_next_line_does() {
    let runs = [run(1, "ci.yml", "completed", "failure")];
    let views = red_views(&runs);
    let check = |target: Option<&str>, globals: &GlobalOpts, want: &str| {
        let r = resolved(target);
        let err = view::verdict(&r, &views, advice_slug(&r, globals)).unwrap_err();
        let out = rendered(&err);
        let next = hint("logs", &r, globals);
        assert_eq!(next, want);
        assert!(out.contains(&format!("1. {want}  (")), "advice is not `{want}`:\n{out}");
        let ErrorKind::CiFailed { slug, .. } = err.kind() else { panic!("{err:?}") };
        assert_eq!(slug.is_some(), want.contains(" -R "));
    };
    let here = GlobalOpts::default();
    let flagged = GlobalOpts { repo: Some("o/r".parse().unwrap()), ..GlobalOpts::default() };
    // In a checkout, no -R: the repository comes from the checkout, where the command reruns.
    check(None, &here, "gea ci logs");
    check(Some("2626"), &here, "gea ci logs 2626");
    // With -R, the advice keeps it.
    check(None, &flagged, "gea ci logs -R o/r");
    check(Some("main"), &flagged, "gea ci logs main -R o/r");
    // A URL names its repository; -R after it would be noise at best.
    let url = "https://git.example.org/o/r/pulls/7";
    check(Some(url), &flagged, &format!("gea ci logs {url}"));
    check(Some(url), &here, &format!("gea ci logs {url}"));
}

/// The same rule for pending and for no runs at all.
#[test]
fn pending_and_no_runs_advice_follow_the_same_r_rule() {
    let runs = [run(1, "ci.yml", "in_progress", "")];
    let views = red_views(&runs);
    let flagged = GlobalOpts { repo: Some("o/r".parse().unwrap()), ..GlobalOpts::default() };
    let here = GlobalOpts::default();
    let r = resolved(Some("main"));
    let out = rendered(&view::verdict(&r, &views, advice_slug(&r, &here)).unwrap_err());
    assert!(out.contains("1. gea ci watch main  ("), "{out}");
    let out = rendered(&view::verdict(&r, &views, advice_slug(&r, &flagged)).unwrap_err());
    assert!(out.contains("1. gea ci watch main -R o/r  ("), "{out}");

    let out = rendered(&no_runs_error(&r, &here, None));
    assert!(out.contains("gea ci watch main --appear-timeout"), "{out}");
    let out = rendered(&no_runs_error(&r, &flagged, None));
    assert!(out.contains("gea ci watch main -R o/r --appear-timeout"), "{out}");
}

// ------------------------------------------------------------------------------------ logs

fn two_workflows() -> FakeTransport {
    on(
        on(
            testing::transport(),
            "GET",
            &jobs_path(6442),
            Canned::json(
                200,
                jobs_body(&[
                    job(11, "build", "completed", "failure"),
                    job(12, "lint", "completed", "success"),
                ]),
            ),
        ),
        "GET",
        &jobs_path(6450),
        Canned::json(
            200,
            jobs_body(&[
                job(21, "build", "completed", "success"),
                job(22, "publish", "completed", "success"),
            ]),
        ),
    )
}

fn logs_runs() -> Vec<ActionWorkflowRun> {
    vec![
        run(6442, "ci.yml", "completed", "failure"),
        run(6450, "release.yml", "completed", "success"),
    ]
}

/// By default only failed jobs, and jobs are listed only for the runs that failed.
#[tokio::test]
async fn logs_default_to_the_failed_jobs_of_the_failed_runs() {
    let runs = logs_runs();
    let selected = select(&runs, false, None);
    let fake = Arc::new(two_workflows());
    let picked =
        logs::pick(&testing::api(fake.clone()), &slug(), &selected, false, None).await.unwrap();
    assert_eq!(picked.iter().map(|(_, j)| j.id.get()).collect::<Vec<_>>(), [11]);
    assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
}

#[tokio::test]
async fn logs_all_prints_every_job_of_every_run() {
    let runs = logs_runs();
    let selected = select(&runs, false, None);
    let picked =
        logs::pick(&testing::api(Arc::new(two_workflows())), &slug(), &selected, true, None)
            .await
            .unwrap();
    assert_eq!(picked.iter().map(|(_, j)| j.id.get()).collect::<Vec<_>>(), [11, 12, 21, 22]);
}

/// Bug this prevents: `--job build` silently picking one of two workflows' `build` jobs. It is
/// refused, and every candidate is labelled with its workflow so the next attempt can pass an id.
#[tokio::test]
async fn a_job_name_ambiguous_across_workflows_is_refused_with_labelled_candidates() {
    let runs = logs_runs();
    let selected = select(&runs, false, None);
    let api = testing::api(Arc::new(two_workflows()));
    let err = logs::pick(&api, &slug(), &selected, false, Some(&JobRef::Name("build".into())))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    let msg = err.to_string();
    assert!(
        msg.contains("ci.yml / build (id 11)") && msg.contains("release.yml / build (id 21)"),
        "{msg}"
    );

    // A unique name, or the id, picks across runs — including a run that passed.
    let picked = logs::pick(&api, &slug(), &selected, false, Some(&JobRef::Name("publish".into())))
        .await
        .unwrap();
    assert_eq!((picked[0].0.id.get(), picked[0].1.id.get()), (6450, 22));
    let picked = logs::pick(&api, &slug(), &selected, false, Some(&JobRef::Id(21))).await.unwrap();
    assert_eq!(picked[0].1.name, "build");
}

#[test]
fn every_log_is_headed_with_its_workflow_run_and_job() {
    let runs = logs_runs();
    let picked = vec![
        (&runs[0], job(11, "build", "completed", "failure")),
        (&runs[1], job(21, "build", "completed", "success")),
    ];
    let out = logs::assemble(&picked, &["one".to_owned(), "two\n".to_owned()]);
    assert_eq!(
        out,
        "==> ci.yml / build (run 6442, job 11) <==\none\n==> release.yml / build (run 6450, job 21) <==\ntwo\n"
    );
}

// ----------------------------------------------------------------------------------- watch

fn poll(appear: u64) -> Poll<'static> {
    Poll {
        workflow: None,
        interval: Duration::from_secs(1),
        appear_timeout: Duration::from_secs(appear),
    }
}

/// Bug this prevents: a watch loop that re-polls without sleeping. Every poll that does not
/// return is followed by exactly one sleep of `--interval`, and each change is reported once.
#[tokio::test]
async fn watch_sleeps_between_every_poll_and_reports_each_change() {
    let replies = [
        vec![run(1, "ci.yml", "queued", ""), run(2, "lint.yml", "in_progress", "")],
        vec![run(1, "ci.yml", "in_progress", ""), run(2, "lint.yml", "in_progress", "")],
        vec![run(1, "ci.yml", "completed", "failure"), run(2, "lint.yml", "completed", "success")],
    ]
    .iter()
    .map(|r| Canned::json(200, runs_body(r)))
    .collect();
    let fake = Arc::new(testing::transport().on_sequence(testing::method("GET"), RUNS, replies));
    let slept = Cell::new(Vec::<Duration>::new());
    let mut said = Vec::new();
    let runs = follow(
        &testing::api(fake.clone()),
        &resolved(None),
        &poll(60),
        |d| {
            let mut v = slept.take();
            v.push(d);
            slept.set(v);
            std::future::ready(())
        },
        |line| said.push(line.to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(fake.call_count(), 3);
    assert_eq!(slept.take(), [Duration::from_secs(1); 2]);
    assert_eq!(
        said,
        [
            "ci.yml: queued → in_progress",
            "ci.yml: in_progress → failure",
            "lint.yml: in_progress → success"
        ]
    );
    assert!(fake.calls()[0].query.contains(&format!("head_sha={SHA}")));
}

/// Right after a push there may be no runs yet. The loop keeps polling — sleeping each time —
/// until `--appear-timeout`, then answers the empty list for `status` to report.
#[tokio::test]
async fn watch_waits_for_runs_to_appear_then_gives_up() {
    let fake = Arc::new(on_fn(testing::transport(), "GET", RUNS, |_| {
        Canned::json(200, r#"{"total_count":0,"workflow_runs":[]}"#)
    }));
    let sleeps = Cell::new(0);
    let mut said = Vec::new();
    let runs = follow(
        &testing::api(fake.clone()),
        &resolved(None),
        &poll(2),
        |_| {
            sleeps.set(sleeps.get() + 1);
            std::future::ready(())
        },
        |line| said.push(line.to_owned()),
    )
    .await
    .unwrap();
    assert!(runs.is_empty());
    assert_eq!((fake.call_count(), sleeps.get()), (3, 2));
    assert_eq!(said.len(), 1, "announced once, not every poll: {said:?}");
    assert!(said[0].contains("waiting up to 2s"), "{said:?}");

    // Giving up is not success. Bug this prevents: `gea ci watch && gea pr merge` merging a
    // commit no workflow ever ran on, because an empty selection exited 0.
    let err = appeared(&runs, &resolved(Some("main")), &GlobalOpts::default(), None).unwrap_err();
    assert_eq!(err.exit_code(), 8);
    let ErrorKind::CiNoRuns { slug, target, sha, workflow } = err.kind() else { panic!("{err:?}") };
    assert_eq!(
        (slug, target.as_deref(), sha.as_str(), workflow),
        (&None, Some("main"), SHA, &None)
    );
}

/// A `--workflow` that matches none of the commit's runs gives up the same way, and says which
/// workflow it was waiting for.
#[test]
fn watch_gives_up_on_a_workflow_filter_that_matches_nothing() {
    let runs = vec![run(1, "ci.yml", "completed", "success")];
    let err =
        appeared(&runs, &resolved(None), &GlobalOpts::default(), Some("nope.yml")).unwrap_err();
    assert_eq!(err.exit_code(), 8);
    let ErrorKind::CiNoRuns { workflow, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!(workflow.as_deref(), Some("nope.yml"));
    assert_eq!(appeared(&runs, &resolved(None), &GlobalOpts::default(), None).unwrap().len(), 1);
}

/// Bug this prevents: transitions matched across polls by display name. When a second
/// `ci.yml` (another ref, same event) appears, the first one's name gains a qualifier; matched by
/// name it would read as a brand-new run, and two same-named runs would swap transitions.
#[test]
fn watch_matches_runs_across_polls_by_identity_not_name() {
    let at = |id: i64, file_ref: &str, status: &str, conclusion: &str| {
        run_with(id, "x", "push", status, conclusion, json!({ "path": file_ref }))
    };
    let main_queued = at(1, "ci.yml@refs/heads/main", "queued", "");
    let before = states(&[&main_queued]);
    let main_running = at(1, "ci.yml@refs/heads/main", "in_progress", "");
    let tag_queued = at(2, "ci.yml@refs/tags/v1", "queued", "");
    let now = states(&[&main_running, &tag_queued]);
    assert_eq!(
        changes(&before, &now),
        ["ci.yml (push, main): queued → in_progress", "ci.yml (push, v1): queued"]
    );

    // Both present on both polls, only one moves: the transition lands on the right one.
    let tag_done = at(2, "ci.yml@refs/tags/v1", "completed", "failure");
    let later = states(&[&main_running, &tag_done]);
    assert_eq!(changes(&now, &later), ["ci.yml (push, v1): queued → failure"]);
}

/// A run that appears after a few empty polls is followed like any other.
#[tokio::test]
async fn watch_follows_a_run_that_appears_late() {
    let replies = vec![
        Canned::json(200, runs_body(&[])),
        Canned::json(200, runs_body(&[run(1, "ci.yml", "queued", "")])),
        Canned::json(200, runs_body(&[run(1, "ci.yml", "completed", "success")])),
    ];
    let fake = Arc::new(testing::transport().on_sequence(testing::method("GET"), RUNS, replies));
    let sleeps = Cell::new(0);
    let runs = follow(
        &testing::api(fake.clone()),
        &resolved(None),
        &poll(5),
        |_| {
            sleeps.set(sleeps.get() + 1);
            std::future::ready(())
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(runs[0].conclusion, "success");
    assert_eq!((fake.call_count(), sleeps.get()), (3, 2));
}

/// An appear timeout of 0 gives up on the first empty poll, without sleeping.
#[tokio::test]
async fn watch_with_no_appear_timeout_gives_up_at_once() {
    let fake = Arc::new(on_fn(testing::transport(), "GET", RUNS, |_| {
        Canned::json(200, r#"{"total_count":0,"workflow_runs":[]}"#)
    }));
    let sleeps = Cell::new(0);
    let runs = follow(
        &testing::api(fake.clone()),
        &resolved(None),
        &poll(0),
        |_| {
            sleeps.set(sleeps.get() + 1);
            std::future::ready(())
        },
        |_| {},
    )
    .await
    .unwrap();
    assert!(runs.is_empty());
    assert_eq!((fake.call_count(), sleeps.get()), (1, 0));
}

// ----------------------------------------------------------------------------------- rerun

/// Only the failed run is rerun, and through the failed-jobs route `run rerun --failed` uses.
#[tokio::test]
async fn rerun_hits_the_failed_jobs_route_for_failed_runs_only() {
    let fake = Arc::new(on(
        testing::transport(),
        "POST",
        &format!("{RUNS}/6442/rerun-failed-jobs"),
        testing::empty(),
    ));
    let runs = vec![
        run(6442, "ci.yml", "completed", "failure"),
        run(6443, "lint.yml", "in_progress", ""),
        run(6444, "docs.yml", "completed", "success"),
    ];
    let selected = select(&runs, false, None);
    let mut said = Vec::new();
    let n =
        rerun_failed(&testing::api(fake.clone()), &slug(), &selected, |l| said.push(l.to_owned()))
            .await
            .unwrap();
    assert_eq!(n, 1);
    assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
    assert_eq!(said, ["reran failed jobs of ci.yml (run 6442)"]);
}

#[test]
fn run_ids_are_what_the_rerun_route_takes() {
    // `request_rerun` takes the run's database id, never its number.
    let r = run(6442, "ci.yml", "completed", "failure");
    assert_eq!(r.id, RunId::new(6442));
    assert_ne!(r.run_number, 6442);
}

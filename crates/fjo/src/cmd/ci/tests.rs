//! `fjo ci`'s tests: argument parsing, the requests each subcommand makes, the human and
//! `--json` views, the exit-code mapping, and `watch`'s no-spin guarantee.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use forgejo_core::error::ErrorKind;
use forgejo_core::http::FakeTransport;
use forgejo_core::http::transport::Canned;
use forgejo_core::types::RepoSlug;
use forgejo_core::types::ids::JobId;
use forgejo_model::ActionRunJob;
use serde_json::json;

use super::view::{self, RunView};
use super::watch::{Poll, follow};
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
fn run(id: i64, file: &str, status: &str) -> ActionRun {
    run_with(id, file, "pull_request", status, json!({}))
}

/// The run's number, distinct from its id as on any instance with more than one repository.
fn number(id: i64) -> i64 {
    if id > 3000 { id - 2819 } else { id }
}

fn run_with(id: i64, file: &str, event: &str, status: &str, extra: serde_json::Value) -> ActionRun {
    let mut v = json!({
        "id": id,
        "index_in_repo": number(id),
        "workflow_id": file,
        "event": event,
        "prettyref": "#2626",
        "status": status,
        "commit_sha": SHA,
        // Forgejo links a run by its number, not its id (measured).
        "html_url": format!("https://git.example.org/o/r/actions/runs/{}", number(id)),
    });
    if let (Some(base), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        base.extend(more.clone());
    }
    serde_json::from_value(v).expect("a well-formed run")
}

fn job(id: i64, name: &str, status: &str) -> ActionRunJob {
    ActionRunJob {
        id: JobId::new(id),
        name: name.into(),
        status: status.into(),
        ..Default::default()
    }
}

fn runs_body(runs: &[ActionRun]) -> String {
    json!({"total_count": runs.len(), "workflow_runs": runs}).to_string()
}

/// Forgejo's jobs listing is a bare array, not Gitea's `{total_count, jobs}` envelope.
fn jobs_body(jobs: &[ActionRunJob]) -> String {
    json!(jobs).to_string()
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
    let mut full = vec!["fjo", "ci"];
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
    // (`args_conflicts_with_subcommands`): `--all` never silently reaches `logs`.
    let a = parse(&["--all", "logs"]).unwrap();
    assert!(a.command.is_none());
    assert_eq!(a.status.target.as_deref(), Some("logs"));
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
    for sub in ["status", "logs", "watch"] {
        let s = ci.find_subcommand(sub).unwrap_or_else(|| panic!("{sub}"));
        assert!(s.get_about().is_some(), "{sub}");
        assert!(s.get_long_about().is_some(), "{sub}");
    }
}

/// Bare `--json` lists `status`'s fields without a runtime; the text-producing subcommands have
/// none, and say so before any request.
#[test]
fn json_fields_are_the_run_table_for_status_only() {
    assert!(matches!(fields_for(None), Fields::Op("ActionRun")));
    let status = Cmd::Status(StatusArgs::default());
    assert!(matches!(fields_for(Some(&status)), Fields::Op("ActionRun")));
    {
        let cmd = Cmd::Logs(LogsArgs::default());
        assert!(matches!(fields_for(Some(&cmd)), Fields::None));
        let err = emit::discover(&testing::json_globals(""), fields_for(Some(&cmd))).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}

// ------------------------------------------------------------------------------- selection

#[test]
fn selection_hides_superseded_runs_unless_all_and_filters_by_workflow_file() {
    let runs = vec![
        run(10, "ci.yml", "failure"),
        run(12, "ci.yml", "success"),
        run(11, "lint.yml", "running"),
    ];
    let ids = |v: Vec<&ActionRun>| v.iter().map(|r| r.id.get()).collect::<Vec<_>>();
    assert_eq!(ids(select(&runs, false, None)), [11, 12]);
    assert_eq!(ids(select(&runs, true, None)), [10, 11, 12]);
    assert_eq!(ids(select(&runs, false, Some("ci.yml"))), [12]);
    assert_eq!(ids(select(&runs, false, Some(".forgejo/workflows/ci.yml"))), [12]);
    assert!(select(&runs, false, Some("nope.yml")).is_empty());
}

#[test]
fn hints_name_the_target_and_carry_an_explicit_repo() {
    let g = GlobalOpts::default();
    assert_eq!(hint("logs", &resolved(Some("2626")), &g), "fjo ci logs 2626");
    assert_eq!(hint("watch", &resolved(None), &g), "fjo ci watch");
    let g = GlobalOpts { repo: Some("o/r".parse().unwrap()), ..GlobalOpts::default() };
    assert_eq!(hint("logs", &resolved(Some("main")), &g), "fjo ci logs main -R o/r");
    let url = "https://git.example.org/o/r/pulls/7";
    assert_eq!(hint("logs", &resolved(Some(url)), &g), format!("fjo ci logs {url}"));
}

#[test]
fn the_empty_note_says_why_and_names_the_workflows_a_filter_missed() {
    let note = no_runs_message(&resolved(Some("2626")), &[], None);
    assert!(
        note.starts_with("no Actions runs for f36d1a7 (PR #2626 (renovate/zip-9.x))"),
        "{note}"
    );
    assert!(note.contains("`on:` filters"), "{note}");

    let runs = vec![run(1, "ci.yml", "success"), run(2, "lint.yml", "waiting")];
    let note = no_runs_message(&resolved(None), &runs, Some("nope.yml"));
    assert!(note.contains("no run of nope.yml") && note.ends_with("ci.yml, lint.yml"), "{note}");
}

// ---------------------------------------------------------------------------------- gather

/// Jobs are fetched for the failed and the pending run only — a run that passed or was skipped
/// costs nothing — and a waiting job is explained.
#[tokio::test]
async fn jobs_are_fetched_only_for_failed_and_pending_runs() {
    let runs = vec![
        run(6442, "ci.yml", "failure"),
        run(6443, "review.yml", "waiting"),
        run(6444, "pr-title.yml", "success"),
        run(6445, "docs.yml", "skipped"),
    ];
    let fake = on(
        on(
            testing::transport(),
            "GET",
            &jobs_path(6442),
            Canned::json(
                200,
                jobs_body(&[job(1, "Fast Checks", "failure"), job(2, "Docs", "success")]),
            ),
        ),
        "GET",
        &jobs_path(6443),
        Canned::json(
            200,
            jobs_body(&[{
                let mut j = job(3, "review", "waiting");
                j.runs_on = vec!["duck-runner-huge".into()];
                j
            }]),
        ),
    );
    let fake =
        Arc::new(on(fake, "GET", "/api/v1/repos/o/r/actions/runners", testing::one_page("[]")));
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
    let runs = vec![run(6442, "ci.yml", "failure"), run(6443, "x.yml", "waiting")];
    let fake = Arc::new(on(
        testing::transport(),
        "GET",
        &jobs_path(6442),
        Canned::json(200, jobs_body(&[job(1, "build", "failure")])),
    ));
    let selected = select(&runs, false, None);
    view::gather(&testing::api(fake.clone()), &slug(), &selected, false).await.unwrap();
    assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
}

// ------------------------------------------------------------------------------- the views

/// A finished run that took `seconds`, as Forgejo reports it: a Go `time.Duration`, in
/// nanoseconds.
fn timed(id: i64, file: &str, status: &str, seconds: i64) -> ActionRun {
    run_with(id, file, "pull_request", status, json!({ "duration": seconds * 1_000_000_000 }))
}

fn mixed_runs() -> Vec<ActionRun> {
    vec![
        timed(6442, "ci.yml", "failure", 404),
        run(6443, "claude-code-review.yml", "waiting"),
        timed(6444, "pr-title.yml", "success", 12),
        run(6445, "docs.yml", "skipped"),
        run_with(6446, "build.yml", "push", "running", json!({})),
        run_with(6447, "build.yml", "pull_request", "cancelled", json!({})),
    ]
}

fn mixed_views(runs: &[ActionRun]) -> Vec<RunView<'_>> {
    let mut views: Vec<RunView<'_>> =
        select(runs, false, None).into_iter().map(RunView::bare).collect();
    for v in &mut views {
        match v.run.id.get() {
            6442 => {
                v.failed_jobs = vec![
                    job(33898, "Fast Checks (fmt + static lints)", "failure"),
                    job(33900, "Build - workspace", "failure"),
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
    assert_eq!(next.as_deref(), Some("fjo ci logs 2626"));
    insta::assert_snapshot!(view::render(
        &resolved(Some("2626")),
        &views,
        &testing::term(),
        next.as_deref()
    ));
}

#[test]
fn status_human_output_when_all_is_well_has_no_next_line() {
    let runs = vec![timed(1, "ci.yml", "success", 61), run(2, "docs.yml", "skipped")];
    let views: Vec<RunView<'_>> =
        select(&runs, false, None).into_iter().map(RunView::bare).collect();
    assert_eq!(view::next_verb(&views), None);
    insta::assert_snapshot!(view::render(&resolved(None), &views, &testing::term(), None));
}

#[test]
fn status_human_output_pending_suggests_watch() {
    let runs = vec![timed(1, "ci.yml", "success", 61), run(2, "lint.yml", "running")];
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
    assert!(view::verdict(&resolved(Some("2626")), &[]).is_ok());
}

fn json_of(selected: &[&ActionRun], fields: &str) -> String {
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
    insta::assert_snapshot!(json_of(&selected, "id,index_in_repo,status,workflow_id,event"));
    assert_eq!(json_of(&[], "id").trim(), "[]");
}

// ----------------------------------------------------------------------------- exit codes

#[test]
fn a_failed_run_is_ci_failed_exit_1_naming_its_failed_jobs() {
    let runs = mixed_runs();
    let views = mixed_views(&runs);
    let err = view::verdict(&resolved(Some("2626")), &views).unwrap_err();
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
    let runs = [run(1, "ci.yml", "success"), run(2, "lint.yml", "waiting")];
    let views: Vec<RunView<'_>> = runs.iter().map(RunView::bare).collect();
    let err = view::verdict(&resolved(None), &views).unwrap_err();
    assert_eq!(err.exit_code(), 8);
    let ErrorKind::CiPending { target, pending, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!((target, pending.as_slice()), (&None, &["lint.yml".to_owned()][..]));
}

#[test]
fn skipped_counts_as_success() {
    let runs = [run(1, "ci.yml", "success"), run(2, "docs.yml", "skipped")];
    let views: Vec<RunView<'_>> = runs.iter().map(RunView::bare).collect();
    assert!(view::verdict(&resolved(None), &views).is_ok());
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
                jobs_body(&[job(11, "build", "failure"), job(12, "lint", "success")]),
            ),
        ),
        "GET",
        &jobs_path(6450),
        Canned::json(200, jobs_body(&[job(21, "build", "success"), job(22, "publish", "success")])),
    )
}

fn logs_runs() -> Vec<ActionRun> {
    vec![run(6442, "ci.yml", "failure"), run(6450, "release.yml", "success")]
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
    let picked =
        vec![(&runs[0], job(11, "build", "failure")), (&runs[1], job(21, "build", "success"))];
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
        vec![run(1, "ci.yml", "waiting"), run(2, "lint.yml", "running")],
        vec![run(1, "ci.yml", "running"), run(2, "lint.yml", "running")],
        vec![run(1, "ci.yml", "failure"), run(2, "lint.yml", "success")],
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
        ["ci.yml: waiting → running", "ci.yml: running → failure", "lint.yml: running → success"]
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
}

/// A run that appears after a few empty polls is followed like any other.
#[tokio::test]
async fn watch_follows_a_run_that_appears_late() {
    let replies = vec![
        Canned::json(200, runs_body(&[])),
        Canned::json(200, runs_body(&[run(1, "ci.yml", "waiting")])),
        Canned::json(200, runs_body(&[run(1, "ci.yml", "success")])),
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
    assert_eq!(runs[0].status, "success");
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

// ------------------------------------------------------------------------- Forgejo specifics

/// The failed-run fact carries the run's **id**, because its advice is `fjo run view <id>
/// --web` and the API path takes the id. Bug this prevents: passing `index_in_repo` — which is
/// what the run's own `html_url` carries on Forgejo — and so opening some other run's page, or
/// a 404, once an instance has more than one repository.
#[test]
fn a_failed_run_is_reported_by_its_id_not_the_number_in_its_url() {
    let runs = [run(6442, "ci.yml", "failure")];
    let views: Vec<RunView<'_>> = runs.iter().map(RunView::bare).collect();
    let err = view::verdict(&resolved(None), &views).unwrap_err();
    let ErrorKind::CiFailed { failed, .. } = err.kind() else { panic!("{err:?}") };
    assert_eq!(failed[0].id, 6442);
    assert_eq!(failed[0].url.as_deref(), Some("https://git.example.org/o/r/actions/runs/3623"));
    assert_ne!(runs[0].index_in_repo, 6442);
}

/// Forgejo has no rerun route, so there is no `ci rerun`. Bug this prevents: shipping the
/// Gitea port's subcommand, whose only request is a router 404 on Forgejo.
#[test]
fn there_is_no_rerun_subcommand() {
    let cmd = Harness::command();
    let ci = cmd.find_subcommand("ci").unwrap();
    assert!(ci.find_subcommand("rerun").is_none());
}

//! Gitea Actions with a **real runner attached**, so runs execute and finish.
//!
//! `live_actions.rs` covers everything that happens without a runner, and records where that
//! stops: a run is born `queued` and stays there, so no conclusion, no job outcome and no log is
//! ever observable. This file is the other half. It attaches the harness's host-mode act_runner
//! (see `gea_itest::runner`), pushes a workflow, lets it run to completion, and then asserts on
//! the outcome through the same `gea` commands a user would reach for.
//!
//! The runner is the one [`gea_itest::runner_or_skip!`] shares across this binary. If it cannot be
//! started the test skips — and under `cargo xtask itest` (which sets `GEA_ITEST_REQUIRE`) that
//! skip is a failure, so a green suite always means the jobs really ran.

use std::time::Duration;

use gea_itest::runner::{echo_unseen, unique_marker, workflow_yaml};
use gea_itest::{TestRepo, commit_and_push, cover, git, instance_or_skip, runner_or_skip};

/// Measured at ~3 s from push to `completed` on a warm runner; the rest is headroom for a loaded
/// CI machine. Kept well under nextest's 180 s per-test kill so a stuck run fails here, with the
/// list of what was seen, rather than as an anonymous kill.
const RUN_DEADLINE: Duration = Duration::from_secs(90);

/// Bug this prevents: the outcome commands describing a run correctly only in fixtures we wrote.
///
/// Before a runner existed in this suite, `run jobs`' conclusions, `run logs --failed`'s choice of
/// jobs and `run watch --exit-status`'s verdict had never seen a real finished run. Here one job
/// passes and one fails, each printing its own marker, so every command has something to get
/// wrong in a way a reader would notice:
///
/// * `run view --json` must report the run's own conclusion, `failure`, not a job's.
/// * `run jobs --json` must pair each job with *its* conclusion.
/// * `run logs --failed` must print the failing job's output and **only** that: marker B present,
///   marker A absent. The markers are printed in two halves (`echo_unseen`) because a job log
///   also contains the step's command text, so a literal `echo MARKER` would put the marker in
///   the log whether or not the step ever ran.
/// * `run watch --exit-status` on the finished run must exit non-zero and say `failure`. Exit
///   code is asserted as non-zero rather than a number: it is documented as a known wart (2,
///   where `gh` says 1) that is expected to change.
#[test]
fn a_run_with_a_passing_and_a_failing_job_reports_each_outcome_and_only_the_failed_log() {
    let inst = instance_or_skip!();
    let _runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["run view", "run jobs", "run logs", "run watch"],
        hits: ["GetWorkflowRun", "listWorkflowRunJobs", "downloadActionsRunJobLogs"],
    );

    let repo = TestRepo::create_initialized(inst, "runner-mixed");
    let slug = repo.slug();
    let marker_ok = unique_marker("ok");
    let marker_bad = unique_marker("bad");
    let yaml = workflow_yaml(
        "[push]",
        &[
            ("ok", &echo_unseen(&marker_ok)),
            ("bad", &format!("{}\nexit 1", echo_unseen(&marker_bad))),
        ],
    );

    let dir = tempfile::tempdir().expect("a scratch directory");
    let clone = dir.path().join("clone");
    repo.clone_to(&clone);
    std::fs::create_dir_all(clone.join(".github/workflows")).expect("create the workflow dir");
    commit_and_push(&clone, "main", ".github/workflows/ci.yml", &yaml, "Add a workflow");
    let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();

    // Out of band, so the wait does not depend on the commands under test.
    let runs = repo
        .wait_for_runs_completed(&sha, 1, RUN_DEADLINE)
        .unwrap_or_else(|e| panic!("{e}\nrunner logs:\n{}", _runner.logs()));
    assert_eq!(runs.len(), 1, "one push, one workflow, one run: {runs:?}");
    let id = runs[0]["id"].as_i64().expect("a run id").to_string();
    assert_eq!(runs[0]["event"], "push", "{runs:?}");

    let view = inst
        .gea(["run", "view", &id, "--json", "status,conclusion", "-R", &slug])
        .assert_ok("gea run view --json")
        .json();
    assert_eq!(view["status"], "completed", "{view}");
    assert_eq!(view["conclusion"], "failure", "one job failed, so the run did: {view}");

    let jobs = inst
        .gea(["run", "jobs", &id, "--json", "name,status,conclusion", "-R", &slug])
        .assert_ok("gea run jobs --json")
        .json();
    let outcome = |name: &str| {
        jobs.as_array()
            .and_then(|a| a.iter().find(|j| j["name"] == name))
            .map(|j| j["conclusion"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("no job named {name}: {jobs}"))
    };
    assert_eq!(outcome("ok"), "success", "{jobs}");
    assert_eq!(outcome("bad"), "failure", "{jobs}");

    let logs = inst.gea(["run", "logs", &id, "--failed", "-R", &slug]);
    logs.assert_ok("gea run logs --failed").assert_says(&marker_bad);
    assert!(
        !logs.stdout.contains(&marker_ok) && !logs.stderr.contains(&marker_ok),
        "--failed must not print the passing job's log, but it contains {marker_ok}:\n{}",
        logs.stdout
    );

    // The unfiltered logs must have both, or the absence above proves nothing.
    inst.gea(["run", "logs", &id, "-R", &slug])
        .assert_ok("gea run logs")
        .assert_says(&marker_ok)
        .assert_says(&marker_bad);

    let watched = inst.gea(["run", "watch", &id, "--exit-status", "-i", "1", "-R", &slug]);
    assert!(
        !watched.ok() && watched.code.is_some(),
        "watch --exit-status on a failed run must exit non-zero:\n{}\n{}",
        watched.stdout,
        watched.stderr
    );
    watched.assert_says("failure");
}

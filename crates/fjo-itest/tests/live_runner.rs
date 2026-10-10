//! Forgejo Actions with a **real runner attached**, so runs execute and finish.
//!
//! `live_actions.rs` covers everything that happens without a runner, and records where that
//! stops: a run is born `waiting` and stays there, so no outcome, no job status past `waiting`
//! and no log is ever observable. This file is the other half. It attaches the harness's
//! host-mode forgejo-runner (see `fjo_itest::runner`), pushes a workflow, lets it run to
//! completion, and then asserts on the outcome through the same `fjo` commands a user would reach
//! for.
//!
//! The runner is the one [`fjo_itest::runner_or_skip!`] shares across this binary. If it cannot be
//! started the test skips — and under `cargo xtask itest` (which sets `FJO_ITEST_REQUIRE`) that
//! skip is a failure, so a green suite always means the jobs really ran.

use std::time::Duration;

use fjo_itest::runner::{WORKFLOW_DIR, echo_unseen, unique_marker, workflow_yaml};
use fjo_itest::{TestRepo, commit_and_push, cover, git, instance_or_skip, runner_or_skip};

/// Measured at ~2 s from push to finished on a warm runner; the rest is headroom for a loaded
/// CI machine. Kept well under nextest's 180 s per-test kill so a stuck run fails here, with the
/// list of what was seen, rather than as an anonymous kill.
const RUN_DEADLINE: Duration = Duration::from_secs(90);

/// Bug this prevents: the outcome commands describing a run correctly only in fixtures we wrote.
///
/// Before a runner existed in this suite, `run jobs`' statuses, `run logs --failed`'s choice of
/// jobs and `run watch --exit-status`'s verdict had never seen a real finished run. Here one job
/// passes and one fails, each printing its own marker, so every command has something to get
/// wrong in a way a reader would notice:
///
/// * `run view --json` must report the run's own status, `failure`, and a real duration.
/// * `run jobs --json` must pair each job with *its* status.
/// * `run logs --failed` must print the failing job's output and **only** that: marker B present,
///   marker A absent. The markers are printed in two halves (`echo_unseen`) because a job log
///   also contains the step's command text, so a literal `echo MARKER` would put the marker in
///   the log whether or not the step ever ran.
/// * `run watch --exit-status` on the finished run must exit non-zero and say `failure`.
/// * The run-level log route, a zip, finally has a run with output to zip.
#[test]
fn a_run_with_a_passing_and_a_failing_job_reports_each_outcome_and_only_the_failed_log() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["run view", "run jobs", "run logs", "run watch"],
        hits: [
            "ActionRun", "ListActionRunJobs", "repoGetActionJobLogs", "repoGetActionRunLogs",
        ],
    );

    let repo = TestRepo::create_initialized(inst, "runner-mixed");
    let slug = repo.slug();
    let marker_ok = unique_marker("ok");
    let marker_bad = unique_marker("bad");
    let yaml = workflow_yaml(
        "[push]",
        &[
            ("ok", &echo_unseen(&marker_ok)),
            // The sleep gives the run a duration to read back. Measured: Forgejo stamps a run's
            // `stopped` when its status is *decided* — here, when `bad` fails — at one-second
            // granularity, and reports `duration` as the gap; a run decided in its first second
            // reports none at all.
            ("bad", &format!("sleep 2\n{}\nexit 1", echo_unseen(&marker_bad))),
        ],
    );

    let dir = tempfile::tempdir().expect("a scratch directory");
    let clone = dir.path().join("clone");
    repo.clone_to(&clone);
    std::fs::create_dir_all(clone.join(WORKFLOW_DIR)).expect("create the workflow dir");
    commit_and_push(&clone, "main", &format!("{WORKFLOW_DIR}/ci.yml"), &yaml, "Add a workflow");
    let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();

    // Out of band, so the wait does not depend on the commands under test — and to the jobs, not
    // only the run, which reads `failure` while `ok` may still be running.
    let runs = repo
        .wait_for_runs_finished(&sha, 1, RUN_DEADLINE)
        .unwrap_or_else(|e| panic!("{e}\nrunner logs:\n{}", runner.logs()));
    assert_eq!(runs.len(), 1, "one push, one workflow, one run: {runs:?}");
    assert_eq!(runs[0]["event"], "push", "{runs:?}");
    let id_n = runs[0]["id"].as_i64().expect("a run id");
    repo.wait_for_jobs_finished(id_n, RUN_DEADLINE)
        .unwrap_or_else(|e| panic!("{e}\nrunner logs:\n{}", runner.logs()));
    let id = id_n.to_string();

    let view = inst
        .fjo(["run", "view", &id, "--json", "status,workflow_id", "-R", &slug])
        .assert_ok("fjo run view --json")
        .json();
    assert_eq!(view["status"], "failure", "one job failed, so the run did: {view}");
    assert_eq!(view["workflow_id"], "ci.yml", "{view}");

    let jobs = inst
        .fjo(["run", "jobs", &id, "--json", "name,status", "-R", &slug])
        .assert_ok("fjo run jobs --json")
        .json();
    let status = |name: &str| {
        jobs.as_array()
            .and_then(|a| a.iter().find(|j| j["name"] == name))
            .map(|j| j["status"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("no job named {name}: {jobs}"))
    };
    assert_eq!(status("ok"), "success", "{jobs}");
    assert_eq!(status("bad"), "failure", "{jobs}");

    let logs = inst.fjo(["run", "logs", &id, "--failed", "-R", &slug]);
    logs.assert_ok("fjo run logs --failed").assert_says(&marker_bad);
    assert!(
        !logs.stdout.contains(&marker_ok) && !logs.stderr.contains(&marker_ok),
        "--failed must not print the passing job's log, but it contains {marker_ok}:\n{}",
        logs.stdout
    );

    // The unfiltered logs must have both, or the absence above proves nothing.
    inst.fjo(["run", "logs", &id, "-R", &slug])
        .assert_ok("fjo run logs")
        .assert_says(&marker_ok)
        .assert_says(&marker_bad);

    let watched = inst.fjo(["run", "watch", &id, "--exit-status", "-i", "1", "-R", &slug]);
    assert!(
        !watched.ok() && watched.code.is_some(),
        "watch --exit-status on a failed run must exit non-zero:\n{}\n{}",
        watched.stdout,
        watched.stderr
    );
    watched.assert_says("failure");

    // The run-level route answers a zip of every job's log. Before a runner existed there was no
    // run with output to zip, and this operation was exempt from live coverage for exactly that.
    let zip = inst.fjo(["raw", "repo", "get-action-run-logs", &repo.owner, &repo.name, &id]);
    zip.assert_ok("raw repo get-action-run-logs");
    assert!(zip.stdout.starts_with("PK"), "a zip archive: {:?}", zip.stdout.get(..16));

    // A human `run view` prints the duration Forgejo reports in nanoseconds as seconds. Bug this
    // prevents: a two-second run reading `33333333m20s`.
    let human = inst.fjo(["run", "view", &id, "-R", &slug]);
    human.assert_ok("fjo run view");
    let line = human
        .stdout
        .lines()
        .find(|l| l.trim_start().starts_with("duration"))
        .unwrap_or_else(|| panic!("a duration row:\n{}", human.stdout));
    let value = line.split_whitespace().last().unwrap_or_default();
    let seconds = value.strip_suffix('s').and_then(|n| n.parse::<u32>().ok());
    assert!(
        seconds.is_some_and(|n| (1..60).contains(&n)),
        "a run decided after a two-second sleep lasts seconds, not nanoseconds read as seconds: {line:?}"
    );
}

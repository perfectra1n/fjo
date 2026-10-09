//! `gea ci` (and the run/job references and hints it brought to `gea run` and `gea pr checks`)
//! driven against runs that really executed.
//!
//! Every test here pushes a workflow to a repository of its own, lets the shared host-mode
//! act_runner (see `gea_itest::runner`) execute it, and then asserts on what a user would see.
//! The waits are done out of band, through the API, so a test does not depend on the command it
//! is testing to know when to start looking.
//!
//! Exit codes are asserted exactly, because they are the interface a script reads: 0 passed,
//! 1 failed (`CiFailed`), 2 usage, 8 still running (`CiPending`).
//!
//! Job logs contain each step's command text as well as its output, so the markers asserted on
//! are printed in two halves (`echo_unseen`): a marker in the log proves the step ran, not merely
//! that the log quoted the script.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gea_itest::runner::{echo_unseen, unique_marker, workflow_yaml};
use gea_itest::{
    Instance, TestRepo, commit_and_push, cover, git, instance_or_skip, runner_or_skip,
};
use serde_json::Value;

/// Measured at ~3 s from push to `completed` on a warm runner; the rest is headroom for a loaded
/// machine, kept under nextest's per-test kill so a stuck run fails here with what was seen.
const RUN_DEADLINE: Duration = Duration::from_secs(90);

/// A repository with one workflow pushed to `branch`, and a checkout of it.
struct Pushed {
    repo: TestRepo<'static>,
    /// Holds the checkout alive.
    _dir: tempfile::TempDir,
    clone: PathBuf,
    branch: String,
    sha: String,
}

impl Pushed {
    fn new(
        inst: &'static Instance,
        prefix: &str,
        branch: &str,
        on: &str,
        jobs: &[(&str, &str)],
    ) -> Self {
        let repo = TestRepo::create_initialized(inst, prefix);
        let dir = tempfile::tempdir().expect("a scratch directory");
        let clone = dir.path().join("clone");
        repo.clone_to(&clone);
        std::fs::create_dir_all(clone.join(".github/workflows")).expect("create the workflow dir");
        commit_and_push(
            &clone,
            branch,
            ".github/workflows/ci.yml",
            &workflow_yaml(on, jobs),
            "Add CI",
        );
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();
        Self { repo, _dir: dir, clone, branch: branch.to_owned(), sha }
    }

    fn slug(&self) -> String {
        self.repo.slug()
    }

    /// Wait for `at_least` runs on the pushed commit to complete, panicking with the runner's
    /// logs when they do not.
    fn wait(&self, at_least: usize, runner: &gea_itest::runner::RunnerGuard<'_>) -> Vec<Value> {
        self.repo
            .wait_for_runs_completed(&self.sha, at_least, RUN_DEADLINE)
            .unwrap_or_else(|e| panic!("{e}\nrunner logs:\n{}", runner.logs()))
    }

    /// The run as the API has it now.
    fn run(&self, id: i64) -> Value {
        let (code, body) = self.repo.api("GET", &format!("actions/runs/{id}"), None);
        assert_eq!(code, 200, "reading run {id}: {body}");
        serde_json::from_str(&body).expect("a run")
    }

    /// The run's jobs as the API has them now.
    fn jobs(&self, id: i64) -> Vec<Value> {
        let (code, body) = self.repo.api("GET", &format!("actions/runs/{id}/jobs"), None);
        assert_eq!(code, 200, "listing run {id}'s jobs: {body}");
        let v: Value = serde_json::from_str(&body).expect("a job list");
        v["jobs"].as_array().cloned().unwrap_or_default()
    }
}

/// The `ok` + `bad` pair every failing test uses: `ok` prints its marker and passes, `bad` prints
/// its marker and exits 1.
fn ok_and_bad(marker_ok: &str, marker_bad: &str) -> [(String, String); 2] {
    [
        ("ok".to_owned(), echo_unseen(marker_ok)),
        ("bad".to_owned(), format!("{}\nexit 1", echo_unseen(marker_bad))),
    ]
}

fn as_refs(jobs: &[(String, String)]) -> Vec<(&str, &str)> {
    jobs.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect()
}

fn id_of(run: &Value) -> i64 {
    run["id"].as_i64().unwrap_or_else(|| panic!("a run id: {run}"))
}

/// Open a pull request from `branch` into `main` over the API, and answer its number.
fn open_pr(repo: &TestRepo<'_>, branch: &str) -> i64 {
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(&format!(r#"{{"title":"CI for {branch}","head":"{branch}","base":"main"}}"#)),
    );
    assert_eq!(code, 201, "could not open a pull request from {branch}: {body}");
    let pr: Value = serde_json::from_str(&body).expect("a pull request");
    pr["number"].as_i64().expect("a pull request number")
}

/// Bug this prevents: `gea ci` getting the verdict, the failing job or the next step wrong on a
/// real failed run — exactly what the porcelain exists to say.
///
/// One push, one run, one job passing and one failing. Every `gea ci` subcommand is driven over
/// it: `watch` must exit 1 once finished; `status` must exit 1, name `bad` and the run id, and end
/// with `next: gea ci logs <branch>`; `--json` must decode and keep the exit code; `logs` must
/// print the failing job's output and *only* that; and `rerun` must make the server actually start
/// the failed job again, which is checked through the API rather than through `gea`.
#[test]
fn a_failed_push_run_is_reported_logged_and_rerun_through_gea_ci() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "ci watch", "ci logs", "ci rerun"],
        hits: [
            "getWorkflowRuns", "listWorkflowRunJobs", "repoGetBranch", "repoListPullRequests",
            "downloadActionsRunJobLogs", "rerunFailedWorkflowRun",
        ],
    );
    let (marker_ok, marker_bad) = (unique_marker("ci-ok"), unique_marker("ci-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-fail", "feature-fail", "[push]", &as_refs(&jobs));
    let (slug, branch) = (p.slug(), p.branch.clone());
    let runs = p.wait(1, runner);
    assert_eq!(runs.len(), 1, "one push, one workflow: {runs:?}");
    let id = id_of(&runs[0]);

    inst.gea(["ci", "watch", &branch, "-i", "1", "-R", &slug])
        .assert_code(1, "gea ci watch on a finished, failed run");

    for argv in [vec!["ci", branch.as_str()], vec!["ci", "status", branch.as_str()]] {
        let status = inst.gea(argv.iter().copied().chain(["-R", &slug]));
        status.assert_code(1, "gea ci on a failed run");
        let out = &status.stdout;
        assert!(out.contains(&format!("run {id}")), "the run id is named:\n{out}");
        assert!(
            out.lines().any(|l| l.contains('✗') && l.trim_start().contains("bad")),
            "the failing job is named under its run:\n{out}"
        );
        assert!(
            !out.lines().any(|l| l.trim_start().starts_with("✗ ok")),
            "the passing job is not listed as failed:\n{out}"
        );
        assert!(
            out.contains(&format!("next: gea ci logs {branch}")),
            "the next command is the failed job's logs:\n{out}"
        );
    }

    let json = inst.gea(["ci", &branch, "--json", "id,conclusion", "-R", &slug]);
    json.assert_code(1, "gea ci --json keeps the exit code");
    let v = json.json();
    let arr = v.as_array().unwrap_or_else(|| panic!("--json prints the runs as an array: {v}"));
    assert_eq!(arr.len(), 1, "{v}");
    assert_eq!(arr[0]["id"], id, "{v}");
    assert_eq!(arr[0]["conclusion"], "failure", "{v}");

    let logs = inst.gea(["ci", "logs", &branch, "-R", &slug]);
    logs.assert_ok("gea ci logs").assert_says(&marker_bad);
    assert!(
        !logs.stdout.contains(&marker_ok) && !logs.stderr.contains(&marker_ok),
        "only the failed job's log is printed, but it contains {marker_ok}:\n{}",
        logs.stdout
    );
    assert!(logs.stdout.contains(&format!("ci.yml / bad (run {id}, job ")), "{}", logs.stdout);

    let bad_job = |p: &Pushed| {
        p.jobs(id)
            .into_iter()
            .find(|j| j["name"] == "bad")
            .unwrap_or_else(|| panic!("run {id} has no job named bad"))
    };
    let before = bad_job(&p);
    let attempt_before = p.run(id)["run_attempt"].as_i64();

    let rerun = inst.gea(["ci", "rerun", &branch, "-R", &slug]);
    rerun.assert_ok("gea ci rerun");
    assert!(
        rerun.stderr.contains(&format!("reran failed jobs of ci.yml (run {id})")),
        "{}",
        rerun.stderr
    );
    assert!(rerun.stdout.contains(&format!("next: gea ci watch {branch}")), "{}", rerun.stdout);

    // The server really started the failed job again: a new attempt, or the job is no longer the
    // finished one we read before. Polled, because a fast runner can finish the rerun at once.
    let deadline = Instant::now() + RUN_DEADLINE;
    loop {
        let run = p.run(id);
        let attempt_now = run["run_attempt"].as_i64();
        let job = bad_job(&p);
        let moved = attempt_now > attempt_before
            || job["id"] != before["id"]
            || job["status"] != "completed"
            || job["started_at"] != before["started_at"]
            || run["status"] != "completed";
        if moved {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "gea ci rerun exited 0 but run {id} never started again: {run}\njob: {job}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let after = p.wait(1, runner);
    let run = p.run(id);
    assert_eq!(run["status"], "completed", "{after:?}");
    assert_eq!(run["conclusion"], "failure", "the rerun fails again, as written: {run}");
    // The passing job was not rerun: only the failed jobs were asked for.
    let ok_now = p.jobs(id).into_iter().find(|j| j["name"] == "ok").expect("job ok");
    assert_eq!(ok_now["conclusion"], "success", "{ok_now}");
}

/// Bug this prevents: an all-green commit reading as anything but green — a stray `next:` line, or
/// `gea ci logs` exiting non-zero because there was nothing to print.
#[test]
fn an_all_green_run_exits_zero_with_no_next_step_and_no_logs_to_print() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "ci logs"],
        hits: ["getWorkflowRuns", "repoGetBranch", "repoListPullRequests"],
    );
    let marker = unique_marker("green");
    let p =
        Pushed::new(inst, "ci-green", "feature-green", "[push]", &[("ok", &echo_unseen(&marker))]);
    let (slug, branch) = (p.slug(), p.branch.clone());
    let runs = p.wait(1, runner);
    let id = id_of(&runs[0]);
    assert_eq!(runs[0]["conclusion"], "success", "{runs:?}");

    let status = inst.gea(["ci", &branch, "-R", &slug]);
    status.assert_ok("gea ci on a green run");
    assert!(status.stdout.contains(&format!("run {id}")), "{}", status.stdout);
    assert!(status.stdout.contains("success"), "{}", status.stdout);
    assert!(!status.stdout.contains("next:"), "nothing to do next when green:\n{}", status.stdout);

    let logs = inst.gea(["ci", "logs", &branch, "-R", &slug]);
    logs.assert_ok("gea ci logs with nothing failed");
    assert!(logs.stdout.is_empty(), "no log is printed when nothing failed:\n{}", logs.stdout);
    let short = &p.sha[..7];
    assert!(
        logs.stderr.contains(&format!("no job failed for {short}; --all prints every job")),
        "the nothing-failed note is on stderr even when piped:\n{}",
        logs.stderr
    );

    // And --all does print the green job, so the empty default above means "filtered", not
    // "could not read".
    inst.gea(["ci", "logs", &branch, "--all", "-R", &slug])
        .assert_ok("gea ci logs --all")
        .assert_says(&marker);
}

/// Bug this prevents — the one the design exists for: CI that ran only as `pull_request` reading
/// as "CI never ran".
///
/// Gitea files a `pull_request` run under no branch (its `head_branch` is empty), so
/// `gea run list --branch` cannot find it. `gea ci` finds runs by commit instead; here the same
/// commit has a `push` run and a `pull_request` run, and both `gea ci <pr>` and `gea ci <branch>`
/// must show the `pull_request` one. The contrast is asserted too, so this test fails if Gitea
/// ever starts filing those runs under their branch and the premise stops holding.
#[test]
fn a_pull_request_run_with_no_branch_is_found_by_pr_number_and_by_branch() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "run list"],
        hits: [
            "getWorkflowRuns", "repoGetPullRequest", "repoListPullRequests",
            "listWorkflowRunJobs",
        ],
    );
    let (marker_ok, marker_bad) = (unique_marker("pr-ok"), unique_marker("pr-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-pr", "feature-pr", "[push, pull_request]", &as_refs(&jobs));
    let (slug, branch) = (p.slug(), p.branch.clone());
    let pr = open_pr(&p.repo, &branch).to_string();
    let runs = p.wait(2, runner);
    let pr_run = runs
        .iter()
        .find(|r| r["event"] == "pull_request")
        .unwrap_or_else(|| panic!("opening the pull request started a pull_request run: {runs:?}"));
    let pr_run_id = id_of(pr_run);
    assert!(
        pr_run["head_branch"].as_str().unwrap_or_default().is_empty(),
        "the premise: Gitea files a pull_request run under no branch: {pr_run}"
    );

    for target in [pr.as_str(), &format!("#{pr}"), branch.as_str()] {
        let found = inst.gea(["ci", target, "--json", "id,event,head_branch", "-R", &slug]);
        found.assert_code(1, &format!("gea ci {target} on a failed pull request"));
        let v = found.json();
        let listed = v.as_array().unwrap_or_else(|| panic!("an array: {v}"));
        let hit = listed
            .iter()
            .find(|r| r["event"] == "pull_request")
            .unwrap_or_else(|| panic!("gea ci {target} lost the pull_request run: {v}"));
        assert_eq!(hit["id"], pr_run_id, "{v}");
        assert!(
            hit["head_branch"].as_str().unwrap_or_default().is_empty(),
            "it is the branchless run: {v}"
        );
        assert!(listed.iter().any(|r| r["event"] == "push"), "and the push run is kept: {v}");

        let human = inst.gea(["ci", target, "-R", &slug]);
        human.assert_code(1, &format!("gea ci {target}"));
        assert!(human.stdout.starts_with(&format!("PR #{pr} ")), "{}", human.stdout);
        assert!(human.stdout.contains(&format!("run {pr_run_id}")), "{}", human.stdout);
        assert!(human.stdout.contains("ci.yml (pull_request)"), "{}", human.stdout);
    }

    // The contrast: the branch filter finds no pull_request run, and says where to look instead.
    let listed = inst.gea([
        "run",
        "list",
        "--branch",
        &branch,
        "--event",
        "pull_request",
        "--json",
        "id",
        "-R",
        &slug,
    ]);
    listed.assert_ok("gea run list --branch --event pull_request");
    assert_eq!(listed.json(), serde_json::json!([]), "the branch filter cannot see it");
    let piped =
        inst.gea(["run", "list", "--branch", &branch, "--event", "pull_request", "-R", &slug]);
    piped.assert_ok("gea run list --branch, piped");
    assert!(piped.stdout.trim().is_empty(), "nothing listed:\n{}", piped.stdout);
    // The pointer at `gea ci` is a support::note, which only a terminal gets.
    let tty = inst.gea_env(
        std::path::Path::new("."),
        &[("GEA_FORCE_TTY", "true")],
        ["run", "list", "--branch", &branch, "--event", "pull_request", "-R", &slug],
    );
    tty.assert_ok("gea run list --branch, on a terminal");
    assert!(
        tty.stderr.contains(&format!("`gea ci {branch}` finds them by commit")),
        "the empty listing points at gea ci:\n{}",
        tty.stderr
    );
}

/// Bug this prevents: run and job references that parse in a unit test but address the wrong
/// thing on a real server — ids are instance-global while run numbers are per repository, so the
/// two collide in exactly the way the fallback exists for.
///
/// `#N`, the run's own web URL and a bare number that is not an id of this repository must all
/// land on the same run, the last with a note saying it fell back. `--job` must pick a job by
/// exact name and refuse a substring two jobs share, naming both with their ids.
#[test]
fn run_view_and_logs_accept_numbers_urls_and_job_names() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["run view", "run logs"],
        hits: ["GetWorkflowRun", "getWorkflowRuns", "listWorkflowRunJobs", "downloadActionsRunJobLogs"],
    );
    let (ma, mb, mbad) =
        (unique_marker("lint-a"), unique_marker("lint-b"), unique_marker("refs-bad"));
    let p = Pushed::new(
        inst,
        "ci-refs",
        "main",
        "[push]",
        &[
            ("lint-a", &echo_unseen(&ma)),
            ("lint-b", &echo_unseen(&mb)),
            ("bad", &format!("{}\nexit 1", echo_unseen(&mbad))),
        ],
    );
    let slug = p.slug();
    let runs = p.wait(1, runner);
    let run = &runs[0];
    let id = id_of(run);
    let number = run["run_number"].as_i64().expect("a run number");
    let url = run["html_url"].as_str().expect("an html_url").to_owned();

    let view = |what: &str| inst.gea(["run", "view", what, "--json", "id,run_number", "-R", &slug]);
    let by_number = view(&format!("#{number}"));
    assert_eq!(by_number.assert_ok("gea run view #N").json()["id"], id, "#{number}");
    let by_url = view(&url);
    assert_eq!(by_url.assert_ok("gea run view <url>").json()["id"], id, "{url}");

    // A bare number is an id first. Ids are instance-global and this run is one of many the suite
    // makes, so its number (1) is usually not also its id; when it is (this was the instance's
    // first run), the bare form simply resolves by id and there is no fallback to observe, so the
    // note is asserted only otherwise, and the skip is said out loud. A different run of *this*
    // repository can never hold id `number`: this repository has one run.
    let bare = view(&number.to_string());
    assert_eq!(bare.assert_ok("gea run view <bare number>").json()["id"], id, "bare {number}");
    if id == number {
        eprintln!("run {id} is also #{number}; the id-miss fallback cannot be observed this time");
    } else {
        assert!(
            bare.stderr
                .contains(&format!("note: no run has id {number}; using run #{number} (id {id})")),
            "the id miss falls back to the run number, and says so:\n{}",
            bare.stderr
        );
    }

    let logs = inst.gea(["run", "logs", &id.to_string(), "--job", "bad", "-R", &slug]);
    logs.assert_ok("gea run logs --job bad").assert_says(&mbad);
    assert!(!logs.stdout.contains(&ma) && !logs.stdout.contains(&mb), "{}", logs.stdout);

    let exact = inst.gea(["run", "logs", &id.to_string(), "--job", "lint-b", "-R", &slug]);
    exact.assert_ok("gea run logs --job lint-b").assert_says(&mb);
    assert!(!exact.stdout.contains(&ma), "an exact name picks one job:\n{}", exact.stdout);

    let jobs = p.jobs(id);
    let job_id = |name: &str| {
        jobs.iter()
            .find(|j| j["name"] == name)
            .and_then(|j| j["id"].as_i64())
            .unwrap_or_else(|| panic!("no job {name}: {jobs:?}"))
    };
    // A pasted job URL — the job's own `html_url`, `…/runs/<id>/jobs/<job id>` — means that job.
    let job_url = jobs
        .iter()
        .find(|j| j["name"] == "lint-b")
        .and_then(|j| j["html_url"].as_str())
        .expect("lint-b has an html_url")
        .to_owned();
    let by_job_url = inst.gea(["run", "logs", &job_url, "-R", &slug]);
    by_job_url.assert_ok("gea run logs <job url>").assert_says(&mb);
    assert!(
        !by_job_url.stdout.contains(&ma) && !by_job_url.stdout.contains(&mbad),
        "a job URL prints that job only ({job_url}):\n{}",
        by_job_url.stdout
    );

    let ambiguous = inst.gea(["run", "logs", &id.to_string(), "--job", "lint", "-R", &slug]);
    ambiguous.assert_code(2, "an ambiguous --job");
    for name in ["lint-a", "lint-b"] {
        ambiguous.assert_says(&format!("{name} (id {})", job_id(name)));
    }
    assert!(!ambiguous.stdout.contains(&ma), "nothing is printed for a refused --job");
}

/// Bug this prevents: `gea pr checks` reporting a red pull request without saying how to see
/// why. Piped, as here, the summary and `next:` hint go to stderr so stdout stays TSV.
#[test]
fn pr_checks_on_a_failing_pull_request_exits_1_and_points_at_gea_ci_logs() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["pr checks", "ci logs"],
        hits: [
            "repoGetPullRequest", "repoGetCombinedStatusByRef", "getWorkflowRuns",
            "listWorkflowRunJobs", "downloadActionsRunJobLogs",
        ],
    );
    let (marker_ok, marker_bad) = (unique_marker("chk-ok"), unique_marker("chk-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-checks", "feature-checks", "[pull_request]", &as_refs(&jobs));
    let slug = p.slug();
    let pr = open_pr(&p.repo, &p.branch).to_string();
    p.wait(1, runner);

    let checks = inst.gea(["pr", "checks", &pr, "-R", &slug]);
    checks.assert_code(1, "gea pr checks on a failing pull request");
    assert!(checks.stdout.contains("fail"), "{}", checks.stdout);
    assert!(
        !checks.stdout.contains("next:"),
        "stdout stays pure TSV when piped:\n{}",
        checks.stdout
    );
    assert!(
        checks.stderr.contains(&format!("next: gea ci logs {pr}")),
        "the hint names the next command:\n{}",
        checks.stderr
    );

    // And the hint works as printed.
    inst.gea(["ci", "logs", &pr, "-R", &slug])
        .assert_ok("the hinted gea ci logs")
        .assert_says(&marker_bad);
}

/// Bug this prevents: `gea ci` with no target resolving "here" to the wrong commit — the local
/// HEAD instead of what the server ran CI on — or doing so silently.
#[test]
fn bare_gea_ci_in_a_checkout_shows_the_branch_and_warns_when_local_head_is_ahead() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status"],
        hits: ["getWorkflowRuns", "repoGetBranch", "repoListPullRequests"],
    );
    let p = Pushed::new(inst, "ci-here", "feature-here", "[push]", &[("ok", "true")]);
    let runs = p.wait(1, runner);
    let id = id_of(&runs[0]);

    let here = inst.gea_in(&p.clone, ["ci"]);
    here.assert_ok("bare gea ci in a checkout");
    assert!(
        here.stdout.starts_with(&format!("branch {} at {}", p.branch, &p.sha[..7])),
        "{}",
        here.stdout
    );
    assert!(here.stdout.contains(&format!("run {id}")), "{}", here.stdout);
    assert!(!here.stderr.contains("local HEAD"), "in sync, so no warning:\n{}", here.stderr);

    // Commit without pushing: CI is still about the server's commit, and the user is told.
    std::fs::write(p.clone.join("local.txt"), "unpushed\n").expect("write a file");
    git(&p.clone, &["add", "-A"]);
    git(&p.clone, &["commit", "--quiet", "-m", "not pushed"]);
    let local = git(&p.clone, &["rev-parse", "--short=7", "HEAD"]).trim().to_owned();
    let ahead = inst.gea_in(&p.clone, ["ci"]);
    ahead.assert_ok("bare gea ci with an unpushed commit");
    assert!(ahead.stdout.contains(&format!("at {}", &p.sha[..7])), "{}", ahead.stdout);
    assert!(
        ahead.stderr.contains(&format!(
            "note: local HEAD {local} is not what the server has for {} ({})",
            p.branch,
            &p.sha[..7]
        )),
        "the mismatch is warned about even when piped:\n{}",
        ahead.stderr
    );
}

/// Bug this prevents: `gea ci watch` returning while a run is still going — the one thing
/// `gea ci watch && gea pr merge` relies on it never doing.
#[test]
fn ci_watch_waits_for_a_running_job_and_reports_its_transition() {
    let inst = instance_or_skip!();
    let _runner = runner_or_skip!(inst);
    cover!(porcelain: ["ci watch"], hits: ["getWorkflowRuns", "repoGetBranch", "repoListPullRequests"]);
    let marker = unique_marker("slow");
    let p = Pushed::new(
        inst,
        "ci-watch",
        "feature-slow",
        "[push]",
        &[("slow", &format!("sleep 5\n{}", echo_unseen(&marker)))],
    );
    let slug = p.slug();

    // Straight after the push: the run is queued, starting, or not even created yet — never done,
    // since its only step sleeps for 5 s.
    let started = Instant::now();
    let watched = inst.gea_env(
        std::path::Path::new("."),
        &[("GEA_FORCE_TTY", "true")],
        ["ci", "watch", &p.branch, "-i", "1", "-R", &slug],
    );
    let took = started.elapsed();
    watched.assert_ok("gea ci watch on a run that passes");

    // The API agrees it was over by the time watch returned.
    let (code, body) = p.repo.api("GET", &format!("actions/runs?head_sha={}", p.sha), None);
    assert_eq!(code, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("runs");
    let runs = v["workflow_runs"].as_array().cloned().unwrap_or_default();
    assert_eq!(runs.len(), 1, "{v}");
    assert_eq!(runs[0]["status"], "completed", "watch returned before the run finished: {v}");
    assert_eq!(runs[0]["conclusion"], "success", "{v}");
    assert!(took >= Duration::from_secs(4), "a 5 s job was watched in {took:?}");

    assert!(
        watched.stderr.lines().any(|l| l.starts_with("ci.yml: ") && l.ends_with("→ success")),
        "a status transition into success was printed:\n{}",
        watched.stderr
    );
    assert!(watched.stdout.contains("success"), "{}", watched.stdout);
    assert!(!watched.stdout.contains("next:"), "{}", watched.stdout);
}

/// Bug this prevents: the raw Actions operations that only mean something on a run that has
/// executed — reruns, attempts, a single job — being wired to the wrong route or shape and
/// nobody noticing, because without a runner there was never a finished run to call them on.
#[test]
fn raw_rerun_attempt_and_job_operations_work_on_an_executed_run() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(raw: [
        "getWorkflowJob", "getWorkflowRunAttempt", "listWorkflowRunAttemptJobs",
        "rerunFailedWorkflowRun", "rerunWorkflowJob", "ActionsGetWorkflow",
    ]);
    let (marker_ok, marker_bad) = (unique_marker("raw-ok"), unique_marker("raw-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-raw", "main", "[push]", &as_refs(&jobs));
    let (owner, name) = (p.repo.owner.clone(), p.repo.name.clone());
    let runs = p.wait(1, runner);
    let id = id_of(&runs[0]);
    let rid = id.to_string();
    let jobs = p.jobs(id);
    let bad = jobs.iter().find(|j| j["name"] == "bad").expect("job bad").clone();
    let bad_id = bad["id"].as_i64().expect("a job id").to_string();

    let job =
        inst.gea(["raw", "job", "view", &owner, &name, &bad_id]).assert_ok("raw job view").json();
    assert_eq!(job["name"], "bad", "{job}");
    assert_eq!(job["conclusion"], "failure", "{job}");
    assert_eq!(job["run_id"], id, "{job}");

    let attempt = inst
        .gea(["raw", "run", "attempt", &owner, &name, &rid, "1"])
        .assert_ok("raw run attempt")
        .json();
    assert_eq!(attempt["id"], id, "attempt 1 is the run as it first ran: {attempt}");
    assert_eq!(attempt["conclusion"], "failure", "{attempt}");

    let attempt_jobs = inst
        .gea(["raw", "run", "attempt-jobs", &owner, &name, &rid, "1"])
        .assert_ok("raw run attempt-jobs")
        .json();
    let names: Vec<&str> = attempt_jobs["jobs"]
        .as_array()
        .unwrap_or_else(|| panic!("a job list: {attempt_jobs}"))
        .iter()
        .filter_map(|j| j["name"].as_str())
        .collect();
    assert!(names.contains(&"ok") && names.contains(&"bad"), "{attempt_jobs}");

    let wf = inst
        .gea(["raw", "workflow", "view", &owner, &name, "ci.yml"])
        .assert_ok("raw workflow view")
        .json();
    assert!(wf["path"].as_str().unwrap_or_default().ends_with("ci.yml"), "{wf}");

    // Rerun the failed jobs, wait it out, then rerun one job by id and wait that out too. Each
    // must start the run again, which the run's attempt counter records. A rerun makes a new
    // attempt with new job rows (measured: jobs 1 and 2 become 3 and 4), and the per-job rerun
    // route only knows the current attempt's jobs, so `bad`'s id is read again in between.
    let attempt_of = |p: &Pushed| p.run(id)["run_attempt"].as_i64().unwrap_or(0);
    let wait_new_attempt = |p: &Pushed, last: i64, what: &str| {
        let deadline = Instant::now() + RUN_DEADLINE;
        while attempt_of(p) <= last {
            assert!(
                Instant::now() < deadline,
                "{what} exited 0 but run {id} never got a new attempt: {}",
                p.run(id)
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        p.wait(1, runner);
        attempt_of(p)
    };
    let first = attempt_of(&p);
    assert_eq!(first, 1, "{}", p.run(id));
    inst.gea(["raw", "run", "rerun-failed", &owner, &name, &rid]).assert_ok("raw run rerun-failed");
    let second = wait_new_attempt(&p, first, "raw run rerun-failed");

    let bad_now = p
        .jobs(id)
        .into_iter()
        .find(|j| j["name"] == "bad")
        .and_then(|j| j["id"].as_i64())
        .expect("job bad in the new attempt")
        .to_string();
    assert_ne!(bad_now, bad_id, "a rerun gives the job a new row");
    inst.gea(["raw", "run", "rerun-job", &owner, &name, &rid, &bad_now])
        .assert_ok("raw run rerun-job");
    wait_new_attempt(&p, second, "raw run rerun-job");
}

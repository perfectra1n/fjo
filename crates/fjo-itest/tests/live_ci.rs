//! `fjo ci` (and the run/job references and hints it brought to `fjo run` and `fjo pr checks`)
//! driven against runs that really executed.
//!
//! Every test here pushes a workflow to a repository of its own, lets the shared host-mode
//! forgejo-runner (see `fjo_itest::runner`) execute it, and then asserts on what a user would see.
//! The waits are done out of band, through the API, so a test does not depend on the command it
//! is testing to know when to start looking.
//!
//! Exit codes are asserted exactly, because they are the interface a script reads: 0 passed,
//! 1 failed (`CiFailed`), 2 usage, 8 still running (`CiPending`).
//!
//! Job logs contain each step's command text as well as its output, so the markers asserted on
//! are printed in two halves (`echo_unseen`): a marker in the log proves the step ran, not merely
//! that the log quoted the script.
//!
//! Where Forgejo differs from the Gitea these tests were translated from, the test says so and
//! asserts the Forgejo behaviour — the premise, not just the command — so a server release that
//! changes it fails here rather than quietly invalidating what `fjo ci` assumes. Those are: a
//! `pull_request` run's ref is the pull request (`prettyref` `#<n>`); a run URL carries the run
//! number and a job URL the job's position; and there is no rerun route, so no `ci rerun`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use fjo_itest::runner::{WORKFLOW_DIR, echo_unseen, unique_marker, workflow_yaml};
use fjo_itest::{
    Instance, TestRepo, commit_and_push, cover, git, instance_or_skip, runner_or_skip,
};
use serde_json::Value;

/// Measured at ~2 s from push to finished on a warm runner; the rest is headroom for a loaded
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
        std::fs::create_dir_all(clone.join(WORKFLOW_DIR)).expect("create the workflow dir");
        commit_and_push(
            &clone,
            branch,
            &format!("{WORKFLOW_DIR}/ci.yml"),
            &workflow_yaml(on, jobs),
            "Add CI",
        );
        let sha = git(&clone, &["rev-parse", "HEAD"]).trim().to_owned();
        Self { repo, _dir: dir, clone, branch: branch.to_owned(), sha }
    }

    fn slug(&self) -> String {
        self.repo.slug()
    }

    /// Wait for `at_least` runs on the pushed commit to finish — **and every job of each** —
    /// panicking with the runner's logs when they do not. The jobs matter on Forgejo, whose run
    /// reads `failure` while its other jobs are still running.
    fn wait(&self, at_least: usize, runner: &fjo_itest::runner::RunnerGuard<'_>) -> Vec<Value> {
        let fail = |e: String| panic!("{e}\nrunner logs:\n{}", runner.logs());
        let runs = self
            .repo
            .wait_for_runs_finished(&self.sha, at_least, RUN_DEADLINE)
            .unwrap_or_else(fail);
        for run in &runs {
            self.repo.wait_for_jobs_finished(id_of(run), RUN_DEADLINE).unwrap_or_else(fail);
        }
        // Re-read: a run's own fields (its duration) settle after its last job.
        self.repo.wait_for_runs_finished(&self.sha, at_least, RUN_DEADLINE).unwrap_or_else(fail)
    }

    /// The run's jobs as the API has them now.
    fn jobs(&self, id: i64) -> Vec<Value> {
        let (code, body) = self.repo.api("GET", &format!("actions/runs/{id}/jobs"), None);
        assert_eq!(code, 200, "listing run {id}'s jobs: {body}");
        // A bare array on Forgejo, not Gitea's `{total_count, jobs}`.
        serde_json::from_str(&body).expect("a job list")
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

/// Bug this prevents: `fjo ci` getting the verdict, the failing job or the next step wrong on a
/// real failed run — exactly what the porcelain exists to say.
///
/// One push, one run, one job passing and one failing. Every `fjo ci` subcommand is driven over
/// it: `watch` must exit 1 once finished; `status` must exit 1, name `bad` and the run id, and end
/// with `next: fjo ci logs <branch>`; `--json` must decode and keep the exit code; `logs` must
/// print the failing job's output and *only* that. Where `gea ci rerun` would come next, the
/// advice on stderr must instead open the run's page — Forgejo can rerun only from there — by
/// the run's **id**, which is what `fjo run view` takes.
#[test]
fn a_failed_push_run_is_reported_and_logged_through_fjo_ci() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "ci watch", "ci logs"],
        hits: [
            "ListActionRuns", "ListActionRunJobs", "repoGetBranch", "repoListPullRequests",
            "repoGetActionJobLogs",
        ],
    );
    let (marker_ok, marker_bad) = (unique_marker("ci-ok"), unique_marker("ci-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-fail", "feature-fail", "[push]", &as_refs(&jobs));
    let (slug, branch) = (p.slug(), p.branch.clone());
    let runs = p.wait(1, runner);
    assert_eq!(runs.len(), 1, "one push, one workflow: {runs:?}");
    let id = id_of(&runs[0]);

    inst.fjo(["ci", "watch", &branch, "-i", "1", "-R", &slug])
        .assert_code(1, "fjo ci watch on a finished, failed run");

    for argv in [vec!["ci", branch.as_str()], vec!["ci", "status", branch.as_str()]] {
        let status = inst.fjo(argv.iter().copied().chain(["-R", &slug]));
        status.assert_code(1, "fjo ci on a failed run");
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
            out.contains(&format!("next: fjo ci logs {branch}")),
            "the next command is the failed job's logs:\n{out}"
        );
        // The rerun advice: the run's page, by id, since there is no rerun route to name.
        let err = &status.stderr;
        assert!(err.contains(&format!("fjo run view {id} -R {slug} --web")), "{err}");
        assert!(
            !err.contains("ci rerun"),
            "no advice may name a command Forgejo cannot serve:\n{err}"
        );
    }

    let json = inst.fjo(["ci", &branch, "--json", "id,status", "-R", &slug]);
    json.assert_code(1, "fjo ci --json keeps the exit code");
    let v = json.json();
    let arr = v.as_array().unwrap_or_else(|| panic!("--json prints the runs as an array: {v}"));
    assert_eq!(arr.len(), 1, "{v}");
    assert_eq!(arr[0]["id"], id, "{v}");
    assert_eq!(arr[0]["status"], "failure", "{v}");

    let logs = inst.fjo(["ci", "logs", &branch, "-R", &slug]);
    logs.assert_ok("fjo ci logs").assert_says(&marker_bad);
    assert!(
        !logs.stdout.contains(&marker_ok) && !logs.stderr.contains(&marker_ok),
        "only the failed job's log is printed, but it contains {marker_ok}:\n{}",
        logs.stdout
    );
    assert!(logs.stdout.contains(&format!("ci.yml / bad (run {id}, job ")), "{}", logs.stdout);

    // There is no `ci rerun` to call. A user with `gea` muscle memory gets a branch lookup for
    // a branch named `rerun`, which does not exist: exit 5, not a request that cannot succeed.
    inst.fjo(["ci", "rerun", "-R", &slug]).assert_code(5, "fjo ci rerun is not a subcommand");
}

/// Bug this prevents: an all-green commit reading as anything but green — a stray `next:` line, or
/// `fjo ci logs` exiting non-zero because there was nothing to print.
#[test]
fn an_all_green_run_exits_zero_with_no_next_step_and_no_logs_to_print() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "ci logs"],
        hits: ["ListActionRuns", "repoGetBranch", "repoListPullRequests"],
    );
    let marker = unique_marker("green");
    let p =
        Pushed::new(inst, "ci-green", "feature-green", "[push]", &[("ok", &echo_unseen(&marker))]);
    let (slug, branch) = (p.slug(), p.branch.clone());
    let runs = p.wait(1, runner);
    let id = id_of(&runs[0]);
    assert_eq!(runs[0]["status"], "success", "{runs:?}");

    let status = inst.fjo(["ci", &branch, "-R", &slug]);
    status.assert_ok("fjo ci on a green run");
    assert!(status.stdout.contains(&format!("run {id}")), "{}", status.stdout);
    assert!(status.stdout.contains("success"), "{}", status.stdout);
    assert!(!status.stdout.contains("next:"), "nothing to do next when green:\n{}", status.stdout);

    let logs = inst.fjo(["ci", "logs", &branch, "-R", &slug]);
    logs.assert_ok("fjo ci logs with nothing failed");
    assert!(logs.stdout.is_empty(), "no log is printed when nothing failed:\n{}", logs.stdout);
    let short = &p.sha[..7];
    assert!(
        logs.stderr.contains(&format!("no job failed for {short}; --all prints every job")),
        "the nothing-failed note is on stderr even when piped:\n{}",
        logs.stderr
    );

    // And --all does print the green job, so the empty default above means "filtered", not
    // "could not read".
    inst.fjo(["ci", "logs", &branch, "--all", "-R", &slug])
        .assert_ok("fjo ci logs --all")
        .assert_says(&marker);
}

/// Bug this prevents — the one the design exists for: CI that ran only as `pull_request` reading
/// as "CI never ran".
///
/// Forgejo files a `pull_request` run under the pull request rather than its branch (its
/// `prettyref` is `#<n>`), so `fjo run list --ref refs/heads/<branch>` cannot find it. `fjo ci`
/// finds runs by commit instead; here the same commit has a `push` run and a `pull_request` run,
/// and both `fjo ci <pr>` and `fjo ci <branch>` must show the `pull_request` one. The contrast is
/// asserted too, so this test fails if Forgejo ever starts filing those runs under their branch
/// and the premise stops holding.
#[test]
fn a_pull_request_run_with_no_branch_is_found_by_pr_number_and_by_branch() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status", "run list"],
        hits: [
            "ListActionRuns", "repoGetPullRequest", "repoListPullRequests", "ListActionRunJobs",
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
    assert_eq!(
        pr_run["prettyref"],
        format!("#{pr}"),
        "the premise: Forgejo files a pull_request run under the pull request: {pr_run}"
    );

    for target in [pr.as_str(), &format!("#{pr}"), branch.as_str()] {
        let found = inst.fjo(["ci", target, "--json", "id,event,prettyref", "-R", &slug]);
        found.assert_code(1, &format!("fjo ci {target} on a failed pull request"));
        let v = found.json();
        let listed = v.as_array().unwrap_or_else(|| panic!("an array: {v}"));
        let hit = listed
            .iter()
            .find(|r| r["event"] == "pull_request")
            .unwrap_or_else(|| panic!("fjo ci {target} lost the pull_request run: {v}"));
        assert_eq!(hit["id"], pr_run_id, "{v}");
        assert_eq!(hit["prettyref"], format!("#{pr}"), "it is the branchless run: {v}");
        assert!(listed.iter().any(|r| r["event"] == "push"), "and the push run is kept: {v}");

        let human = inst.fjo(["ci", target, "-R", &slug]);
        human.assert_code(1, &format!("fjo ci {target}"));
        assert!(human.stdout.starts_with(&format!("PR #{pr} ")), "{}", human.stdout);
        assert!(human.stdout.contains(&format!("run {pr_run_id}")), "{}", human.stdout);
        assert!(human.stdout.contains("ci.yml (pull_request)"), "{}", human.stdout);
    }

    // The contrast: the ref filter finds no pull_request run, and says where to look instead.
    let full_ref = format!("refs/heads/{branch}");
    let listed = inst.fjo([
        "run",
        "list",
        "--ref",
        &full_ref,
        "--event",
        "pull_request",
        "--json",
        "id",
        "-R",
        &slug,
    ]);
    listed.assert_ok("fjo run list --ref --event pull_request");
    assert_eq!(listed.json(), serde_json::json!([]), "the ref filter cannot see it");
    // The pointer at `fjo ci` is a warning — it changes how the empty list should be read — so a
    // pipe gets it too.
    let piped =
        inst.fjo(["run", "list", "--ref", &full_ref, "--event", "pull_request", "-R", &slug]);
    piped.assert_ok("fjo run list --ref, piped");
    assert!(piped.stdout.trim().is_empty(), "nothing listed:\n{}", piped.stdout);
    assert!(
        piped.stderr.contains(&format!("`fjo ci {branch}` finds them by commit")),
        "the empty listing points at fjo ci:\n{}",
        piped.stderr
    );
    // And a short ref, which Forgejo matches against nothing at all, is called out.
    let short = inst.fjo(["run", "list", "--ref", &branch, "-R", &slug]);
    short.assert_ok("fjo run list --ref <short>");
    assert!(
        short.stdout.trim().is_empty(),
        "measured: a short ref matches no run:\n{}",
        short.stdout
    );
    assert!(
        short.stderr.contains(&format!("did you mean `--ref refs/heads/{branch}`?")),
        "{}",
        short.stderr
    );
}

/// Bug this prevents: run and job references that parse in a unit test but address the wrong
/// thing on a real server — ids are instance-global while run numbers are per repository, so the
/// two collide in exactly the way the fallback exists for.
///
/// `#N`, the run's own web URL (which on Forgejo carries the **number**) and a bare number that
/// is not an id of this repository must all land on the same run, the last with a note saying it
/// fell back. `--job` must pick a job by exact name and refuse a substring two jobs share, naming
/// both with their ids. And the `target_url` of a commit status Actions posted — path-only, with
/// the job's **position** — must print that one job's log.
#[test]
fn run_view_and_logs_accept_numbers_urls_and_job_names() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["run view", "run logs"],
        hits: [
            "ActionRun", "ListActionRuns", "ListActionRunJobs", "repoGetActionJobLogs",
            "repoListStatusesByRef",
        ],
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
    let number = run["index_in_repo"].as_i64().expect("a run number");
    let url = run["html_url"].as_str().expect("an html_url").to_owned();
    // The premise the URL form is built on, measured: the URL carries the number, not the id.
    assert!(url.ends_with(&format!("/actions/runs/{number}")), "{url} for run {id} #{number}");

    let view =
        |what: &str| inst.fjo(["run", "view", what, "--json", "id,index_in_repo", "-R", &slug]);
    let by_number = view(&format!("#{number}"));
    assert_eq!(by_number.assert_ok("fjo run view #N").json()["id"], id, "#{number}");
    let by_url = view(&url);
    assert_eq!(by_url.assert_ok("fjo run view <url>").json()["id"], id, "{url}");

    // A bare number is an id first. Ids are instance-global and this run is one of many the suite
    // makes, so its number (1) is usually not also its id; when it is (this was the instance's
    // first run), the bare form simply resolves by id and there is no fallback to observe, so the
    // note is asserted only otherwise, and the skip is said out loud. A different run of *this*
    // repository can never hold id `number`: this repository has one run.
    let bare = view(&number.to_string());
    assert_eq!(bare.assert_ok("fjo run view <bare number>").json()["id"], id, "bare {number}");
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

    let logs = inst.fjo(["run", "logs", &id.to_string(), "--job", "bad", "-R", &slug]);
    logs.assert_ok("fjo run logs --job bad").assert_says(&mbad);
    assert!(!logs.stdout.contains(&ma) && !logs.stdout.contains(&mb), "{}", logs.stdout);

    let exact = inst.fjo(["run", "logs", &id.to_string(), "--job", "lint-b", "-R", &slug]);
    exact.assert_ok("fjo run logs --job lint-b").assert_says(&mb);
    assert!(!exact.stdout.contains(&ma), "an exact name picks one job:\n{}", exact.stdout);

    let jobs = p.jobs(id);
    let job_id = |name: &str| {
        jobs.iter()
            .find(|j| j["name"] == name)
            .and_then(|j| j["id"].as_i64())
            .unwrap_or_else(|| panic!("no job {name}: {jobs:?}"))
    };
    // A pasted job URL, as Forgejo hands them out: the `target_url` of the commit status the
    // `lint-b` job posted. Path-only, and `…/jobs/<position>` — not the job's id.
    let (code, body) = p.repo.api("GET", &format!("commits/{}/statuses", p.sha), None);
    assert_eq!(code, 200, "{body}");
    let statuses: Vec<Value> = serde_json::from_str(&body).expect("statuses");
    let job_url = statuses
        .iter()
        .find(|s| s["context"].as_str().is_some_and(|c| c.contains("lint-b")))
        .and_then(|s| s["target_url"].as_str())
        .unwrap_or_else(|| panic!("lint-b posted a status: {body}"))
        .to_owned();
    assert!(job_url.starts_with('/'), "measured: Actions statuses are path-only: {job_url}");
    let position = jobs.iter().position(|j| j["name"] == "lint-b").expect("lint-b");
    assert!(
        job_url.ends_with(&format!("/jobs/{position}")),
        "measured: a job is linked by its position ({position}), not its id ({}): {job_url}",
        job_id("lint-b")
    );
    let by_job_url = inst.fjo(["run", "logs", &job_url, "-R", &slug]);
    by_job_url.assert_ok("fjo run logs <job url>").assert_says(&mb);
    assert!(
        !by_job_url.stdout.contains(&ma) && !by_job_url.stdout.contains(&mbad),
        "a job URL prints that job only ({job_url}):\n{}",
        by_job_url.stdout
    );

    let ambiguous = inst.fjo(["run", "logs", &id.to_string(), "--job", "lint", "-R", &slug]);
    ambiguous.assert_code(2, "an ambiguous --job");
    for name in ["lint-a", "lint-b"] {
        ambiguous.assert_says(&format!("{name} (id {})", job_id(name)));
    }
    assert!(!ambiguous.stdout.contains(&ma), "nothing is printed for a refused --job");
}

/// Bug this prevents: `fjo pr checks` reporting a red pull request without saying how to see
/// why. Piped, as here, the summary and `next:` hint go to stderr so stdout stays TSV.
#[test]
fn pr_checks_on_a_failing_pull_request_exits_1_and_points_at_fjo_ci_logs() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["pr checks", "ci logs"],
        hits: [
            "repoGetPullRequest", "repoGetCombinedStatusByRef", "ListActionRuns",
            "ListActionRunJobs", "repoGetActionJobLogs",
        ],
    );
    let (marker_ok, marker_bad) = (unique_marker("chk-ok"), unique_marker("chk-bad"));
    let jobs = ok_and_bad(&marker_ok, &marker_bad);
    let p = Pushed::new(inst, "ci-checks", "feature-checks", "[pull_request]", &as_refs(&jobs));
    let slug = p.slug();
    let pr = open_pr(&p.repo, &p.branch).to_string();
    p.wait(1, runner);

    let checks = inst.fjo(["pr", "checks", &pr, "-R", &slug]);
    checks.assert_code(1, "fjo pr checks on a failing pull request");
    assert!(checks.stdout.contains("fail"), "{}", checks.stdout);
    assert!(
        !checks.stdout.contains("next:"),
        "stdout stays pure TSV when piped:\n{}",
        checks.stdout
    );
    assert!(
        checks.stderr.contains(&format!("next: fjo ci logs {pr}")),
        "the hint names the next command:\n{}",
        checks.stderr
    );

    // And the hint works as printed.
    inst.fjo(["ci", "logs", &pr, "-R", &slug])
        .assert_ok("the hinted fjo ci logs")
        .assert_says(&marker_bad);
}

/// Bug this prevents: `fjo ci` with no target resolving "here" to the wrong commit — the local
/// HEAD instead of what the server ran CI on — or doing so silently.
#[test]
fn bare_fjo_ci_in_a_checkout_shows_the_branch_and_warns_when_local_head_is_ahead() {
    let inst = instance_or_skip!();
    let runner = runner_or_skip!(inst);
    cover!(
        porcelain: ["ci status"],
        hits: ["ListActionRuns", "repoGetBranch", "repoListPullRequests"],
    );
    let p = Pushed::new(inst, "ci-here", "feature-here", "[push]", &[("ok", "true")]);
    let runs = p.wait(1, runner);
    let id = id_of(&runs[0]);

    let here = inst.fjo_in(&p.clone, ["ci"]);
    here.assert_ok("bare fjo ci in a checkout");
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
    let ahead = inst.fjo_in(&p.clone, ["ci"]);
    ahead.assert_ok("bare fjo ci with an unpushed commit");
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

/// Bug this prevents: `fjo ci watch` returning while a run is still going — the one thing
/// `fjo ci watch && fjo pr merge` relies on it never doing.
#[test]
fn ci_watch_waits_for_a_running_job_and_reports_its_transition() {
    let inst = instance_or_skip!();
    let _runner = runner_or_skip!(inst);
    cover!(porcelain: ["ci watch"], hits: ["ListActionRuns", "repoGetBranch", "repoListPullRequests"]);
    let marker = unique_marker("slow");
    let p = Pushed::new(
        inst,
        "ci-watch",
        "feature-slow",
        "[push]",
        &[("slow", &format!("sleep 5\n{}", echo_unseen(&marker)))],
    );
    let slug = p.slug();

    // Straight after the push: the run is waiting, starting, or not even created yet — never done,
    // since its only step sleeps for 5 s.
    let started = Instant::now();
    let watched = inst.fjo_env(
        std::path::Path::new("."),
        &[("FJO_FORCE_TTY", "true")],
        ["ci", "watch", &p.branch, "-i", "1", "-R", &slug],
    );
    let took = started.elapsed();
    watched.assert_ok("fjo ci watch on a run that passes");

    // The API agrees it was over by the time watch returned.
    let (code, body) = p.repo.api("GET", &format!("actions/runs?head_sha={}", p.sha), None);
    assert_eq!(code, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("runs");
    let runs = v["workflow_runs"].as_array().cloned().unwrap_or_default();
    assert_eq!(runs.len(), 1, "{v}");
    assert_eq!(runs[0]["status"], "success", "watch returned before the run finished: {v}");
    assert!(took >= Duration::from_secs(4), "a 5 s job was watched in {took:?}");

    assert!(
        watched.stderr.lines().any(|l| l.starts_with("ci.yml: ") && l.ends_with("→ success")),
        "a status transition into success was printed:\n{}",
        watched.stderr
    );
    assert!(watched.stdout.contains("success"), "{}", watched.stdout);
    assert!(!watched.stdout.contains("next:"), "{}", watched.stdout);
}

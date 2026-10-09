//! Forgejo Actions, driven against a server with **no runner attached**.
//!
//! That constraint is the whole subject of this file rather than an excuse for skipping it.
//! Without a runner nothing ever *executes*, but a great deal still happens: a dispatch is
//! accepted and queues a run, a push queues one, `cancel` and `delete` act on it, and the three
//! repository-wide lists answer with the empty-but-typed shapes the generated models decode
//! into. Those are exactly the parts a `FakeTransport` test cannot check, because a mock
//! invents the reply it is then asserted against.
//!
//! # Two instance shapes, and why every test here still has to survive both
//!
//! The harness used to boot its container with `FORGEJO__actions__ENABLED=false`, and that
//! setting is not repository-local: with it off, Forgejo puts the Actions unit in its disabled
//! set, so `PATCH /repos/{owner}/{repo}` with `has_actions: true` was accepted, returned 200, and
//! left `has_actions` **false** — measured, not assumed. Every route under
//! `/repos/{owner}/{repo}/actions/` then answered 404 for the unit rather than for the id, and
//! four of the twelve operations here were unreachable. That is fixed at the source (cbe2134):
//! the harness now enables Actions, so every test in this file runs.
//!
//! The distinction the file was built around is still worth keeping, because the suite can also
//! be pointed at somebody else's server with `FJO_TEST_HOST`, where Actions may well be off:
//!
//! * Those that hold under **either** configuration make no check at all. A missing run id is a
//!   404 whether the unit is off or the id is absent, so "this exits 5 and says 404" is a real
//!   assertion about the request path, the response classification and the error rendering in
//!   both worlds. `workflow list`/`view` are in this group for a different reason — they are
//!   built from the contents API, not from an Actions route, so they work with the unit off.
//! * Those that need the unit call [`actions_or_skip`] **before** `cover!`, so an instance
//!   without Actions records nothing. That ordering is the same contract `instance_or_skip!`
//!   has, and for the same reason: coverage that a skipped test claimed would be a lie told by
//!   the gate that exists to stop exactly this. Against the harness that guard never fires.
//!
//! # What a runner would buy
//!
//! Only the *outcome* half, and the tests below pin where the line falls. A dispatch is accepted
//! and mints a run, the run's job is created and named — and then nothing happens: the run is
//! born `waiting` and stays there, so there is no `success`/`failure` to observe,
//! `repoGetActionJobLogs` answers 404 for a job that exists but never started, `task list` stays
//! empty because a task is created when a runner *claims* a job, and no artifact is ever
//! uploaded, which is why the three artifact operations are reachable only through their
//! not-found paths.
//!
//! The harness can now attach a runner (`fjo_itest::runner`), and the outcome half lives in
//! `live_runner.rs` and `live_ci.rs`. This file stays runner-free on purpose: the `waiting`
//! lifecycle is its subject, and a runner on the shared instance would take its jobs. Its
//! workflows ask for no label the shared runner offers, so none of them is ever picked up.

use std::path::{Path, PathBuf};

use fjo_itest::{Instance, TestRepo, commit_and_push, cover, instance_or_skip};

/// `ErrorKind::ResourceNotFound` and its siblings, per `ErrorKind::exit_code` in
/// `crates/forgejo-core/src/error/mod.rs`. Named rather than inlined because the whole point of
/// the not-found tests below is that they agree with that one table.
const NOT_FOUND: i32 = 5;

/// `ErrorKind::Usage`, the code a refusal decided locally produces.
const USAGE: i32 = 2;

/// Comfortably past any id this throwaway instance will mint, so the 404 is about the id.
const ABSENT_ID: &str = "999999999";

/// Forgejo reads `.forgejo/`, `.gitea/` and `.github/`; `.github/` is the one a repository
/// mirrored from GitHub carries, and the one the brief for this file asked for.
const WORKFLOW_PATH: &str = ".github/workflows/ci.yml";

/// Only `workflow_dispatch`, with no `push` trigger, so committing the file does not itself
/// queue a run. A test that then dispatches would otherwise have two runs to tell apart, and
/// which one it got would depend on how fast Forgejo indexed the push.
const DISPATCH_ONLY: &str = "name: CI\n\
                             on:\n  workflow_dispatch:\n\
                             jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

/// A scratch directory that cleans up after itself, for the tests that need a git checkout.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("fjo-itest-actions-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Self(d)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Turn the repository's Actions unit on, and report whether it actually came on.
///
/// The read-back is the entire value of this function. `PATCH /repos/{owner}/{repo}` answers 200
/// for `has_actions: true` even when the instance has Actions disabled globally, so trusting the
/// status code would make every test below assert against a unit that is still off — the same
/// "a command exiting 0 is not evidence the server did anything" mistake, one layer down.
fn try_enable_actions(inst: &Instance, repo: &TestRepo<'_>) -> bool {
    let (code, body) =
        inst.api("PATCH", &format!("repos/{}", repo.slug()), Some(r#"{"has_actions":true}"#));
    assert!(
        (200..300).contains(&code),
        "enabling the Actions unit on {} should have been accepted: HTTP {code}: {body}",
        repo.slug()
    );
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["has_actions"].as_bool())
        .unwrap_or(false)
}

/// As [`try_enable_actions`], but announce a skip when the unit could not be turned on.
///
/// **This condition is never true against the harness**, which enables Actions instance-wide, so
/// in CI every caller runs. It is kept for the other supported way to drive this suite:
/// `FJO_TEST_HOST` + `FJO_TEST_TOKEN` pointed at a server somebody else administers, where
/// Actions may be off and nothing here can turn it on. Without the guard those runs would fail
/// nine tests and report it as a bug in `fjo`.
///
/// Deliberately **not** a panic under `FJO_ITEST_REQUIRE`: that switch means "a Forgejo must be
/// reachable", and one is. An instance configured without Actions is a supported configuration,
/// not a broken harness, and turning it into a red CI run would punish the wrong thing. The
/// caller returns before `cover!`, so the cost is recorded honestly as uncovered instead.
#[must_use]
fn actions_or_skip(inst: &Instance, repo: &TestRepo<'_>, what: &str) -> bool {
    if try_enable_actions(inst, repo) {
        return true;
    }
    let msg = format!(
        "SKIPPED ({what}): this instance has Actions disabled, so every route under \
         /repos/{}/actions/ answers 404 for the unit. Nothing was recorded as covered. \
         Point the suite at an instance with Actions enabled (FJO_TEST_HOST + FJO_TEST_TOKEN) \
         to run this.",
        repo.slug()
    );
    println!("{msg}");
    eprintln!("{msg}");
    false
}

/// Commit `body` to [`WORKFLOW_PATH`] on `main`, through a real clone and push.
///
/// Through git rather than the contents API on purpose: `workflow list` reads what is actually
/// on the default branch, and a test that seeded it through the same API family it then reads
/// back would be a shorter round trip through less of the server.
fn commit_workflow(repo: &TestRepo<'_>, tag: &str, body: &str) -> Scratch {
    let scratch = Scratch::new(tag);
    repo.clone_to(scratch.path());
    std::fs::create_dir_all(scratch.path().join(".github/workflows"))
        .expect("create .github/workflows in the clone");
    commit_and_push(scratch.path(), "main", WORKFLOW_PATH, body, "Add a workflow");
    scratch
}

// --------------------------------------------------------------------------- configuration-free

/// Bug this prevents: `workflow list` and `workflow view` regressing onto an Actions route.
///
/// Forgejo has no `GET /repos/{owner}/{repo}/actions/workflows` — it is a Gitea route — so both
/// commands are built from the contents API and parse the YAML themselves. That is invisible in
/// a unit test, which decides for itself what any call returns. It is very visible here: this
/// passes on an instance with Actions switched **off**, which nothing built on an Actions route
/// could do, and it is the reason a user of such an instance can still see their workflows.
#[test]
fn listing_workflows_reads_the_committed_file_rather_than_an_actions_route() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["workflow list", "workflow view"],
        hits: ["repoGetContentsList", "repoGetContents"],
    );
    let repo = TestRepo::create_initialized(inst, "wf-list");
    let _scratch = commit_workflow(&repo, "list", DISPATCH_ONLY);

    // Out of band: prove the file is really on the default branch before believing a command
    // that claims to have read it.
    let (code, body) = repo.api("GET", "contents/.github/workflows/ci.yml", None);
    assert_eq!(code, 200, "the workflow file should be on main: {body}");

    let listed = inst
        .fjo(["workflow", "list", "--json", "name,workflow_name,path,dispatch", "-R", &repo.slug()])
        .assert_ok("fjo workflow list")
        .json();
    let rows = listed.as_array().expect("workflow list --json is an array");
    assert_eq!(rows.len(), 1, "exactly the one committed workflow: {listed}");
    assert_eq!(rows[0]["path"], WORKFLOW_PATH, "{listed}");
    assert_eq!(rows[0]["name"], "ci.yml", "`name` is the file's name: {listed}");
    // Two different names, and the distinction is the reason `workflow_name` exists: it is the
    // `name:` key inside the YAML, which only a command that actually parsed the file can know.
    assert_eq!(rows[0]["workflow_name"], "CI", "the `name:` key is read from the YAML: {listed}");
    assert_eq!(
        rows[0]["dispatch"], true,
        "the file declares workflow_dispatch, so it must be reported as dispatchable: {listed}"
    );

    inst.fjo(["workflow", "view", "ci.yml", "-R", &repo.slug()])
        .assert_ok("fjo workflow view by bare filename")
        .assert_says(WORKFLOW_PATH);
    // The full path must resolve to the same file: `fjo workflow run` documents both spellings,
    // and a user who copied the path out of `workflow list` uses the long one.
    inst.fjo(["workflow", "view", WORKFLOW_PATH, "-R", &repo.slug()])
        .assert_ok("fjo workflow view by full path")
        .assert_says("CI");
}

/// Bug this prevents: an absent run id producing anything other than a clean not-found.
///
/// Seven leaves share one shape — take a run id, ask the server about it — and each maps a
/// different endpoint's 404 onto `ErrorKind`. A mock proves only that we classify the reply we
/// wrote; this proves the server really answers 404 for an id it has never issued, on all five
/// endpoints, and that every one of them lands on the same exit code.
///
/// `run watch` is included, and this is the **only** safe way to include it: it polls until the
/// run finishes, and with no runner attached a real run never does. Pointed at an id that does
/// not exist it fails on its first poll and returns. `.config/nextest.toml` kills an fjo-itest
/// test at 180s with a message that names nothing useful, so a watch of a live run here would
/// trade a test for a mystery.
#[test]
fn every_run_subcommand_reports_an_absent_run_as_not_found() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "run view", "run jobs", "run artifacts", "run cancel", "run delete", "run logs",
            "run watch",
        ],
        hits: [
            "ActionRun", "ListActionRunJobs", "ListActionRunArtifacts", "CancelActionRun",
            "DeleteActionRun",
        ],
    );
    let repo = TestRepo::create_initialized(inst, "run-404");
    // Best effort: where the unit can be enabled the 404 is about the id, which is the stronger
    // test. Where it cannot, it is about the unit — and the classification under test is the
    // same either way.
    let _ = try_enable_actions(inst, &repo);
    let slug = repo.slug();

    let cases: [(&str, Vec<&str>); 7] = [
        ("run view", vec!["run", "view", ABSENT_ID]),
        ("run jobs", vec!["run", "jobs", ABSENT_ID]),
        ("run artifacts", vec!["run", "artifacts", ABSENT_ID]),
        ("run cancel", vec!["run", "cancel", ABSENT_ID]),
        ("run delete", vec!["run", "delete", ABSENT_ID, "--yes"]),
        ("run logs", vec!["run", "logs", ABSENT_ID]),
        // A one-second interval so that a regression which *did* start polling shows up as a
        // fast-failing test rather than as a three-minute one.
        ("run watch", vec!["run", "watch", ABSENT_ID, "-i", "1"]),
    ];

    for (what, args) in cases {
        let run = inst.fjo(args.iter().copied().chain(["-R", &slug]));
        run.assert_code(NOT_FOUND, &format!("fjo {what} against a run that does not exist"));
        // A substring, never the server's whole sentence: the status is the contract, the
        // wording is Forgejo's to change.
        run.assert_says("404");
    }
}

/// Bug this prevents: the three artifact operations reaching the user as a panic, a hang, or a
/// success, on the only path they have without a runner.
///
/// No runner means no workflow ever uploads anything, so no artifact id is ever valid on this
/// instance — the not-found path is not a corner of these operations, it is all of them. That
/// makes this the only test that proves the generated request for each is even well formed:
/// a malformed path would answer 404 too, so the assertion is deliberately on the *request line*
/// that `fjo` reports as well as on the status.
#[test]
fn every_artifact_subcommand_reports_an_absent_artifact_as_not_found() {
    let inst = instance_or_skip!();
    cover!(
        raw: ["GetActionArtifact", "DownloadActionArtifact", "DeleteActionArtifact"],
    );
    let repo = TestRepo::create_initialized(inst, "artifact-404");
    let _ = try_enable_actions(inst, &repo);

    let base = format!("actions/artifacts/{ABSENT_ID}");
    let cases: [(&str, Vec<&str>, String); 3] = [
        ("view", vec!["raw", "artifact", "view"], base.clone()),
        ("download", vec!["raw", "artifact", "download"], format!("{base}/zip")),
        ("delete", vec!["raw", "artifact", "delete"], base.clone()),
    ];

    for (what, args, path) in cases {
        let run =
            inst.fjo(args.iter().copied().chain([&repo.owner[..], &repo.name[..], ABSENT_ID]));
        run.assert_code(NOT_FOUND, &format!("fjo raw artifact {what} for an absent artifact"));
        run.assert_says("404");
        run.assert_says(&path);
    }
}

/// Bug this prevents: `secret get` quietly asking the server for a value it can never return.
///
/// Forgejo's API exposes secret *names* and nothing else, by design. The leaf exists only so
/// that a user who types the obvious command is told why it cannot work and what to type
/// instead. If it ever grew a request, this would catch it: the refusal has to be a local
/// `Usage` failure (exit 2), not a server round trip, which is why this test passes identically
/// on an instance with Actions switched off.
#[test]
fn secret_get_refuses_locally_rather_than_asking_the_server() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["secret get"]);
    let repo = TestRepo::create_initialized(inst, "secret-get");

    let run = inst.fjo(["secret", "get", "REGISTRY_TOKEN", "-R", &repo.slug()]);
    run.assert_code(USAGE, "fjo secret get");
    run.assert_says("cannot be read back");
    // The remedy matters as much as the refusal: a user told "no" with no alternative will go
    // looking for one in the wrong place.
    run.assert_says("fjo secret list");
    run.assert_says("fjo variable");
}

/// Bug this prevents: `workflow enable`/`disable` pretending to work on Forgejo.
///
/// Both are Gitea routes (`PUT …/actions/workflows/{file}/enable`). Forgejo 16.0.4 does not
/// implement either, so the only honest outcome is a not-found that names the route it tried —
/// and the command's own help says as much. A unit test cannot tell the difference between
/// "Forgejo has no such route" and "we built the wrong URL", because it answers its own request.
#[test]
fn enabling_and_disabling_a_workflow_reports_forgejos_missing_route() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["workflow enable", "workflow disable"]);
    let repo = TestRepo::create_initialized(inst, "wf-toggle");
    let _ = try_enable_actions(inst, &repo);
    let _scratch = commit_workflow(&repo, "toggle", DISPATCH_ONLY);

    for verb in ["enable", "disable"] {
        let run = inst.fjo(["workflow", verb, "ci.yml", "-R", &repo.slug()]);
        run.assert_code(NOT_FOUND, &format!("fjo workflow {verb} on Forgejo"));
        run.assert_says("404");
        // The route is the interesting part of the message: it is what tells the reader this is
        // the server's gap rather than their typo.
        run.assert_says(&format!("/ci.yml/{verb}"));
    }
}

// ------------------------------------------------------------------------ needs the Actions unit

/// Bug this prevents: an empty Actions list decoding as `null` instead of as an empty list.
///
/// The three repository-wide lists do not share a shape — `ListActionRuns` and `ListActionTasks`
/// answer an object with a `workflow_runs` array beside a `total_count`, while
/// `ListActionArtifacts` answers a bare array — and with nothing to list that difference is
/// exactly where a generated model goes wrong. A `null` where a list belongs is the failure that
/// would reach a user as `fjo run list` printing `null` into a script's `jq`, and no mock can
/// find it, because a mock is handed the empty body it is then asserted against.
///
/// `task list` stays empty even after a run exists — measured — because a task is created when a
/// runner claims a job. That is the cleanest single illustration of what a runner would buy.
#[test]
fn the_repository_wide_actions_lists_are_empty_but_well_formed() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "actions-empty");
    if !actions_or_skip(inst, &repo, "empty Actions lists") {
        return;
    }
    cover!(
        porcelain: ["run list", "run artifacts"],
        hits: ["ListActionRuns", "ListActionArtifacts", "ListActionTasks"],
    );
    let (owner, name) = (repo.owner.clone(), repo.name.clone());

    let runs = inst.fjo(["raw", "run", "list", &owner, &name]).assert_ok("fjo raw run list").json();
    assert!(
        runs["workflow_runs"].is_array(),
        "ListActionRuns must answer an array, not null: {runs}"
    );
    assert_eq!(runs["workflow_runs"].as_array().map(Vec::len), Some(0), "{runs}");
    assert_eq!(runs["total_count"], 0, "{runs}");

    let tasks =
        inst.fjo(["raw", "task", "list", &owner, &name]).assert_ok("fjo raw task list").json();
    assert!(
        tasks["workflow_runs"].is_array(),
        "ListActionTasks must answer an array, not null: {tasks}"
    );
    assert_eq!(tasks["total_count"], 0, "{tasks}");

    // Deliberately a different assertion: this operation's 200 really is a bare array in the
    // specification, so asserting the object shape above would be asserting a bug.
    let artifacts = inst
        .fjo(["raw", "artifact", "list", &owner, &name])
        .assert_ok("fjo raw artifact list")
        .json();
    assert!(artifacts.is_array(), "ListActionArtifacts must answer an array: {artifacts}");
    assert_eq!(artifacts.as_array().map(Vec::len), Some(0), "{artifacts}");

    // And the porcelain over them, which is what a script actually sees.
    for (what, args) in
        [("run list", vec!["run", "list"]), ("run artifacts", vec!["run", "artifacts"])]
    {
        let out = inst
            .fjo(args.iter().copied().chain(["--json", "id", "-R", &repo.slug()]))
            .assert_ok(&format!("fjo {what}"))
            .json();
        assert_eq!(out, serde_json::json!([]), "fjo {what} --json must print [], not null");
    }
}

/// Bug this prevents: believing a dispatch failed because nothing ran.
///
/// This is the one test that proves what "no runner" actually means. `DispatchWorkflow` succeeds
/// with no runner: the server accepts it, mints a run, and answers 201 with the run's id, number
/// and job names. The run is then born `waiting` and stays there forever, which is what makes
/// the rest of the lifecycle drivable — `run view` and `run jobs` describe a run that will never
/// start, `run cancel` moves it to `cancelled`, and only then will `run delete` take it.
///
/// Two things here are only observable against a real server. The dispatch reply is a distinct
/// type (`DispatchWorkflowRun`, not `ActionRun`), so a mock asserting the shape of its own fixture
/// proves nothing about which one Forgejo sends. And **deleting a run that has not finished is
/// refused** — today with an HTTP 500 rather than the 400 the specification declares for this
/// operation, which is why the assertion below is "did not succeed" and not an exit code: the
/// status is Forgejo's bug to fix, and pinning it would turn that fix into a failure here.
#[test]
fn dispatching_a_workflow_queues_a_run_that_no_runner_will_ever_start() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "wf-dispatch");
    if !actions_or_skip(inst, &repo, "workflow dispatch and the run lifecycle") {
        return;
    }
    cover!(
        porcelain: [
            "workflow run", "run view", "run jobs", "run cancel", "run delete", "run logs",
        ],
        hits: [
            "DispatchWorkflow", "ActionRun", "ListActionRunJobs", "ListActionRunArtifacts",
            "CancelActionRun", "DeleteActionRun", "ListActionRuns", "repoGetActionJobLogs",
        ],
    );
    let scratch = commit_workflow(&repo, "dispatch", DISPATCH_ONLY);
    let slug = repo.slug();

    // Run from inside the clone rather than from the test process's own directory. `--ref`
    // defaults to *the checked-out branch*, so `inst.fjo` — whose cwd is this repository —
    // dispatched against `test/exhaustive-command-coverage` and got a 500 from a server that
    // has no such ref. That is the command working as documented, and it is a trap worth
    // leaving a marker on: driving it from a checkout of the repository under test is both the
    // realistic invocation and the deterministic one.
    let started = inst
        .fjo_in(
            scratch.path(),
            ["workflow", "run", "ci.yml", "--json", "id,run_number,jobs", "-R", &slug],
        )
        .assert_ok("fjo workflow run with no runner attached")
        .json();
    let id = started["id"].as_i64().expect("the dispatch reply carries a run id");
    assert_eq!(started["run_number"], 1, "the first run of this repository: {started}");
    assert_eq!(
        started["jobs"],
        serde_json::json!(["build"]),
        "the reply names the jobs it queued: {started}"
    );

    // Out of band: a 201 is not evidence that a run exists.
    let (code, body) = repo.api("GET", &format!("actions/runs/{id}"), None);
    assert_eq!(code, 200, "the dispatched run should be readable: {body}");
    let server: serde_json::Value = serde_json::from_str(&body).expect("a run");
    assert_eq!(
        server["status"], "waiting",
        "with no runner attached a dispatched run must stay waiting: {body}"
    );
    assert_eq!(
        server["prettyref"], "main",
        "the ref came from the clone's checked-out branch, not from a default guessed here: {body}"
    );

    let id = id.to_string();
    inst.fjo(["run", "view", &id, "-R", &slug])
        .assert_ok("fjo run view of a waiting run")
        .assert_says("waiting");

    let jobs = inst
        .fjo(["run", "jobs", &id, "--json", "name,status", "-R", &slug])
        .assert_ok("fjo run jobs")
        .json();
    assert_eq!(
        jobs,
        serde_json::json!([{"name": "build", "status": "waiting"}]),
        "the job exists and is queued, it just has nobody to run it"
    );

    let artifacts = inst
        .fjo(["run", "artifacts", &id, "--json", "id", "-R", &slug])
        .assert_ok("fjo run artifacts of a run that never started")
        .json();
    assert_eq!(artifacts, serde_json::json!([]), "a run that never started uploaded nothing");

    // The sharpest single measurement of the runner gap, and only reachable now that a real run
    // exists. `run logs` resolves the run's jobs first — that call succeeds and yields job 1,
    // asserted just above — and then asks `repoGetActionJobLogs` for that job's output, which
    // 404s. So the failure is not "no such job": the job exists and the message names it. It is
    // that a job nobody ever started has no logs to fetch. A mock cannot produce this shape,
    // because it would have to decide for itself that the second call fails after the first
    // succeeded.
    let logs = inst.fjo(["run", "logs", &id, "-R", &slug]);
    logs.assert_code(NOT_FOUND, "fjo run logs for a job that never started");
    logs.assert_says("404");
    logs.assert_says("/actions/jobs/");

    // A run that has not finished cannot be deleted. See the doc comment for why this asserts
    // failure rather than a particular code.
    let premature = inst.fjo(["run", "delete", &id, "--yes", "-R", &slug]);
    assert!(
        !premature.ok(),
        "deleting a run that is still waiting must not succeed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        premature.stdout,
        premature.stderr
    );

    inst.fjo(["run", "cancel", &id, "-R", &slug]).assert_ok("fjo run cancel of a waiting run");
    let (_, body) = repo.api("GET", &format!("actions/runs/{id}"), None);
    let server: serde_json::Value = serde_json::from_str(&body).expect("a run");
    assert_eq!(server["status"], "cancelled", "cancel must actually change the run: {body}");

    inst.fjo(["run", "delete", &id, "--yes", "-R", &slug])
        .assert_ok("fjo run delete of a cancelled run");
    let (code, body) = repo.api("GET", &format!("actions/runs/{id}"), None);
    assert_eq!(code, 404, "the run should be gone after delete: {body}");

    // And the list agrees, which is the check that the delete was not merely a soft flag.
    let listed = inst
        .fjo(["run", "list", "--json", "id", "-R", &slug])
        .assert_ok("fjo run list after the only run was deleted")
        .json();
    assert_eq!(listed, serde_json::json!([]), "{listed}");
}

/// Bug this prevents: a secret appearing to be readable.
///
/// The API returns names and never values, so the whole lifecycle has to be verified by
/// *absence*: the value must not come back from `secret list`, and it must not come back from
/// the raw endpoint either, checked out of band. A test that seemed to read a secret back would
/// be reporting a security bug as a pass.
///
/// The set is done twice on purpose. `PUT …/actions/secrets/{name}` is an upsert, and its two
/// outcomes have different status codes (201 creating, 204 replacing) — a client that only
/// tolerated one of them would work until the second time anyone rotated a secret, and only a
/// real server ever sends the second.
#[test]
fn a_repository_secret_can_be_set_and_named_but_never_read_back() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "secret-life");
    if !actions_or_skip(inst, &repo, "the repository secret lifecycle") {
        return;
    }
    cover!(
        porcelain: ["secret set", "secret list", "secret delete"],
        hits: ["updateRepoSecret", "repoListActionsSecrets", "deleteRepoSecret"],
    );
    let slug = repo.slug();

    inst.fjo(["secret", "set", "REGISTRY_TOKEN", "-b", "first-value", "-R", &slug])
        .assert_ok("fjo secret set (create)");
    inst.fjo(["secret", "set", "REGISTRY_TOKEN", "-b", "second-value", "-R", &slug])
        .assert_ok("fjo secret set (replace) — the upsert's other status code");

    let listed = inst
        .fjo(["secret", "list", "--json", "name", "-R", &slug])
        .assert_ok("fjo secret list")
        .json();
    assert_eq!(listed, serde_json::json!([{"name": "REGISTRY_TOKEN"}]), "{listed}");

    // Out of band, and the most important assertion in the file: the value must not be anywhere
    // in what the server will hand back.
    let (code, body) = repo.api("GET", "actions/secrets", None);
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("REGISTRY_TOKEN"), "the secret should be listed by name: {body}");
    for value in ["first-value", "second-value"] {
        assert!(!body.contains(value), "a secret's value must never be returned: {body}");
    }

    // A destructive command must not act without confirmation, and must not have half-acted.
    let unconfirmed = inst.fjo(["secret", "delete", "REGISTRY_TOKEN", "-R", &slug]);
    unconfirmed.assert_code(USAGE, "fjo secret delete without --yes");
    let (_, body) = repo.api("GET", "actions/secrets", None);
    assert!(body.contains("REGISTRY_TOKEN"), "an unconfirmed delete must delete nothing: {body}");

    inst.fjo(["secret", "delete", "REGISTRY_TOKEN", "--yes", "-R", &slug])
        .assert_ok("fjo secret delete --yes");
    let (code, body) = repo.api("GET", "actions/secrets", None);
    assert_eq!(code, 200, "{body}");
    assert!(!body.contains("REGISTRY_TOKEN"), "the secret should be gone: {body}");
}

/// Bug this prevents: a variable's value being reported from the request rather than the server.
///
/// Variables are the readable half of the pair, so unlike a secret this can be checked by
/// presence — and the check has teeth, because **Forgejo upper-cases the name**. `fjo variable
/// set my_var` creates `MY_VAR`, and `fjo variable get my_var` has to find it again. Nothing
/// about that is visible in the specification or reproducible in a mock: it is the server's
/// normalisation, and a client that echoed back what it sent would look correct right up until
/// someone listed their variables.
#[test]
fn a_repository_variable_round_trips_through_set_get_list_and_delete() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "variable-life");
    if !actions_or_skip(inst, &repo, "the repository variable lifecycle") {
        return;
    }
    cover!(
        porcelain: ["variable set", "variable get", "variable list", "variable delete"],
        hits: [
            "createRepoVariable", "updateRepoVariable", "getRepoVariable",
            "getRepoVariablesList", "deleteRepoVariable",
        ],
    );
    let slug = repo.slug();

    inst.fjo(["variable", "set", "build_flavour", "release", "-R", &slug])
        .assert_ok("fjo variable set (create)");

    // Out of band, and asserting the normalisation rather than what we sent.
    let (code, body) = repo.api("GET", "actions/variables/BUILD_FLAVOUR", None);
    assert_eq!(code, 200, "the server stores the name upper-cased: {body}");
    let stored: serde_json::Value = serde_json::from_str(&body).expect("a variable");
    assert_eq!(stored["data"], "release", "{body}");

    inst.fjo(["variable", "get", "build_flavour", "-R", &slug])
        .assert_ok("fjo variable get with the name as it was typed")
        .assert_says("release");

    // Setting an existing variable is a different operation from creating one
    // (`PUT …/variables/{name}` rather than `POST`), and the command has to pick the right one
    // without being told which case it is in.
    inst.fjo(["variable", "set", "build_flavour", "debug", "-R", &slug])
        .assert_ok("fjo variable set (update)");
    let (_, body) = repo.api("GET", "actions/variables/BUILD_FLAVOUR", None);
    let stored: serde_json::Value = serde_json::from_str(&body).expect("a variable");
    assert_eq!(stored["data"], "debug", "the update must reach the server: {body}");

    let listed = inst
        .fjo(["variable", "list", "--json", "name,data", "-R", &slug])
        .assert_ok("fjo variable list")
        .json();
    assert_eq!(listed, serde_json::json!([{"name": "BUILD_FLAVOUR", "data": "debug"}]), "{listed}");

    let unconfirmed = inst.fjo(["variable", "delete", "build_flavour", "-R", &slug]);
    unconfirmed.assert_code(USAGE, "fjo variable delete without --yes");

    inst.fjo(["variable", "delete", "build_flavour", "--yes", "-R", &slug])
        .assert_ok("fjo variable delete --yes");
    let (code, body) = repo.api("GET", "actions/variables/BUILD_FLAVOUR", None);
    assert_eq!(code, 404, "the variable should be gone: {body}");
}

/// Bug this prevents: a `run list` filter that is accepted, ignored, and reported as a match.
///
/// All five filters become query parameters that only the server can apply, and a client that
/// dropped one — a renamed parameter after a spec bump, a flag wired to nothing — would still
/// exit 0 and print rows. It would just print the wrong ones, which is the failure mode nobody
/// notices until a script acts on the result. A unit test cannot catch it: the mock returns the
/// same fixture whatever the query string says.
///
/// `--event` carries the sharpest version of that. A dispatched run comes back from this server
/// with its `event` field **empty**, yet `--event workflow_dispatch` selects it and
/// `--event push` does not — so the filter is genuinely the server's, and any client-side
/// reimplementation of it over the field would return nothing at all. This test is the only
/// place that difference is observable.
#[test]
fn run_list_filters_are_applied_by_the_server_rather_than_by_the_client() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "run-filter");
    if !actions_or_skip(inst, &repo, "run list filters") {
        return;
    }
    cover!(porcelain: ["run list"], hits: ["DispatchWorkflow", "ListActionRuns"]);
    let scratch = commit_workflow(&repo, "filter", DISPATCH_ONLY);
    let slug = repo.slug();

    let started = inst
        .fjo_in(scratch.path(), ["workflow", "run", "ci.yml", "--json", "id", "-R", &slug])
        .assert_ok("fjo workflow run")
        .json();
    let id = started["id"].as_i64().expect("a run id");

    // Each pair is (a filter that must select the run, one that must exclude it). Asserting both
    // halves is the point: a filter dropped on the floor passes the first and fails the second.
    let cases: [(&str, &str, &str); 4] = [
        ("--status", "waiting", "success"),
        ("--workflow", "ci.yml", "not-a-workflow.yml"),
        ("--ref", "refs/heads/main", "refs/heads/no-such-branch"),
        ("--event", "workflow_dispatch", "push"),
    ];

    for (flag, matching, excluding) in cases {
        let hit = inst
            .fjo(["run", "list", flag, matching, "--json", "id", "-R", &slug])
            .assert_ok(&format!("fjo run list {flag} {matching}"))
            .json();
        assert_eq!(
            hit,
            serde_json::json!([{ "id": id }]),
            "{flag} {matching} should have selected the run"
        );

        let miss = inst
            .fjo(["run", "list", flag, excluding, "--json", "id", "-R", &slug])
            .assert_ok(&format!("fjo run list {flag} {excluding}"))
            .json();
        assert_eq!(
            miss,
            serde_json::json!([]),
            "{flag} {excluding} matched a run it should have excluded — the filter is not \
             reaching the server"
        );
    }

    // `--commit` takes the sha the run actually ran on, which has to be read back rather than
    // guessed: it is the tip of `main` after the workflow was pushed.
    let (_, body) = repo.api("GET", &format!("actions/runs/{id}"), None);
    let run: serde_json::Value = serde_json::from_str(&body).expect("a run");
    let sha = run["commit_sha"].as_str().expect("the run names its commit").to_owned();
    let hit = inst
        .fjo(["run", "list", "--commit", &sha, "--json", "id", "-R", &slug])
        .assert_ok("fjo run list --commit")
        .json();
    assert_eq!(hit, serde_json::json!([{ "id": id }]), "--commit did not select its own run");

    // Leave nothing behind: cancel first, because an unfinished run cannot be deleted.
    inst.fjo(["run", "cancel", &id.to_string(), "-R", &slug]).assert_ok("fjo run cancel");
    inst.fjo(["run", "delete", &id.to_string(), "--yes", "-R", &slug]).assert_ok("fjo run delete");
}

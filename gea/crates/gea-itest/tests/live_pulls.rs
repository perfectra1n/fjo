//! Pull requests, against a real Gitea.
//!
//! The pull request group is the one where a `FakeTransport` test proves the least. Almost
//! everything here is a *relationship between two server-side facts* that a mock decides for
//! itself: a review is refused unless somebody other than the author files it, a review request is
//! refused unless the named account can already see the repository, `refs/pull/<n>/head` exists
//! only because the server publishes it, a scheduled auto-merge is invisible except in what a
//! second cancel answers, and "this repository runs no CI" arrives as a shape
//! (`{"state":"","statuses":null}`) that nobody would think to write into a fixture.
//!
//! Deliberately not repeated here: `porcelain.rs` already covers `pr create --fill`,
//! `pr merge --squash -d`, and the machine-output path of `pr view`/`pr status`; `agit.rs` covers
//! the AGit flow end to end. This file picks up where those stop — reviews, review comments,
//! requested reviewers, files and diffs, the edit/close/reopen lifecycle, branch updates, and the
//! merge pre-checks.

use std::path::{Path, PathBuf};

use gea_itest::{Instance, ScopedUser, TestRepo, commit_and_push, cover, git, instance_or_skip};

// ---------------------------------------------------------------------------------- the fixtures

/// A scratch directory that cleans up after itself, for the tests that need a git checkout.
///
/// Copied rather than shared with `porcelain.rs`: the isolation rule for this suite is one file per
/// group, and a four-line helper is cheaper than a shared module every group has to agree on.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-pulls-{}-{tag}", std::process::id()));
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

/// Commit to a branch that already exists, without moving it.
///
/// [`commit_and_push`] uses `git checkout -B`, which *resets* the named branch to wherever HEAD
/// happens to be — right for creating a topic branch off `main`, catastrophic for adding a commit
/// to `main` while standing on a topic branch, which is exactly what the update-branch test needs.
fn commit_on(dir: &Path, branch: &str, file: &str, contents: &str, message: &str) {
    git(dir, &["checkout", "--quiet", branch]);
    std::fs::write(dir.join(file), contents).expect("write a file in the clone");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", message]);
    git(dir, &["push", "--quiet", "origin", branch]);
}

/// Clone `repo`, put `file` on a new `branch` off `main`, push it, and open a pull request.
///
/// The pull request is opened over the API rather than with `gea pr create`, on purpose: a test of
/// reviews must fail when reviews break, not when creation does. The one test that *is* about
/// creation drives the command instead.
fn open_pull_request(
    repo: &TestRepo<'_>,
    dir: &Path,
    branch: &str,
    file: &str,
    title: &str,
) -> i64 {
    repo.clone_to(dir);
    commit_and_push(dir, branch, file, "first line\n", &format!("add {file}"));
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(&format!(r#"{{"title":"{title}","head":"{branch}","base":"main","body":"seed"}}"#)),
    );
    assert_eq!(code, 201, "could not open a pull request from {branch}: {body}");
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    pr["number"].as_i64().expect("a pull request number")
}

/// A second account with write access to `repo`.
///
/// Both halves are needed and neither is optional. Gitea refuses a review of your own pull
/// request with a 422, so there has to be somebody else; and it refuses a review *request* naming
/// an account that cannot see the repository, which every test repository here is (private), so
/// that somebody has to be a collaborator. A mock has no opinion about either.
fn collaborator(inst: &Instance, repo: &TestRepo<'_>, prefix: &str) -> ScopedUser {
    let user = inst
        .scoped_user(prefix, &["write:repository", "read:user"])
        .unwrap_or_else(|e| panic!("could not mint a second account for {prefix}: {e}"));
    let (code, body) =
        repo.api("PUT", &format!("collaborators/{}", user.name), Some(r#"{"permission":"write"}"#));
    assert!(
        (200..300).contains(&code),
        "could not add {} as a collaborator: HTTP {code}: {body}",
        user.name
    );
    user
}

/// Poll `f` until it answers `Some`, for at most twenty seconds.
///
/// Not every write Gitea acknowledges is visible in every projection of it immediately: a pull
/// request's `head.sha` moves synchronously while the commit list served for it is recomputed by a
/// background job. Polling is the honest way to wait for that — a fixed sleep is flaky on a loaded
/// runner and wasted on an idle one, and dropping the assertion would leave the operation untested.
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "waited twenty seconds for {what} and it never happened"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The pull request, straight from the server.
fn pull(repo: &TestRepo<'_>, index: i64) -> serde_json::Value {
    let (code, body) = repo.api("GET", &format!("pulls/{index}"), None);
    assert_eq!(code, 200, "reading #{index} back: {body}");
    serde_json::from_str(&body).expect("a pull request")
}

/// Every review on the pull request, as `(id, state, dismissed)`.
fn reviews(repo: &TestRepo<'_>, index: i64) -> Vec<(i64, String, bool)> {
    let (code, body) = repo.api("GET", &format!("pulls/{index}/reviews"), None);
    assert_eq!(code, 200, "listing reviews on #{index}: {body}");
    let list: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array of reviews");
    list.iter()
        .map(|r| {
            (
                r["id"].as_i64().unwrap_or_default(),
                r["state"].as_str().unwrap_or_default().to_owned(),
                r["dismissed"].as_bool().unwrap_or_default(),
            )
        })
        .collect()
}

/// The logins currently asked for a review, sorted so an assertion does not depend on order.
fn requested_reviewers(repo: &TestRepo<'_>, index: i64) -> Vec<String> {
    let pr = pull(repo, index);
    let mut names: Vec<String> = pr["requested_reviewers"]
        .as_array()
        .map(|a| a.iter().filter_map(|u| u["login"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

// ------------------------------------------------------------------------------ create and amend

/// `pr create -l/-r` is three calls that a mock would let disagree: create, resolve the label names
/// to ids, and then a *separate* `POST …/requested_reviewers`, because `CreatePullRequestOption`
/// has no reviewers field at all.
///
/// The reviewer half is the one only a server can judge. Gitea answers 422 when the account named
/// cannot see the repository, so a fixture that returns 201 for any login would hide the single
/// most likely way for this to be wrong in the field.
///
/// `--draft` is checked for the same reason: Gitea has no draft column, so the flag is
/// implemented as a `WIP:` title prefix and `draft` comes back *derived by the server* from the
/// title it stored. Asserting both together is what proves the prefix we chose is one the server
/// actually recognises.
#[test]
fn creating_a_pull_request_attaches_the_label_and_the_reviewer_it_was_given() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr create"], hits: ["repoCreatePullRequest", "repoCreatePullReviewRequests"]);
    let repo = TestRepo::create_initialized(inst, "pr-create");
    let scratch = Scratch::new("create");
    let reviewer = collaborator(inst, &repo, "prcreatrev");

    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "f.txt", "hello\n", "add f.txt");

    let (code, body) =
        repo.api("POST", "labels", Some(r#"{"name":"needs-eyes","color":"00ff00"}"#));
    assert_eq!(code, 201, "creating the label: {body}");

    inst.gea_in(
        scratch.path(),
        [
            "pr",
            "create",
            "--title",
            "a reviewed change",
            "-b",
            "please look",
            "--draft",
            "-l",
            "needs-eyes",
            "-r",
            &reviewer.name,
            "-H",
            "feature",
            "-B",
            "main",
            "-R",
            &repo.slug(),
        ],
    )
    .assert_ok("gea pr create with a label, a reviewer and --draft");

    // Out of band: the command printing a URL is not evidence that anything was attached.
    let pr = pull(&repo, 1);
    assert_eq!(
        pr["title"], "WIP: a reviewed change",
        "--draft is a title prefix and nothing else: {pr}"
    );
    assert_eq!(pr["draft"], true, "the server must derive draft from the prefix we wrote: {pr}");
    let labels: Vec<&str> = pr["labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| l["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(labels, ["needs-eyes"], "the label name was not resolved to an id: {pr}");
    assert_eq!(
        requested_reviewers(&repo, 1),
        std::slice::from_ref(&reviewer.name),
        "the second call, POST …/requested_reviewers, did not happen or was refused: {pr}"
    );
}

/// The whole write side of a pull request's life, each step read back from the server.
///
/// Two things here are only observable against a real instance. `gea pr edit` builds its patch body
/// by hand because [`gitea_model::EditPullRequestOption`] cannot express an absent field — its
/// `labels: Vec<i64>` serialises to `[]`, which Gitea reads as *remove every label* — so an edit
/// of the title alone must leave the labels alone, and only the server can say whether it did.
/// And `pr close -c` posts the comment *before* the close, so that a failed close never loses the
/// explanation; the comment must therefore exist even though the two are separate calls.
#[test]
fn editing_closing_and_reopening_a_pull_request_leaves_everything_else_alone() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoCreatePullRequest", "repoGetPullRequest"]);
    cover!(
        porcelain: ["pr edit", "pr close", "pr reopen", "pr comment", "pr view"],
        hits: ["repoEditPullRequest", "repoGetPullRequest", "issueCreateComment", "issueGetComments"]
    );
    let repo = TestRepo::create_initialized(inst, "pr-cycle");
    let scratch = Scratch::new("cycle");
    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "f.txt", "hello\n", "add f.txt");

    let (code, body) = repo.api("POST", "labels", Some(r#"{"name":"keep-me","color":"0000ff"}"#));
    assert_eq!(code, 201, "creating the label: {body}");

    // Created through the generated operation, so the raw layer is exercised on the same object
    // the porcelain then edits.
    let created = inst.gea([
        "raw",
        "repo",
        "create-pull-request",
        "-R",
        &repo.slug(),
        "--title",
        "original title",
        "--head",
        "feature",
        "--base",
        "main",
        "--body",
        "original body",
    ]);
    created.assert_ok("gea raw repo create-pull-request");
    let index = created.json()["number"].as_i64().expect("a pull request number");

    inst.gea(["pr", "edit", &index.to_string(), "--add-label", "keep-me", "-R", &repo.slug()])
        .assert_ok("gea pr edit --add-label");
    inst.gea(["pr", "edit", &index.to_string(), "--title", "a better title", "-R", &repo.slug()])
        .assert_ok("gea pr edit --title");

    let pr = pull(&repo, index);
    assert_eq!(pr["title"], "a better title", "the title edit did not land: {pr}");
    let labels: Vec<&str> = pr["labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| l["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        labels,
        ["keep-me"],
        "editing only the title must not send an empty label list, which Gitea reads as \
         'remove every label': {pr}"
    );
    assert_eq!(
        pr["body"], "original body",
        "the body was not touched and must have survived: {pr}"
    );

    inst.gea([
        "pr",
        "comment",
        &index.to_string(),
        "-b",
        "a standalone remark",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea pr comment");

    inst.gea(["pr", "close", &index.to_string(), "-c", "superseded, closing", "-R", &repo.slug()])
        .assert_ok("gea pr close -c");

    let closed = pull(&repo, index);
    assert_eq!(closed["state"], "closed", "the close did not land: {closed}");
    assert_eq!(closed["merged"], false, "closing must not merge: {closed}");

    let (code, body) = repo.api("GET", &format!("issues/{index}/comments"), None);
    assert_eq!(code, 200, "{body}");
    let comments: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    let bodies: Vec<&str> = comments.iter().filter_map(|c| c["body"].as_str()).collect();
    assert!(
        bodies.contains(&"a standalone remark"),
        "gea pr comment did not reach the server: {bodies:?}"
    );
    assert!(
        bodies.contains(&"superseded, closing"),
        "the explanation is posted before the close so a failed close cannot lose it, so it must \
         be there once the close succeeded: {bodies:?}"
    );

    // The human path, which `porcelain.rs` deliberately does not exercise (it asserts the machine
    // one). `-c` is the display flag that decides whether the comments are fetched at all.
    let viewed = inst.gea(["pr", "view", &index.to_string(), "-c", "-R", &repo.slug()]);
    viewed.assert_ok("gea pr view -c");
    viewed.assert_says("a better title");
    viewed.assert_says("a standalone remark");

    inst.gea(["pr", "reopen", &index.to_string(), "-R", &repo.slug()]).assert_ok("gea pr reopen");
    assert_eq!(pull(&repo, index)["state"], "open", "the reopen did not land");
}

/// `gea pr ready` is a title edit and nothing else, because Gitea has no draft field and no
/// `ready` endpoint — a pull request is a draft when its title carries a work-in-progress prefix.
///
/// That makes the round trip the only real test: `--undo` writes `WIP:` and the server must answer
/// `draft: true` for it, and stripping it must flip the flag back. A mock returning a hand-written
/// `draft` would pass whichever prefix we invented, including one this instance does not recognise.
#[test]
fn the_work_in_progress_prefix_is_the_only_thing_that_makes_a_pull_request_a_draft() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr ready"], hits: ["repoEditPullRequest", "repoGetPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "pr-ready");
    let scratch = Scratch::new("ready");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "ready or not");

    assert_eq!(pull(&repo, index)["draft"], false, "a plain title is not a draft");

    inst.gea(["pr", "ready", &index.to_string(), "--undo", "-R", &repo.slug()])
        .assert_ok("gea pr ready --undo");
    let drafted = pull(&repo, index);
    assert_eq!(drafted["title"], "WIP: ready or not", "--undo must write the prefix: {drafted}");
    assert_eq!(
        drafted["draft"], true,
        "the server must recognise the prefix this build writes, or `pr ready` is fiction: \
         {drafted}"
    );

    inst.gea(["pr", "ready", &index.to_string(), "-R", &repo.slug()]).assert_ok("gea pr ready");
    let ready = pull(&repo, index);
    assert_eq!(ready["title"], "ready or not", "the prefix was not stripped: {ready}");
    assert_eq!(ready["draft"], false, "stripping the prefix must clear the flag: {ready}");

    // Idempotent: a second `ready` has nothing to do and must not fail or mangle the title.
    inst.gea(["pr", "ready", &index.to_string(), "-R", &repo.slug()])
        .assert_ok("gea pr ready on a pull request that is already ready");
    assert_eq!(pull(&repo, index)["title"], "ready or not", "a no-op edit changed the title");
}

// --------------------------------------------------------------------------------- list and merge

/// `-s merged` and `-s closed` are filtered **client-side**, because Gitea's `state` is only
/// `open`/`closed`/`all`: a merged pull request is a closed one with `merged: true`. Passing
/// `merged` through to the API would return an empty list with no error at all.
///
/// So this asserts both halves against one repository holding one of each: the raw operation
/// showing that the server's `closed` really does include the merged one, and the porcelain
/// showing that it splits them. A mock cannot produce that overlap because the overlap is the
/// server's opinion.
#[test]
fn listing_pull_requests_separates_merged_from_merely_closed() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoListPullRequests", "repoMergePullRequest", "repoPullRequestIsMerged"]);
    cover!(porcelain: ["pr list"], hits: ["repoListPullRequests"]);
    let repo = TestRepo::create_initialized(inst, "pr-list");
    let scratch = Scratch::new("list");

    let merged = open_pull_request(&repo, scratch.path(), "to-merge", "m.txt", "gets merged");
    git(scratch.path(), &["checkout", "--quiet", "main"]);
    commit_and_push(scratch.path(), "to-close", "c.txt", "closed\n", "add c.txt");
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(r#"{"title":"gets closed","head":"to-close","base":"main","body":"seed"}"#),
    );
    assert_eq!(code, 201, "{body}");
    let closed: i64 =
        serde_json::from_str::<serde_json::Value>(&body).expect("a pull request")["number"]
            .as_i64()
            .expect("a number");

    // Before the merge the merge probe is a 404, which the taxonomy maps to exit 5. That is the
    // control for the assertion after it: exit 0 afterwards would otherwise prove nothing.
    inst.gea([
        "raw",
        "repo",
        "pull-request-is-merged",
        "-R",
        &repo.slug(),
        "--index",
        &merged.to_string(),
    ])
    .assert_code(5, "gea raw repo pull-request-is-merged on an open pull request");

    inst.gea([
        "raw",
        "repo",
        "merge-pull-request",
        "-R",
        &repo.slug(),
        "--index",
        &merged.to_string(),
        "--do",
        "squash",
    ])
    .assert_ok("gea raw repo merge-pull-request --do squash");

    inst.gea([
        "raw",
        "repo",
        "pull-request-is-merged",
        "-R",
        &repo.slug(),
        "--index",
        &merged.to_string(),
    ])
    .assert_ok("gea raw repo pull-request-is-merged after merging");

    let (_, body) = repo.api("PATCH", &format!("pulls/{closed}"), Some(r#"{"state":"closed"}"#));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).expect("a pull request")["state"],
        "closed",
        "{body}"
    );

    // The server's own view: `closed` covers both, which is precisely why `-s merged` cannot be
    // passed through.
    let raw_closed =
        inst.gea(["raw", "repo", "list-pull-requests", "-R", &repo.slug(), "--state", "closed"]);
    raw_closed.assert_ok("gea raw repo list-pull-requests --state closed");
    let mut server_side: Vec<i64> = raw_closed
        .json()
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|p| p["number"].as_i64())
        .collect();
    server_side.sort_unstable();
    let mut both = [merged, closed];
    both.sort_unstable();
    assert_eq!(
        server_side, both,
        "the API's `closed` must include the merged pull request; if it ever stops doing so, \
         `pr list -s merged` silently returns nothing"
    );

    let merged_rows =
        inst.gea(["pr", "list", "-s", "merged", "-R", &repo.slug(), "--json", "number,title"]);
    merged_rows.assert_ok("gea pr list -s merged");
    assert_eq!(
        numbers(&merged_rows.json()),
        [merged],
        "-s merged must keep only the merged one: {}",
        merged_rows.stdout
    );

    let closed_rows =
        inst.gea(["pr", "list", "-s", "closed", "-R", &repo.slug(), "--json", "number,title"]);
    closed_rows.assert_ok("gea pr list -s closed");
    assert_eq!(
        numbers(&closed_rows.json()),
        [closed],
        "-s closed must drop the merged one: {}",
        closed_rows.stdout
    );

    let all_rows = inst.gea(["pr", "list", "-s", "all", "-R", &repo.slug(), "--json", "number"]);
    all_rows.assert_ok("gea pr list -s all");
    let mut seen = numbers(&all_rows.json());
    seen.sort_unstable();
    assert_eq!(seen, both, "-s all must show both: {}", all_rows.stdout);
}

/// The `number` column of a `--json` listing.
fn numbers(v: &serde_json::Value) -> Vec<i64> {
    v.as_array().expect("a JSON array").iter().filter_map(|row| row["number"].as_i64()).collect()
}

/// `--auto` is the one merge that is supposed to do nothing yet, and "nothing happened" is
/// indistinguishable from "the request was dropped" unless something else can see the schedule.
///
/// Nothing in the pull request object records it. The only observable is that cancelling it
/// succeeds **once**: `DELETE …/pulls/{n}/merge` answers 204 while a schedule exists and 404 when
/// none does. So the second cancel exiting 5 is the actual proof that the first one had something
/// to remove — and therefore that `--auto` scheduled rather than silently merging.
#[test]
fn an_auto_merge_leaves_the_pull_request_open_until_the_schedule_is_cancelled() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoCancelScheduledAutoMerge", "repoPullRequestIsMerged"]);
    cover!(porcelain: ["pr merge"], hits: ["repoMergePullRequest"]);
    let repo = TestRepo::create_initialized(inst, "pr-auto");
    let scratch = Scratch::new("auto");
    // Something to wait for. Gitea's schedule re-checks straight away and merges a pull request
    // with nothing blocking it (measured: roughly half the runs had merged before the next read),
    // so a required status check that never reports is what keeps the schedule pending.
    let (code, body) = repo.api(
        "POST",
        "branch_protections",
        Some(r#"{"rule_name":"main","enable_status_check":true,"status_check_contexts":["ci"]}"#),
    );
    assert!((200..300).contains(&code), "could not require a status check: {code}: {body}");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "merge me later");

    inst.gea(["pr", "merge", &index.to_string(), "--auto", "--squash", "-R", &repo.slug()])
        .assert_ok("gea pr merge --auto --squash");

    let pr = pull(&repo, index);
    assert_eq!(
        pr["merged"], false,
        "--auto must schedule rather than merge while a required check is missing: {pr}"
    );
    assert_eq!(pr["state"], "open", "a scheduled merge leaves the pull request open: {pr}");
    inst.gea([
        "raw",
        "repo",
        "pull-request-is-merged",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
    ])
    .assert_code(5, "the merge probe on a pull request that is only scheduled");

    inst.gea([
        "raw",
        "repo",
        "cancel-scheduled-auto-merge",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
    ])
    .assert_ok("gea raw repo cancel-scheduled-auto-merge");

    inst.gea([
        "raw",
        "repo",
        "cancel-scheduled-auto-merge",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
    ])
    .assert_code(
        5,
        "cancelling twice — the 404 is the only evidence the first cancel removed a real schedule",
    );
}

// --------------------------------------------------------------------------------------- reviews

/// A review has to come from somebody else, and once it exists it can be taken out of the reckoning
/// and put back without being destroyed.
///
/// Gitea answers `approve your own pull is not allowed` with a 422, so the self-review arm is a
/// genuine server rule that no `FakeTransport` test would ever discover — and the second account
/// with write access is the only way to reach the code path at all.
///
/// Dismissal is the part worth a round trip: the review keeps its `state` (`APPROVED`) and only its
/// `dismissed` flag moves, so a client that confused the two would look right in a fixture and be
/// wrong about whether the approval still counts.
#[test]
fn a_review_from_a_second_user_can_be_dismissed_and_restored_without_being_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoListPullReviews",
        "repoGetPullReview",
        "repoDismissPullReview",
        "repoUnDismissPullReview",
        "repoDeletePullReview"
    ]);
    cover!(porcelain: ["pr review"], hits: ["repoCreatePullReview"]);
    let repo = TestRepo::create_initialized(inst, "pr-review");
    let scratch = Scratch::new("review");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "review me");
    let reviewer = collaborator(inst, &repo, "prreviewer");

    // The author cannot review their own work, and the message is the server's.
    let refused = inst.gea(["pr", "review", &index.to_string(), "--approve", "-R", &repo.slug()]);
    assert!(
        !refused.ok(),
        "Gitea refuses a self-review; accepting one here means the refusal was swallowed:\n{}\n{}",
        refused.stdout,
        refused.stderr
    );
    assert!(
        reviews(&repo, index).is_empty(),
        "a refused review must leave nothing behind: {:?}",
        reviews(&repo, index)
    );

    inst.gea_as(
        &reviewer.token,
        ["pr", "review", &index.to_string(), "--approve", "-b", "ship it", "-R", &repo.slug()],
    )
    .assert_ok("gea pr review --approve as the second account");

    let listed = inst.gea([
        "raw",
        "repo",
        "list-pull-reviews",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
    ]);
    listed.assert_ok("gea raw repo list-pull-reviews");
    let rows = listed.json();
    let rows = rows.as_array().expect("an array of reviews");
    assert_eq!(rows.len(), 1, "exactly one review should exist: {}", listed.stdout);
    let review_id = rows[0]["id"].as_i64().expect("a review id");
    assert_eq!(rows[0]["state"], "APPROVED", "{}", listed.stdout);
    assert_eq!(rows[0]["user"]["login"], reviewer.name.as_str(), "{}", listed.stdout);

    inst.gea([
        "raw",
        "repo",
        "dismiss-pull-review",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
        "--id",
        &review_id.to_string(),
        "--message",
        "the branch moved under it",
    ])
    .assert_ok("gea raw repo dismiss-pull-review");
    assert_eq!(
        reviews(&repo, index),
        [(review_id, "APPROVED".to_owned(), true)],
        "a dismissal sets `dismissed` and must NOT rewrite `state` — the approval still happened"
    );

    inst.gea([
        "raw",
        "repo",
        "un-dismiss-pull-review",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
        "--id",
        &review_id.to_string(),
    ])
    .assert_ok("gea raw repo un-dismiss-pull-review");
    assert_eq!(
        reviews(&repo, index),
        [(review_id, "APPROVED".to_owned(), false)],
        "un-dismissing must restore the review rather than file a new one"
    );

    let fetched = inst.gea([
        "raw",
        "repo",
        "get-pull-review",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
        "--id",
        &review_id.to_string(),
    ]);
    fetched.assert_ok("gea raw repo get-pull-review");
    assert_eq!(fetched.json()["body"], "ship it", "{}", fetched.stdout);

    inst.gea([
        "raw",
        "repo",
        "delete-pull-review",
        "-R",
        &repo.slug(),
        "--index",
        &index.to_string(),
        "--id",
        &review_id.to_string(),
    ])
    .assert_ok("gea raw repo delete-pull-review");
    assert!(reviews(&repo, index).is_empty(), "the review survived its own deletion");
}

/// A `PENDING` review is a draft: the line comments attached to it are held back until it is
/// submitted, and submitting is a *different* endpoint from creating.
///
/// This is the sequence with the most moving parts in the group and the least mock value. Gitea
/// has no route that adds a line comment to an existing review, so the comment travels inside the
/// review's own `comments` array — an array of objects, which `gea raw` flattens into no flag and
/// which therefore has to arrive through `--body-file`. The comment must name a path and a line
/// that exist in the diff Gitea computed, and `comments_count` on the submitted review is the
/// server's own count, the only thing that proves it was attached to this review rather than filed
/// loose. Once submitted, the comment's thread can be replied to, resolved and reopened — three
/// more routes keyed by the *comment's* id, not the review's.
#[test]
fn a_pending_review_holds_its_line_comments_until_it_is_submitted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoCreatePullReview",
        "repoGetPullReviewComments",
        "repoSubmitPullReview",
        "repoCreatePullReviewCommentReply",
        "repoResolvePullReviewComment",
        "repoUnresolvePullReviewComment"
    ]);
    let repo = TestRepo::create_initialized(inst, "pr-pending");
    let scratch = Scratch::new("pending");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "line notes");
    let reviewer = collaborator(inst, &repo, "prpendrev");
    let idx = index.to_string();

    let body = scratch.path().join("review.json");
    std::fs::write(
        &body,
        r#"{"event":"PENDING","body":"still reading","comments":[
            {"path":"f.txt","body":"this line worries me","new_position":1,"old_position":0}]}"#,
    )
    .expect("write the review body");
    let pending = inst.gea_as(
        &reviewer.token,
        [
            "raw",
            "repo",
            "create-pull-review",
            "-R",
            &repo.slug(),
            "--index",
            &idx,
            "--body-file",
            &body.to_string_lossy(),
        ],
    );
    pending.assert_ok("gea raw repo create-pull-review --event PENDING");
    let review_id = pending.json()["id"].as_i64().expect("a review id");
    assert_eq!(pending.json()["state"], "PENDING", "{}", pending.stdout);

    let listed = inst.gea_as(
        &reviewer.token,
        [
            "raw",
            "repo",
            "get-pull-review-comments",
            "-R",
            &repo.slug(),
            "--index",
            &idx,
            "--id",
            &review_id.to_string(),
        ],
    );
    listed.assert_ok("gea raw repo get-pull-review-comments");
    let rows = listed.json();
    let rows = rows.as_array().expect("an array of comments");
    assert_eq!(rows.len(), 1, "the pending review should hold one comment: {}", listed.stdout);
    let comment_id = rows[0]["id"].as_i64().expect("a comment id");
    assert_eq!(rows[0]["path"], "f.txt", "{}", listed.stdout);
    assert_eq!(
        rows[0]["pull_request_review_id"], review_id,
        "the comment must belong to the pending review, not to the pull request at large: {}",
        listed.stdout
    );
    assert!(
        rows[0]["diff_hunk"].as_str().is_some_and(|h| h.contains("first line")),
        "the server anchors the comment in the diff it computed; an empty hunk means the path or \
         the position was not understood: {}",
        listed.stdout
    );

    let submitted = inst.gea_as(
        &reviewer.token,
        [
            "raw",
            "repo",
            "submit-pull-review",
            "-R",
            &repo.slug(),
            "--index",
            &idx,
            "--id",
            &review_id.to_string(),
            "--event",
            "COMMENT",
            "--body",
            "finished reading",
        ],
    );
    submitted.assert_ok("gea raw repo submit-pull-review");
    assert_eq!(
        submitted.json()["state"],
        "COMMENT",
        "submitting must move the review out of PENDING: {}",
        submitted.stdout
    );
    assert_eq!(
        submitted.json()["comments_count"],
        1,
        "the server's own count is what proves the line comment was carried into the submission: \
         {}",
        submitted.stdout
    );

    let reply = inst.gea_as(
        &reviewer.token,
        [
            "raw",
            "repo",
            "create-pull-review-comment-reply",
            "-R",
            &repo.slug(),
            "--index",
            &idx,
            "--id",
            &comment_id.to_string(),
            "--body",
            "on second thought, fine",
        ],
    );
    reply.assert_ok("gea raw repo create-pull-review-comment-reply");
    assert_eq!(reply.json()["body"], "on second thought, fine", "{}", reply.stdout);
    assert_eq!(reply.json()["path"], "f.txt", "a reply lives on the same line: {}", reply.stdout);

    let resolved_state = |verb: &str| {
        inst.gea_as(
            &reviewer.token,
            [
                "raw",
                "repo",
                &format!("{verb}-pull-review-comment"),
                "-R",
                &repo.slug(),
                "--id",
                &comment_id.to_string(),
            ],
        )
        .assert_ok(&format!("gea raw repo {verb}-pull-review-comment"));
        let (code, body) =
            repo.api("GET", &format!("pulls/{index}/reviews/{review_id}/comments"), None);
        assert_eq!(code, 200, "{body}");
        let rows: serde_json::Value = serde_json::from_str(&body).expect("comments");
        rows.as_array()
            .and_then(|r| r.iter().find(|c| c["id"].as_i64() == Some(comment_id)))
            .map(|c| !c["resolver"].is_null())
            .unwrap_or_else(|| panic!("the comment vanished: {body}"))
    };
    assert!(resolved_state("resolve"), "resolving must record a resolver");
    assert!(!resolved_state("unresolve"), "unresolving must clear the resolver");
}

/// Review requests are a set on the pull request, and Gitea refuses to add anyone who cannot see
/// the repository — 422, with nothing written.
///
/// The refusal arm is the reason this is a live test. Every test repository here is private, so an
/// account that exists but is not a collaborator is the ordinary mistake, and a mock answering 201
/// would let a broken `pr create -r` ship. The state is read back after the refusal precisely
/// because a partially-applied set would also return an error.
#[test]
fn a_review_request_is_refused_for_somebody_who_cannot_see_the_repository() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoCreatePullReviewRequests", "repoDeletePullReviewRequests"]);
    let repo = TestRepo::create_initialized(inst, "pr-reqrev");
    let scratch = Scratch::new("reqrev");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "who should look");
    let idx = index.to_string();
    let member = collaborator(inst, &repo, "prreqin");
    let outsider = inst
        .scoped_user("prreqout", &["read:user"])
        .expect("a second account that is deliberately not a collaborator");

    let refused = inst.gea([
        "raw",
        "repo",
        "create-pull-review-requests",
        "-R",
        &repo.slug(),
        "--index",
        &idx,
        "--reviewers",
        &outsider.name,
    ]);
    assert!(
        !refused.ok(),
        "requesting a review from a non-collaborator on a private repository must fail:\n{}\n{}",
        refused.stdout,
        refused.stderr
    );
    assert!(
        requested_reviewers(&repo, index).is_empty(),
        "a refused request must write nothing: {:?}",
        requested_reviewers(&repo, index)
    );

    inst.gea([
        "raw",
        "repo",
        "create-pull-review-requests",
        "-R",
        &repo.slug(),
        "--index",
        &idx,
        "--reviewers",
        &member.name,
    ])
    .assert_ok("gea raw repo create-pull-review-requests for a collaborator");
    assert_eq!(
        requested_reviewers(&repo, index),
        std::slice::from_ref(&member.name),
        "the request did not reach the pull request's own record"
    );

    inst.gea([
        "raw",
        "repo",
        "delete-pull-review-requests",
        "-R",
        &repo.slug(),
        "--index",
        &idx,
        "--reviewers",
        &member.name,
    ])
    .assert_ok("gea raw repo delete-pull-review-requests");
    assert!(
        requested_reviewers(&repo, index).is_empty(),
        "withdrawing the request left it in place: {:?}",
        requested_reviewers(&repo, index)
    );
}

// ------------------------------------------------------------------- what the pull request touches

/// The three views of the same change — the file table, the diff, and the commits — have to agree,
/// and every one of them is computed by the server from the git history we pushed.
///
/// A mock supplies all three itself, so it can never catch the interesting failure: a per-file
/// line count that does not match the hunk, or a commit list that omits the commit the diff is of.
/// Two commits rather than one, so an off-by-one in the commit walk is visible.
#[test]
fn the_file_table_the_diff_and_the_commit_list_describe_the_same_change() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoGetPullRequestFiles",
        "repoGetPullRequestCommits",
        "repoDownloadPullDiffOrPatch"
    ]);
    cover!(
        porcelain: ["pr files", "pr diff"],
        hits: ["repoGetPullRequestFiles", "repoDownloadPullDiffOrPatch"]
    );
    let repo = TestRepo::create_initialized(inst, "pr-files");
    let scratch = Scratch::new("files");
    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "one.txt", "alpha\nbeta\n", "add one.txt");
    commit_and_push(scratch.path(), "feature", "two.txt", "gamma\n", "add two.txt");
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(r#"{"title":"two files","head":"feature","base":"main","body":"seed"}"#),
    );
    assert_eq!(code, 201, "{body}");
    let index = serde_json::from_str::<serde_json::Value>(&body).expect("a pull request")["number"]
        .as_i64()
        .expect("a number");
    let idx = index.to_string();

    let files = inst.gea([
        "pr",
        "files",
        &idx,
        "-R",
        &repo.slug(),
        "--json",
        "filename,status,additions,deletions",
    ]);
    files.assert_ok("gea pr files --json");
    let table = files.json();
    let mut rows: Vec<(String, String, i64, i64)> = table
        .as_array()
        .expect("an array")
        .iter()
        .map(|f| {
            (
                f["filename"].as_str().unwrap_or_default().to_owned(),
                f["status"].as_str().unwrap_or_default().to_owned(),
                f["additions"].as_i64().unwrap_or_default(),
                f["deletions"].as_i64().unwrap_or_default(),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        [
            ("one.txt".to_owned(), "added".to_owned(), 2, 0),
            ("two.txt".to_owned(), "added".to_owned(), 1, 0),
        ],
        "the per-file line counts are the server's arithmetic over the commits we pushed: {}",
        files.stdout
    );

    // The generated operation must see exactly what the porcelain saw.
    let raw_files =
        inst.gea(["raw", "repo", "get-pull-request-files", "-R", &repo.slug(), "--index", &idx]);
    raw_files.assert_ok("gea raw repo get-pull-request-files");
    let mut raw_names: Vec<String> = raw_files
        .json()
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|f| f["filename"].as_str().map(str::to_owned))
        .collect();
    raw_names.sort();
    assert_eq!(raw_names, ["one.txt", "two.txt"], "{}", raw_files.stdout);

    let names = inst.gea(["pr", "diff", &idx, "--name-only", "-R", &repo.slug()]);
    names.assert_ok("gea pr diff --name-only");
    let mut listed: Vec<&str> = names.stdout.lines().filter(|l| !l.is_empty()).collect();
    listed.sort_unstable();
    assert_eq!(
        listed,
        ["one.txt", "two.txt"],
        "--name-only walks the files endpoint and must agree with the table: {}",
        names.stdout
    );

    let diff = inst.gea(["pr", "diff", &idx, "-R", &repo.slug()]);
    diff.assert_ok("gea pr diff");
    for needle in ["diff --git a/one.txt b/one.txt", "@@ -0,0 +1,2 @@", "+alpha", "+gamma"] {
        assert!(diff.stdout.contains(needle), "the diff is missing {needle:?}: {}", diff.stdout);
    }

    // `.patch` is a different representation from the same endpoint, and the distinguishing
    // feature is the `git am` envelope a plain diff does not have.
    let patch = inst.gea([
        "raw",
        "repo",
        "download-pull-diff-or-patch",
        "-R",
        &repo.slug(),
        "--index",
        &idx,
        "--diff-type",
        "patch",
    ]);
    patch.assert_ok("gea raw repo download-pull-diff-or-patch patch");
    assert!(
        patch.stdout.starts_with("From "),
        "a patch must carry the `git am` envelope a diff does not: {}",
        patch.stdout
    );
    // One numbered message per commit: a patch is a mailbox, not a single blob, and collapsing
    // two commits into one message would make `git am` reproduce a history that is not this one.
    for subject in ["Subject: [PATCH 1/2] add one.txt", "Subject: [PATCH 2/2] add two.txt"] {
        assert!(
            patch.stdout.contains(subject),
            "the patch must carry {subject:?}: {}",
            patch.stdout
        );
    }

    let commits =
        inst.gea(["raw", "repo", "get-pull-request-commits", "-R", &repo.slug(), "--index", &idx]);
    commits.assert_ok("gea raw repo get-pull-request-commits");
    let messages: Vec<String> = commits
        .json()
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|c| c["commit"]["message"].as_str().map(|m| m.trim().to_owned()))
        .collect();
    assert_eq!(
        messages,
        ["add two.txt", "add one.txt"],
        "both commits must be listed, newest first: {}",
        commits.stdout
    );
}

/// A diff is attacker-controlled text: anybody who can open a pull request chooses its contents,
/// and a terminal interprets what is written to it. An ESC sequence in a hunk can retitle the
/// window, hide output, or switch the character set.
///
/// Only a live test can prove this: the bytes have to survive a real `git push`, Gitea's own diff
/// generation, and the HTTP round trip before they reach the sanitiser. A fixture containing an ESC
/// proves the sanitiser works on a string we chose, not on what a server hands back.
#[test]
fn an_escape_sequence_in_a_diff_is_neutralised_unless_it_is_asked_for() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr diff"], hits: ["repoDownloadPullDiffOrPatch"]);
    let repo = TestRepo::create_initialized(inst, "pr-escape");
    let scratch = Scratch::new("escape");
    repo.clone_to(scratch.path());
    commit_and_push(
        scratch.path(),
        "feature",
        "hostile.txt",
        "harmless\n\u{1b}[31mred\u{1b}[0m\n",
        "add hostile.txt",
    );
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(r#"{"title":"hostile diff","head":"feature","base":"main","body":"seed"}"#),
    );
    assert_eq!(code, 201, "{body}");
    let index = serde_json::from_str::<serde_json::Value>(&body).expect("a pull request")["number"]
        .as_i64()
        .expect("a number");

    let safe = inst.gea(["pr", "diff", &index.to_string(), "-R", &repo.slug()]);
    safe.assert_ok("gea pr diff");
    assert!(
        safe.stdout.contains("harmless"),
        "this test is pointless unless the diff really arrived: {}",
        safe.stdout
    );
    assert!(
        !safe.stdout.contains('\u{1b}'),
        "an ESC byte reached the terminal; the diff sanitiser is not running on what the server \
         sent"
    );
    assert!(
        safe.stdout.contains("^[[31m"),
        "the sequence must be shown as caret notation rather than dropped — a user reviewing a \
         change has to be able to see that it is there: {}",
        safe.stdout
    );

    let raw = inst.gea([
        "pr",
        "diff",
        &index.to_string(),
        "--allow-escape-sequences",
        "-R",
        &repo.slug(),
    ]);
    raw.assert_ok("gea pr diff --allow-escape-sequences");
    assert!(
        raw.stdout.contains('\u{1b}'),
        "--allow-escape-sequences must actually turn the protection off, or the flag is a lie: {}",
        raw.stdout.escape_debug()
    );
}

// ------------------------------------------------------------------------ the rest of the surface

/// `POST …/pulls/{n}/update` merges the base branch into the head, and the only proof it happened
/// is the head branch moving to a commit that was not pushed from here.
///
/// A mock cannot produce that commit: it is made by the server, in its own repository, out of two
/// histories it holds. The commit list is checked as well, because a merge that updated the branch
/// without the pull request noticing is the interesting half-failure.
#[test]
fn updating_a_pull_request_brings_the_base_branchs_commits_into_its_head() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoUpdatePullRequest", "repoGetPullRequestCommits"]);
    let repo = TestRepo::create_initialized(inst, "pr-update");
    let scratch = Scratch::new("update");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "needs a refresh");
    let idx = index.to_string();

    let before = pull(&repo, index);
    let before_sha = before["head"]["sha"].as_str().expect("a head sha").to_owned();

    // The base moves on underneath the pull request. `commit_on` rather than `commit_and_push`
    // because the latter would reset `main` onto the feature branch's tip.
    commit_on(scratch.path(), "main", "base.txt", "the base moved\n", "main moves on");

    inst.gea([
        "raw",
        "repo",
        "update-pull-request",
        "-R",
        &repo.slug(),
        "--index",
        &idx,
        "--style",
        "merge",
    ])
    .assert_ok("gea raw repo update-pull-request --style merge");

    let after = pull(&repo, index);
    let after_sha = after["head"]["sha"].as_str().expect("a head sha").to_owned();
    assert_ne!(
        before_sha, after_sha,
        "the update must move the head branch; it is the server that makes the merge commit, so \
         an unchanged sha means nothing happened: {after}"
    );

    // Polled rather than read once. Gitea moves `head.sha` synchronously and recomputes the
    // commit list it serves from a background job, so the listing fetched in the same millisecond
    // as the update still describes the *old* head — a one-shot assertion here fails about half
    // the time, and a fixed sleep is either flaky on a loaded runner or wasted on an idle one.
    let listed = eventually("the commit list to catch up with the updated head", || {
        let run = inst.gea([
            "raw",
            "repo",
            "get-pull-request-commits",
            "-R",
            &repo.slug(),
            "--index",
            &idx,
        ]);
        run.assert_ok("gea raw repo get-pull-request-commits after the update");
        let rows = run.json().as_array().cloned().unwrap_or_default();
        (rows.len() == 2).then_some(rows)
    });
    assert_eq!(
        listed[0]["sha"].as_str(),
        Some(after_sha.as_str()),
        "the newest commit listed must be the head the pull request now points at: {listed:?}"
    );
    assert_eq!(
        listed[0]["parents"].as_array().map(Vec::len),
        Some(2),
        "`--style merge` must produce a real merge commit, with the base and the head as parents; \
         a single parent would mean the branch was rebased instead: {listed:?}"
    );
}

/// `GET /repos/{owner}/{repo}/pulls/{base}/{head}` puts two branch names into path *segments*, and
/// a branch name may legitimately contain a slash.
///
/// That makes it the sharpest encoding test in the group: `topic/slashy` has to leave as
/// `topic%2Fslashy` and be understood by the server as one segment rather than two. Getting it
/// wrong produces a 404 that looks exactly like "no such pull request", and no mock can tell the
/// difference because the mock is the one deciding what the path means.
#[test]
fn a_pull_request_is_findable_by_base_and_head_when_the_head_name_contains_a_slash() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoGetPullRequestByBaseHead"]);
    let repo = TestRepo::create_initialized(inst, "pr-basehead");
    let scratch = Scratch::new("basehead");
    let index = open_pull_request(&repo, scratch.path(), "topic/slashy", "s.txt", "slashy head");

    let found = inst.gea([
        "raw",
        "repo",
        "get-pull-request-by-base-head",
        "-R",
        &repo.slug(),
        "--base",
        "main",
        "--head",
        "topic/slashy",
    ]);
    found.assert_ok("gea raw repo get-pull-request-by-base-head with a slash in the head name");
    assert_eq!(
        found.json()["number"].as_i64(),
        Some(index),
        "the lookup found the wrong pull request: {}",
        found.stdout
    );
    assert_eq!(found.json()["head"]["ref"], "topic/slashy", "{}", found.stdout);

    // The control: a head that does not exist must be a clean 404 rather than a match on a prefix.
    inst.gea([
        "raw",
        "repo",
        "get-pull-request-by-base-head",
        "-R",
        &repo.slug(),
        "--base",
        "main",
        "--head",
        "topic",
    ])
    .assert_code(5, "a base/head lookup for a branch that does not exist");
}

/// `GET …/pulls/pinned` is its own endpoint rather than a filter, so the only way to know it reads
/// the same pin an issue write sets is to set one and look.
///
/// The empty listing before the pin is the control: `[]` is also what a broken endpoint returns.
#[test]
fn only_a_pinned_pull_request_appears_in_the_pinned_listing() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoListPinnedPullRequests"]);
    let repo = TestRepo::create_initialized(inst, "pr-pinned");
    let scratch = Scratch::new("pinned");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "pin me");

    let empty = inst.gea(["raw", "repo", "list-pinned-pull-requests", "-R", &repo.slug()]);
    empty.assert_ok("gea raw repo list-pinned-pull-requests before anything is pinned");
    assert_eq!(
        empty.json().as_array().map(Vec::len),
        Some(0),
        "nothing is pinned yet: {}",
        empty.stdout
    );

    // Pinned out of band: a pull request is pinned through the *issue* endpoint, which belongs to
    // another group's coverage. This test only claims the listing.
    let (code, body) = repo.api("POST", &format!("issues/{index}/pin"), None);
    assert!((200..300).contains(&code), "could not pin #{index}: HTTP {code}: {body}");

    let pinned = inst.gea(["raw", "repo", "list-pinned-pull-requests", "-R", &repo.slug()]);
    pinned.assert_ok("gea raw repo list-pinned-pull-requests");
    assert_eq!(
        numbers(&pinned.json()),
        [index],
        "the pinned listing must reflect the pin an issue write set: {}",
        pinned.stdout
    );

    let (code, body) = repo.api("DELETE", &format!("issues/{index}/pin"), None);
    assert!((200..300).contains(&code), "could not unpin #{index}: HTTP {code}: {body}");
    let after = inst.gea(["raw", "repo", "list-pinned-pull-requests", "-R", &repo.slug()]);
    after.assert_ok("gea raw repo list-pinned-pull-requests after unpinning");
    assert_eq!(
        after.json().as_array().map(Vec::len),
        Some(0),
        "the unpin was not reflected: {}",
        after.stdout
    );
}

/// A repository with no CI has not failed anything, and `gea pr checks` must exit **0** for it.
///
/// This is the standing complaint the command was written against, and it is only reproducible
/// live because the shape Gitea actually sends is one nobody would invent:
///
/// ```json
/// {"state":"","sha":"","total_count":0,"statuses":null}
/// ```
///
/// Three traps in one object. `statuses` is `null` rather than `[]`, so a non-tolerant decode
/// fails. `state` is the empty string, which is **not** a member of the specification's
/// `CommitStatusState` enum, so a strict enum decode fails — this build passes it through and says
/// so as a compatibility note. And a verdict read from `state` rather than from the statuses list
/// would have no arm to take. Exit 8 (pending) or exit 1 (failed) here would both be wrong, and
/// both are what a naive implementation produces.
#[test]
fn a_repository_with_no_ci_reports_no_checks_rather_than_a_failure() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["pr checks", "pr status"],
        hits: ["repoGetCombinedStatusByRef", "repoGetPullRequest"]
    );
    let repo = TestRepo::create_initialized(inst, "pr-checks");
    let scratch = Scratch::new("checks");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "no ci here");
    let idx = index.to_string();

    inst.gea(["pr", "checks", &idx, "-R", &repo.slug()])
        .assert_code(0, "gea pr checks on a repository that runs no CI");

    let machine = inst.gea([
        "pr",
        "checks",
        &idx,
        "-R",
        &repo.slug(),
        "--json",
        "state,statuses,total_count",
    ]);
    machine.assert_ok("gea pr checks --json");
    let status = machine.json();
    assert_eq!(
        status["statuses"].as_array().map(Vec::len),
        Some(0),
        "a null `statuses` must decode to an empty list rather than failing: {}",
        machine.stdout
    );
    // Gitea rolls a commit with no statuses up to `pending` (Forgejo sends an empty string), so
    // `state` alone cannot tell "waiting for CI" from "no CI"; the empty `statuses` is what `pr
    // checks` keys on, and the human output below is where that distinction is asserted.
    assert_eq!(
        status["state"], "pending",
        "Gitea rolls up a commit with no statuses as `pending`: {}",
        machine.stdout
    );
    assert_eq!(status["total_count"], 0, "{}", machine.stdout);

    let human = inst.gea(["pr", "status", &idx, "-R", &repo.slug()]);
    human.assert_ok("gea pr status");
    human.assert_says("none configured");
    human.assert_says("mergeable");
}

/// `pr checkout` fetches `refs/pull/<n>/head` from the **base** repository rather than the head
/// branch, which is the only route that works for a fork or an AGit pull request.
///
/// That ref is created by the server and exists nowhere in the clone until it is fetched, so this
/// cannot be faked: a `FakeGit` test proves only that the argv we assembled is the one we meant.
/// The upstream wiring is asserted too — for a same-repository pull request the local branch is
/// pointed at the *head branch*, not at the pull ref, because pushing to `refs/pull/<n>/head`
/// would be rejected and a push configuration that cannot work is worse than none.
#[test]
fn checking_out_a_pull_request_fetches_the_servers_pull_ref() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr checkout"], hits: ["repoGetPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "pr-checkout");
    let scratch = Scratch::new("checkout");
    let index = open_pull_request(&repo, scratch.path(), "feature", "f.txt", "check me out");
    let head_sha = pull(&repo, index)["head"]["sha"].as_str().expect("a head sha").to_owned();

    // Standing somewhere else, so the fetch has to create the branch rather than update the one
    // that is already checked out.
    git(scratch.path(), &["checkout", "--quiet", "main"]);

    inst.gea_in(
        scratch.path(),
        ["pr", "checkout", &index.to_string(), "-b", "under-review", "-R", &repo.slug()],
    )
    .assert_ok("gea pr checkout -b");

    assert_eq!(
        git(scratch.path(), &["rev-parse", "HEAD"]).trim(),
        head_sha,
        "the checkout must land on the commit the server says is the pull request's tip"
    );
    assert_eq!(
        git(scratch.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "under-review",
        "-b names the local branch"
    );
    assert_eq!(
        git(scratch.path(), &["config", "--get", "branch.under-review.merge"]).trim(),
        "refs/heads/feature",
        "a same-repository pull request is wired to its head branch, so a later `git push` works; \
         wiring it to refs/pull/<n>/head would give a push that the server always rejects"
    );

    // `--detach` is the read-only shape: it must leave no branch behind to clean up.
    git(scratch.path(), &["checkout", "--quiet", "main"]);
    inst.gea_in(
        scratch.path(),
        ["pr", "checkout", &index.to_string(), "--detach", "-R", &repo.slug()],
    )
    .assert_ok("gea pr checkout --detach");
    assert_eq!(
        git(scratch.path(), &["rev-parse", "HEAD"]).trim(),
        head_sha,
        "--detach must still land on the pull request's tip"
    );
    assert_eq!(
        git(scratch.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "HEAD",
        "--detach must leave a detached HEAD rather than a branch"
    );
}

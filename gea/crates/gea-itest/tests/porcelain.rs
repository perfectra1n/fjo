//! Porcelain commands that make several API calls to do one thing.
//!
//! These are where unit tests are weakest. A `FakeTransport` test of a multi-call command
//! decides for itself what the second call returns, so it proves the calls are made in the
//! expected order and nothing about whether the server agrees with any of them. Name-to-id
//! resolution, conflict adoption, and "create then upload" sequences are all only really tested
//! here.

use std::path::PathBuf;

use gea_itest::{TestRepo, commit_and_push, cover, instance_or_skip};

/// A scratch directory that cleans up after itself, for the tests that need a git checkout.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Self(d)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `issue create --label --milestone` takes names on the command line but the API wants ids, so
/// the command lists labels and milestones first. Three calls, and the two lookups are the part
/// a mock cannot check: it would happily return an id that does not exist.
#[test]
fn issue_create_resolves_label_and_milestone_names() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["label create", "milestone create", "issue create"],
        hits: [
            "issueCreateLabel",
            "issueCreateMilestone",
            "issueListLabels",
            "issueGetMilestonesList",
            "issueCreateIssue",
        ],
    );
    let repo = TestRepo::create(inst, "issue-resolve");

    inst.gea(["label", "create", "bug", "-c", "FF0000", "-R", &repo.slug()])
        .assert_ok("gea label create");
    inst.gea(["milestone", "create", "v1", "-R", &repo.slug()]).assert_ok("gea milestone create");

    inst.gea([
        "issue",
        "create",
        "--title",
        "resolved",
        "--body",
        "b",
        "--label",
        "bug",
        "--milestone",
        "v1",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea issue create with a label and a milestone");

    // Checked out of band: the command reporting success is not evidence that the server
    // attached anything.
    let (code, body) = repo.api("GET", "issues/1", None);
    assert_eq!(code, 200, "{body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("an issue");
    assert_eq!(issue["labels"][0]["name"], "bug", "the label was not attached: {body}");
    assert_eq!(issue["milestone"]["title"], "v1", "the milestone was not attached: {body}");
}

/// An unknown name must fail before anything is created, rather than silently dropping the
/// label and reporting success.
#[test]
fn issue_create_rejects_an_unknown_label() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue create"], hits: ["issueListLabels"]);
    let repo = TestRepo::create(inst, "issue-badlabel");

    let run = inst.gea([
        "issue",
        "create",
        "--title",
        "t",
        "--body",
        "b",
        "--label",
        "no-such-label",
        "-R",
        &repo.slug(),
    ]);
    assert!(!run.ok(), "an unknown label should not succeed:\n{}\n{}", run.stdout, run.stderr);

    let (_, body) = repo.api("GET", "issues", None);
    let issues: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    assert_eq!(
        issues.as_array().map(Vec::len),
        Some(0),
        "the issue must not be created when a label could not be resolved: {body}"
    );
}

/// `release create` with files is create-then-upload-each: one JSON POST followed by a
/// `multipart/form-data` POST per asset, streamed from the file. The bytes are compared after a
/// round trip, because a multipart body that is subtly wrong still uploads *something*.
#[test]
fn release_create_uploads_assets_intact() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["release create"],
        hits: ["repoCreateRelease", "repoCreateReleaseAttachment"],
    );
    let repo = TestRepo::create_initialized(inst, "release-assets");
    let scratch = Scratch::new("rel");
    std::fs::create_dir_all(scratch.path()).expect("scratch dir");

    // Deliberately not text, and larger than one buffer, so a chunking bug shows up.
    let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let big = scratch.path().join("big.bin");
    std::fs::write(&big, &blob).expect("write the asset");
    let small = scratch.path().join("notes.txt");
    std::fs::write(&small, b"release notes payload").expect("write the asset");

    inst.gea([
        "release",
        "create",
        "v0.1.0",
        "-R",
        &repo.slug(),
        "--title",
        "v0.1.0",
        "--notes",
        "first release",
        &big.to_string_lossy(),
        &small.to_string_lossy(),
    ])
    .assert_ok("gea release create with assets");

    let (code, body) = repo.api("GET", "releases/tags/v0.1.0", None);
    assert_eq!(code, 200, "{body}");
    let rel: serde_json::Value = serde_json::from_str(&body).expect("a release");
    let assets = rel["assets"].as_array().cloned().unwrap_or_default();
    assert_eq!(assets.len(), 2, "both assets should be attached: {body}");

    let big_asset = assets
        .iter()
        .find(|a| a["name"] == "big.bin")
        .unwrap_or_else(|| panic!("big.bin is missing: {body}"));
    assert_eq!(
        big_asset["size"].as_u64(),
        Some(blob.len() as u64),
        "the uploaded size does not match the file, so the multipart body was truncated"
    );
}

/// Forking with an explicit name works, which is the control for the test below.
#[test]
fn repo_fork_with_an_explicit_name() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["repo fork"], hits: ["createFork"]);
    let repo = TestRepo::create_initialized(inst, "fork-src");
    // Public: a fork of a private repository is a different permission path.
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    let org = format!("forkorg{}", std::process::id());
    let (code, body) = inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{org}"}}"#)));
    assert!((200..300).contains(&code) || code == 422, "could not create an org: {code} {body}");

    let fork_name = format!("{}-forked", repo.name);
    let run = inst.gea(["repo", "fork", &repo.slug(), "--org", &org, "--fork-name", &fork_name]);
    run.assert_ok("gea repo fork --fork-name");

    let (code, _) = inst.api("GET", &format!("repos/{org}/{fork_name}"), None);
    assert_eq!(code, 200, "the fork was reported but does not exist");
    let _ = inst.api("DELETE", &format!("repos/{org}/{fork_name}"), None);
    let _ = inst.api("DELETE", &format!("orgs/{org}"), None);
}

/// `gea repo fork <slug>` with no `--fork-name` — the ordinary invocation.
///
/// This was ignored as a known bug: `repo/fork.rs` built the body as
/// `name: Some(args.fork_name.clone().unwrap_or_default())` and the same for `organization`, so
/// an unset flag went out as `""` rather than omitted, and Gitea answered `500 name is empty`.
/// The model was always correct (`CreateForkOption.name` is `Option<String>` with
/// `skip_serializing_if`); the call site reintroduced the zero value — the exact failure mode
/// commit b62c3d1 set out to remove. The `Option` is now passed straight through, so this runs
/// and guards against the next `unwrap_or_default()`.
///
/// It also now covers the source repository no longer being fetched without `--clone`: the only
/// reader of that object was the clone path, so the plain form spent a round trip on a value
/// nobody looked at. Dropping a request is invisible until the thing it fed turns out to have
/// been load-bearing after all, so the fork is read back and checked to be a real fork **of this
/// source** — not merely a repository with the right name.
#[test]
fn repo_fork_without_a_name() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["repo fork"], hits: ["createFork"]);
    let repo = TestRepo::create_initialized(inst, "fork-plain");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    let org = format!("forkplain{}", std::process::id());
    inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{org}"}}"#)));

    let run = inst.gea(["repo", "fork", &repo.slug(), "--org", &org]);
    let (code, body) = inst.api("GET", &format!("repos/{org}/{}", repo.name), None);
    let _ = inst.api("DELETE", &format!("repos/{org}/{}", repo.name), None);
    let _ = inst.api("DELETE", &format!("orgs/{org}"), None);
    run.assert_ok("gea repo fork with no --fork-name");

    assert_eq!(code, 200, "the fork was reported but does not exist: {body}");
    let fork: serde_json::Value = serde_json::from_str(&body).expect("a repository");
    assert_eq!(
        fork["fork"],
        serde_json::Value::Bool(true),
        "a repository was created but it is not a fork: {body}"
    );
    assert_eq!(
        fork["parent"]["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "the fork points at the wrong source: {body}"
    );
}

/// `pr create --fill` reads the branch and its commit message from git, then creates.
///
/// This was ignored while the null-scalar decode bug stood, and the failure it produced is
/// worth recording: the pull request **was created** — the server answered 201 with a valid
/// body — and the command then failed decoding `merge_commit_sha` (see `decode.rs`) and printed
/// "nothing was changed on the server", which was false, so re-running hit a 409. The decode is
/// fixed and this runs.
#[test]
fn pr_create_fill_reads_git() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr create"], hits: ["repoGet", "repoCreatePullRequest"]);
    let repo = TestRepo::create_initialized(inst, "pr-fill");
    let scratch = Scratch::new("prfill");
    repo.clone_to(scratch.path());
    commit_and_push(
        scratch.path(),
        "feature",
        "f.txt",
        "one\n",
        "Add f.txt\n\nThe body comes from this commit message.",
    );

    let run = inst.gea_in(scratch.path(), ["pr", "create", "--fill", "-R", &repo.slug()]);
    run.assert_ok("gea pr create --fill");

    let (_, body) = repo.api("GET", "pulls/1", None);
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    assert_eq!(pr["title"], "Add f.txt", "--fill should take the title from the commit subject");
    assert!(
        pr["body"].as_str().unwrap_or_default().contains("The body comes from"),
        "--fill should take the body from the commit message: {body}"
    );
}

/// `pr merge --squash` checks the pull request's state, merges, and can delete the branch.
///
/// This was ignored for the same reason as everything else in the `pr` group — reading the pull
/// request back decoded `merge_commit_sha`, which is null until the merge happens. Fixed.
#[test]
fn pr_merge_squash_and_delete_branch() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr merge"], hits: ["repoGetPullRequest", "repoMergePullRequest"]);
    let repo = TestRepo::create_initialized(inst, "pr-merge");
    let scratch = Scratch::new("prmerge");
    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "f.txt", "one\n", "Add f.txt");

    let (code, body) =
        repo.api("POST", "pulls", Some(r#"{"title":"merge me","head":"feature","base":"main"}"#));
    assert!((200..300).contains(&code), "{body}");

    inst.gea(["pr", "merge", "1", "--squash", "--delete-branch", "-R", &repo.slug()])
        .assert_ok("gea pr merge --squash --delete-branch");

    let (_, body) = repo.api("GET", "pulls/1", None);
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    assert_eq!(pr["merged"], serde_json::Value::Bool(true), "the pull request was not merged");

    let (code, _) = repo.api("GET", "branches/feature", None);
    assert_eq!(code, 404, "--delete-branch should have removed the head branch");
}

// ---------------------------------------------------------------------------------------------
// Concurrency and redundant-call fixes
//
// A batch of commands that used to make their calls one after another now overlap them, and a
// few stopped making calls they never needed. The unit tests that came with that work count
// requests against `FakeTransport`, which is the right tool for "was this request sent" and the
// wrong one for "is the answer still correct": `FakeTransport` resolves synchronously and never
// returns `Pending`, so a `buffer_unordered` that should have been `buffered` still comes back
// in order there, and a future that is dropped on the floor is indistinguishable from one that
// completed instantly. Everything below runs against a real server, where the requests genuinely
// interleave, and asserts on the *result* rather than on the call count.
// ---------------------------------------------------------------------------------------------

/// `gea issue pin N --position 0` used to send `POST …/pin`, pin the issue, and only then exit 2
/// saying `--position counts from 1`. A user reads a usage error as "nothing happened", so the
/// issue was left pinned by a command that reported failure.
///
/// Both halves are asserted, and the second is the one that matters: an exit code of 2 was
/// already correct while the bug stood. The `pin_order` is read back out of band, because the
/// only evidence that nothing was written is the server's own state.
#[test]
fn issue_pin_refuses_position_zero_without_pinning_anything() {
    let inst = instance_or_skip!();
    // No `hits:`: the whole point is that nothing was sent.
    cover!(porcelain: ["issue pin"]);
    let repo = TestRepo::create(inst, "pin-validate");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"pin me"}"#));
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");

    let run = inst.gea(["issue", "pin", "1", "--position", "0", "-R", &repo.slug()]);
    run.assert_code(2, "gea issue pin --position 0");
    run.assert_says("--position counts from 1");

    let (code, body) = repo.api("GET", "issues/1", None);
    assert_eq!(code, 200, "{body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("an issue");
    assert_eq!(
        issue["pin_order"].as_i64(),
        Some(0),
        "the command exited 2 but the issue is pinned at {}. A rejected --position must be \
         refused before anything is sent, or the error message is a lie: {body}",
        issue["pin_order"]
    );
}

/// `--mark-read` sends one `PATCH` per listed thread, and those now go out six at a time instead
/// of one after another. A dropped future in that fan-out leaves notifications unread while the
/// command still prints "marked N thread(s) read" — silent, and only discovered the next time
/// the inbox comes back fuller than it should be.
///
/// Eighteen threads, deliberately: more than the concurrency bound of six and not a multiple of
/// it, so a lost first wave, a lost tail, and an off-by-one all show up.
///
/// The inbox has to be filled by *somebody else* — Gitea does not notify you about your own
/// actions — hence the second account. The repository is made public so that account can file
/// issues in it without being added as a collaborator, and the listing is scoped with `-R` so
/// this test neither sees nor marks threads belonging to another test sharing the instance.
#[test]
fn notification_mark_read_marks_every_thread_it_listed() {
    const THREADS: usize = 18;

    let inst = instance_or_skip!();
    cover!(
        porcelain: ["notification list"],
        hits: ["notifyGetRepoList", "notifyReadThread"],
    );
    let Ok(reporter) = inst.scoped_user("notifier", &["all"]) else {
        panic!("could not mint a second account to fill the inbox with");
    };
    let repo = TestRepo::create_initialized(inst, "notif-mark");
    // Public, so the second account can open issues without a collaborator grant.
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    for i in 1..=THREADS {
        let (code, body) = inst.api_as(
            &reporter.token,
            "POST",
            &format!("repos/{}/issues", repo.slug()),
            Some(&format!(r#"{{"title":"from the reporter {i}"}}"#)),
        );
        assert!((200..300).contains(&code), "seeding issue {i} failed: HTTP {code}: {body}");
    }

    // Gitea writes notifications from a queue, so they are not there the instant the issue is
    // created. Polled rather than slept on: a fixed sleep is either flaky or slow.
    let unread = |state: &str| -> Vec<serde_json::Value> {
        let (_, body) = repo.api(
            "GET",
            &format!("notifications?status-types={state}&limit={}", THREADS * 2),
            None,
        );
        serde_json::from_str(&body).unwrap_or_default()
    };
    let mut waited = 0;
    while unread("unread").len() < THREADS && waited < 60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        waited += 1;
    }
    assert_eq!(
        unread("unread").len(),
        THREADS,
        "the instance never delivered {THREADS} notifications, so this test would have proved \
         nothing about marking them"
    );

    let run = inst.gea([
        "notification",
        "list",
        "-R",
        &repo.slug(),
        "--limit",
        "50",
        "--mark-read",
        "--json",
        "id",
    ]);
    run.assert_ok("gea notification list --mark-read");
    assert_eq!(
        run.json().as_array().map(Vec::len),
        Some(THREADS),
        "the listing itself was short, so the marking below would be testing the wrong set"
    );

    let left = unread("unread");
    assert!(
        left.is_empty(),
        "{} of {THREADS} thread(s) are still unread after --mark-read reported success. A \
         request lost in the fan-out is invisible to the user: {left:?}",
        left.len()
    );
    assert_eq!(unread("read").len(), THREADS, "every listed thread should now be read");
}

/// `workflow list` now reads both workflow directories at once and then reads each file it
/// found six at a time. Two things can go wrong under that, and neither is reachable with a
/// `FakeTransport`:
///
/// * `try_join!` over the directories would let the first 404 — and one of the two *is*
///   normally a 404 — cancel the directory that actually exists.
/// * `buffer_unordered` instead of `buffered` would pair each row with whichever file's body
///   came back first, producing a table where every path is right and every other column
///   belongs to a different workflow. Against a real server the responses genuinely race, so
///   this is the run where that shows.
///
/// Hence eight files, spread across both directories and with a distinct `name:` in each:
/// more files than the concurrency bound, and a body that can be traced back to its path.
#[test]
fn workflow_list_reads_both_directories_and_keeps_bodies_with_their_paths() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["workflow list"], hits: ["repoGetContents", "repoGetRawFile"]);
    let repo = TestRepo::create_initialized(inst, "workflow-dirs");

    // Path -> the workflow's `name:`. Deliberately not in path order, and deliberately spanning
    // `.yml` and `.yaml`.
    let files = [
        (".github/workflows/f.yml", "FFF"),
        (".gitea/workflows/b.yml", "BBB"),
        (".gitea/workflows/d.yml", "DDD"),
        (".github/workflows/h.yml", "HHH"),
        (".gitea/workflows/a.yml", "AAA"),
        (".gitea/workflows/e.yml", "EEE"),
        (".github/workflows/g.yml", "GGG"),
        (".gitea/workflows/c.yaml", "CCC"),
    ];
    for (path, name) in files {
        let body = gitea_core::http::base64::encode(format!(
            "name: {name}\non: [push]\njobs:\n  j:\n    runs-on: ubuntu-latest\n"
        ));
        let (code, reply) = repo.api(
            "POST",
            &format!("contents/{path}"),
            Some(&format!(r#"{{"content":"{body}","message":"add {path}"}}"#)),
        );
        assert!((200..300).contains(&code), "could not add {path}: HTTP {code}: {reply}");
    }

    let run = inst.gea(["workflow", "list", "-R", &repo.slug(), "--json", "path,workflow_name"]);
    run.assert_ok("gea workflow list");
    let rows = run.json();
    let got: Vec<(String, String)> = rows
        .as_array()
        .expect("an array of workflows")
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap_or_default().to_owned(),
                r["workflow_name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();

    // Path order, which is what `discover` sorts by, with each file's own `name:` beside it.
    let mut want: Vec<(String, String)> =
        files.iter().map(|(p, n)| ((*p).to_owned(), (*n).to_owned())).collect();
    want.sort();

    assert_eq!(
        got, want,
        "every workflow file must appear exactly once, in path order, carrying its own \
         `name:`. A mismatched pairing here means the concurrent reads were collected out of \
         order; a missing directory means one arm was cancelled by another's 404."
    );
}

/// The normal repository has **one** of the two workflow directories Gitea reads, so one of
/// `discover`'s two concurrent listings answers 404 — and that 404 is the expected case, not a
/// failure.
/// `try_join!` there would cancel the directory that exists the moment a missing one answered,
/// reporting "no workflows" (or an error) for a repository whose Actions work.
///
/// Written as its own repository rather than folded into the test above precisely because that
/// one has files in both directories and so never produces the 404 this depends on.
#[test]
fn workflow_list_tolerates_the_directory_that_is_not_there() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["workflow list"], hits: ["repoGetContents", "repoGetRawFile"]);
    let repo = TestRepo::create_initialized(inst, "workflow-onedir");

    let body = gitea_core::http::base64::encode(
        "name: only\non: [push]\njobs:\n  j:\n    runs-on: ubuntu-latest\n",
    );
    let (code, reply) = repo.api(
        "POST",
        "contents/.github/workflows/only.yml",
        Some(&format!(r#"{{"content":"{body}","message":"add the only workflow"}}"#)),
    );
    assert!((200..300).contains(&code), "could not add the workflow: HTTP {code}: {reply}");

    // The 404 this test exists for, confirmed rather than assumed.
    let (code, _) = repo.api("GET", "contents/.gitea/workflows", None);
    assert_eq!(code, 404, ".gitea/workflows was expected to be missing, so this proves nothing");

    let run = inst.gea(["workflow", "list", "-R", &repo.slug(), "--json", "path,workflow_name"]);
    run.assert_ok("gea workflow list where one of the two directories does not exist");
    let rows = run.json();
    let rows = rows.as_array().expect("an array of workflows");
    assert_eq!(rows.len(), 1, "exactly the one workflow that exists: {}", run.stdout);
    assert_eq!(rows[0]["path"].as_str(), Some(".github/workflows/only.yml"));
    assert_eq!(
        rows[0]["workflow_name"].as_str(),
        Some("only"),
        "the file's body must have been read, not just its directory entry"
    );
}

/// `label list --include-org` walks the repository's labels and the owner's organization labels
/// at the same time. On a **user**-owned repository — which is what every test repository here
/// is — `/orgs/{user}/labels` genuinely answers 404, and that is not a failure: the flag means
/// "as well", not "instead".
///
/// `try_join!` would abandon the repository's labels the moment that tolerated 404 arrived and
/// turn a working command into a hard error, so this asserts on the exit code *and* on the rows,
/// against the owner shape that produces the 404 for real rather than a mocked one.
#[test]
fn label_list_include_org_survives_an_owner_that_is_not_an_organization() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["label list"], hits: ["issueListLabels", "orgListLabels"]);
    let repo = TestRepo::create(inst, "label-includeorg");
    for name in ["alpha", "beta", "gamma"] {
        let (code, body) =
            repo.api("POST", "labels", Some(&format!(r#"{{"name":"{name}","color":"00ff00"}}"#)));
        assert!((200..300).contains(&code), "seeding label {name} failed: HTTP {code}: {body}");
    }

    // The 404 this test exists for, confirmed rather than assumed: if Gitea ever starts
    // serving organization labels for a user, the tolerated-failure path stops being exercised
    // and this test quietly becomes a test of nothing.
    let (code, _) = inst.api("GET", &format!("orgs/{}/labels", repo.owner), None);
    assert_eq!(
        code, 404,
        "/orgs/{}/labels no longer 404s for a user, so --include-org's tolerated failure is \
         no longer exercised here",
        repo.owner
    );

    let run = inst.gea(["label", "list", "-R", &repo.slug(), "--include-org", "--json", "name"]);
    run.assert_ok("gea label list --include-org on a user-owned repository");
    let mut names: Vec<String> = run
        .json()
        .as_array()
        .expect("an array of labels")
        .iter()
        .map(|l| l["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["alpha", "beta", "gamma"],
        "the repository's own labels must survive the organization walk's 404"
    );
}

/// `issue view -c` now fetches the issue and its comments at the same time on the human path.
/// The risk of that is a comment list that arrives detached from the issue it belongs to, or one
/// that is silently dropped, so this asserts every comment is present and still in the order the
/// API returned them.
///
/// Eight comments rather than two: enough that a body printed out of order is unambiguous rather
/// than a coin flip.
#[test]
fn issue_view_with_comments_prints_all_of_them_in_order() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue view"], hits: ["issueGetIssue", "issueGetComments"]);
    let repo = TestRepo::create(inst, "issue-viewcomments");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"talkative"}"#));
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");
    for i in 1..=8 {
        let (code, body) = repo.api(
            "POST",
            "issues/1/comments",
            Some(&format!(r#"{{"body":"comment body number {i}"}}"#)),
        );
        assert!((200..300).contains(&code), "seeding comment {i} failed: HTTP {code}: {body}");
    }

    let run = inst.gea(["issue", "view", "1", "-c", "-R", &repo.slug()]);
    run.assert_ok("gea issue view -c");

    let positions: Vec<usize> = (1..=8)
        .map(|i| {
            run.stdout.find(&format!("comment body number {i}")).unwrap_or_else(|| {
                panic!("comment {i} is missing from the output:\n{}", run.stdout)
            })
        })
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "the comments were printed out of order, so the concurrent fetch reshuffled them:\n{}",
        run.stdout
    );
}

/// `repo view` fetches the repository and its README at the same time on a terminal, and must
/// still print the README it fetched. `--json` takes the other branch: it emits the repository
/// alone and must never start the README fetch at all, which is visible here as the command
/// succeeding and emitting the repository object.
///
/// The README is rewritten with a marker rather than trusting `auto_init`'s default text, so the
/// assertion is about *this* repository's README and not about any file that happens to render.
#[test]
fn repo_view_prints_the_readme_it_fetched_concurrently() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["repo view"],
        hits: ["repoGet", "repoGetContentsList", "repoGetRawFile"],
    );
    let repo = TestRepo::create_initialized(inst, "repo-viewreadme");

    let (code, body) = repo.api("GET", "contents/README.md", None);
    assert_eq!(code, 200, "auto_init should have written a README: {body}");
    let existing: serde_json::Value = serde_json::from_str(&body).expect("a contents response");
    let sha = existing["sha"].as_str().expect("a blob sha");
    let content =
        gitea_core::http::base64::encode("# Title\n\nREADME_MARKER_FOR_CONCURRENT_VIEW\n");
    let (code, body) = repo.api(
        "PUT",
        "contents/README.md",
        Some(&format!(r#"{{"content":"{content}","message":"marker","sha":"{sha}"}}"#)),
    );
    assert!((200..300).contains(&code), "could not rewrite the README: HTTP {code}: {body}");

    // Terminal rendering, because the README is deliberately TTY-only output — the same reason
    // `pagination.rs`'s banner test asks for it.
    let run = inst.gea_env(
        std::path::Path::new("."),
        &[("GEA_FORCE_TTY", "100")],
        ["repo", "view", &repo.slug()],
    );
    run.assert_ok("gea repo view");
    run.assert_says("README_MARKER_FOR_CONCURRENT_VIEW");

    let run = inst.gea(["repo", "view", &repo.slug(), "--json", "full_name,private"]);
    run.assert_ok("gea repo view --json");
    assert_eq!(
        run.json()["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "--json must still emit the repository, having skipped the README entirely"
    );
}

/// `issue depends list` walks `/dependencies` and `/blocks` at the same time. Both directions
/// have to survive that, and they have to stay the right way round — two walks that return the
/// same shape are exactly the pair a wiring mistake makes look fine.
#[test]
fn issue_depends_list_shows_both_directions() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["issue depends add", "issue depends list"],
        hits: [
            "issueCreateIssueDependencies",
            "issueCreateIssueBlocking",
            "issueListIssueDependencies",
            "issueListBlocks",
        ],
    );
    let repo = TestRepo::create(inst, "issue-depends");
    for title in ["the subject", "the blocker", "the blocked"] {
        let (code, body) = repo.api("POST", "issues", Some(&format!(r#"{{"title":"{title}"}}"#)));
        assert!((200..300).contains(&code), "seeding {title} failed: HTTP {code}: {body}");
    }

    inst.gea(["issue", "depends", "add", "1", "--blocked-by", "2", "-R", &repo.slug()])
        .assert_ok("gea issue depends add --blocked-by");
    inst.gea(["issue", "depends", "add", "1", "--blocks", "3", "-R", &repo.slug()])
        .assert_ok("gea issue depends add --blocks");

    let run = inst.gea(["issue", "depends", "list", "1", "-R", &repo.slug()]);
    run.assert_ok("gea issue depends list");
    let blocked_by = run
        .stdout
        .lines()
        .find(|l| l.starts_with("blocked by"))
        .unwrap_or_else(|| panic!("no 'blocked by' row:\n{}", run.stdout));
    let blocks = run
        .stdout
        .lines()
        .find(|l| l.starts_with("blocks"))
        .unwrap_or_else(|| panic!("no 'blocks' row:\n{}", run.stdout));
    assert!(
        blocked_by.contains("the blocker"),
        "the blocker ended up on the wrong side:\n{}",
        run.stdout
    );
    assert!(
        blocks.contains("the blocked"),
        "what the issue blocks ended up on the wrong side:\n{}",
        run.stdout
    );
}

/// `pr view -c --json` and `pr status --json` stopped fetching the comments and checks that
/// machine output discards. Skipping a request is the sort of change that is invisible until the
/// output it fed turns out to have been needed after all, so this asserts the machine output is
/// still complete and still correct — including the fields `pr status` derives, which come from
/// the pull request itself rather than from the checks it no longer fetches.
#[test]
fn pr_machine_output_is_complete_without_the_calls_it_stopped_making() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["pr view", "pr status"],
        hits: ["repoGetPullRequest", "issueGetComments"],
    );
    let repo = TestRepo::create_initialized(inst, "pr-machine");
    let scratch = Scratch::new("prmachine");
    repo.clone_to(scratch.path());
    commit_and_push(scratch.path(), "feature", "f.txt", "one\n", "Add f.txt");

    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(r#"{"title":"machine readable","head":"feature","base":"main"}"#),
    );
    assert!((200..300).contains(&code), "{body}");
    let (code, body) = repo.api("POST", "issues/1/comments", Some(r#"{"body":"a comment"}"#));
    assert!((200..300).contains(&code), "seeding the comment failed: HTTP {code}: {body}");

    let run = inst.gea(["pr", "view", "1", "-c", "-R", &repo.slug(), "--json", "number,title"]);
    run.assert_ok("gea pr view -c --json");
    let pr = run.json();
    assert_eq!(pr["number"].as_u64(), Some(1), "{}", run.stdout);
    assert_eq!(pr["title"].as_str(), Some("machine readable"), "{}", run.stdout);

    let run =
        inst.gea(["pr", "status", "1", "-R", &repo.slug(), "--json", "number,title,mergeable"]);
    run.assert_ok("gea pr status --json");
    let status = run.json();
    assert_eq!(status["number"].as_u64(), Some(1), "{}", run.stdout);
    assert_eq!(status["title"].as_str(), Some("machine readable"), "{}", run.stdout);
    assert!(
        status["mergeable"].is_boolean(),
        "mergeability comes from the pull request itself, so dropping the checks fetch must not \
         have taken it with it: {}",
        run.stdout
    );

    // The human path still shows the comments, which is the control for the assertions above:
    // `-c --json` dropping them is a deliberate choice about machine output, not a lost fetch.
    let run = inst.gea(["pr", "view", "1", "-c", "-R", &repo.slug()]);
    run.assert_ok("gea pr view -c");
    run.assert_says("a comment");
}

/// `block list` renders the rows in the order the server lists them. Ten accounts, with logins
/// chosen so that alphabetical order and creation order differ — otherwise a client-side sort
/// could pass by coincidence.
#[test]
fn block_list_rows_follow_the_api_listing_order() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["block add", "block list", "block remove"],
        hits: ["userBlockUser", "userUnblockUser", "userListBlocks"],
    );
    // Names unique to this process: blocking is an account-level act, not a repository one, so
    // there is no `TestRepo` to scope it to.
    let tag = std::process::id();
    let mine = |login: &str| login.starts_with("blocked") && login.ends_with(&tag.to_string());
    let logins: Vec<String> =
        [7, 2, 9, 4, 1, 8, 3, 10, 5, 6].iter().map(|n| format!("blocked{n}x{tag}")).collect();
    for login in &logins {
        let (code, body) = inst.api(
            "POST",
            "admin/users",
            Some(&format!(
                r#"{{"username":"{login}","email":"{login}@example.invalid","password":"gea-itest-blocked-1","must_change_password":false}}"#
            )),
        );
        assert!(
            (200..300).contains(&code) || code == 422,
            "could not create {login}: HTTP {code}: {body}"
        );
        inst.gea(["block", "add", login]).assert_ok("gea block add");
    }

    // What the server lists, read out of band, in the order it lists it.
    let (code, body) = inst.api("GET", "user/blocks?limit=100", None);
    assert_eq!(code, 200, "{body}");
    let listed: serde_json::Value = serde_json::from_str(&body).expect("an array");
    let expected: Vec<String> = listed
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|u| u["login"].as_str().map(str::to_owned))
        .filter(|l| mine(l))
        .collect();
    assert_eq!(
        expected.len(),
        logins.len(),
        "the server did not list every account this test blocked, so the ordering assertion \
         below would be comparing the wrong set"
    );

    let run = inst.gea(["block", "list", "--limit", "100"]);
    run.assert_ok("gea block list");
    let rendered: Vec<String> = run
        .stdout
        .lines()
        // Piped output is headerless TSV: `USER<TAB>NAME`. Rows belonging to another test's
        // accounts are dropped, but the ones that remain stay in the order they were printed.
        .filter_map(|l| l.split('\t').next())
        .filter(|l| mine(l))
        .map(str::to_owned)
        .collect();
    assert_eq!(rendered, expected, "the rows were not rendered in listing order:\n{}", run.stdout);

    for login in &logins {
        let _ = inst.gea(["block", "remove", login]);
    }
}

/// `times list --all` resolves the login it filters by at the same time as it walks
/// `/user/times`, and the filter is then applied client-side because `/user/times` takes no
/// `user` parameter. Getting that wrong loses every entry or filters none of them, and both
/// failures are silent.
#[test]
fn times_list_all_still_filters_by_the_login_it_resolved() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["times list"],
        hits: ["userGetCurrent", "userCurrentTrackedTimes"],
    );
    let repo = TestRepo::create(inst, "times-all");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"timed"}"#));
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");
    let (code, body) = repo.api("POST", "issues/1/times", Some(r#"{"time":3600}"#));
    assert!((200..300).contains(&code), "recording time failed: HTTP {code}: {body}");

    // `--mine` is the branch that has to resolve a login before it can filter, and `/user/times`
    // is inherently the caller's, so every entry here is the caller's: the filter must keep them.
    let run =
        inst.gea(["times", "list", "--all", "--mine", "--limit", "100", "--json", "id,user_name"]);
    run.assert_ok("gea times list --all --mine");
    let rows = run.json();
    let rows = rows.as_array().expect("an array of tracked time");
    assert!(
        rows.iter().any(|t| t["user_name"].as_str() == Some(inst.user.as_str())),
        "the entry just recorded is missing, so the client-side filter dropped what the walk \
         found: {}",
        run.stdout
    );
    assert!(
        rows.iter().all(|t| t["user_name"].as_str() == Some(inst.user.as_str())),
        "--mine returned somebody else's entries, so the filter was not applied at all: {}",
        run.stdout
    );
}

/// `gea nodeinfo --jq <broken>` must fail on the expression before it sends anything. Compiling
/// it *after* the `/version` read would cost a round trip, and on an unreachable or wrong URL
/// would report the server's failure instead — sending the user to look at their URL rather than
/// at their jq. The exit code and message are the whole difference between compiling the
/// expression first and compiling it last.
#[test]
fn a_broken_jq_expression_beats_the_request_it_would_have_filtered() {
    let inst = instance_or_skip!();
    // Nothing is declared here. `gea nodeinfo` is a *group* with one leaf (`nodeinfo limits`),
    // so the bare form this drives is not in the porcelain inventory — and no operation was
    // exercised either, since the whole assertion is that the request never went out.

    let run = inst.gea(["nodeinfo", "--jq", ".bad["]);
    run.assert_code(2, "gea nodeinfo --jq with a broken expression");
    run.assert_says("invalid --jq expression");
    assert!(
        !run.stderr.contains("was not found"),
        "the server's answer was reported instead of the broken expression, so the request went \
         out first:\n{}",
        run.stderr
    );
}

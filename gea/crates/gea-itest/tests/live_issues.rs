//! Issues, comments, labels, milestones, reactions, attachments, tracked time and stopwatches,
//! driven end to end against a real Gitea.
//!
//! # Why these lifecycles cannot be mocked
//!
//! Every test here is a *round trip*: a `gea` command writes, and the result is read back out of
//! band with `curl`. That second half is the whole point. A `FakeTransport` test decides for
//! itself what the server returns, so it proves the request matched our reading of the
//! specification and nothing about whether Gitea agrees. The failures that only show up here
//! are the ones where both sides are individually plausible:
//!
//! * a body field serialised under the wrong wire name, which the server ignores and answers
//!   `200` to anyway (`issueReplaceLabels` sending `label` for `labels` would look identical);
//! * a path built from the wrong id *kind* — `/issues/{index}` and `/issues/comments/{id}` both
//!   take a bare integer, so `42` addresses a real object either way and a mix-up never 404s;
//! * a `multipart/form-data` body that is subtly malformed but still uploads *something*.
//!
//! So attachments are compared byte for byte after the round trip, reactions are exercised on
//! both collections in the same test with deliberately non-equal ids, and nothing is believed
//! because a command exited 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use gea_itest::{TestRepo, cover, instance_or_skip};

// ------------------------------------------------------------------------------- fixtures

/// A scratch directory that cleans up after itself, for the tests that need files on disk.
///
/// Named after the process *and* the caller's tag: tests in this file run on parallel threads
/// inside one binary, so a shared path would have two tests writing the same attachment.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-issues-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("creating the scratch directory");
        Self(d)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("writing a scratch file");
        p
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

/// Create an issue straight through the API and return its index.
///
/// Seeding with `curl` rather than with `gea` on purpose: a test of `issue edit` that seeds with
/// `issue create` fails for two different reasons with one message.
fn seed_issue(repo: &TestRepo<'_>, title: &str) -> i64 {
    let (code, body) = repo.api("POST", "issues", Some(&format!(r#"{{"title":"{title}"}}"#)));
    assert!((200..300).contains(&code), "seeding issue {title:?} failed: HTTP {code}: {body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("the created issue");
    issue["number"].as_i64().unwrap_or_else(|| panic!("the issue has no number: {body}"))
}

/// Create a comment straight through the API and return its database id.
fn seed_comment(repo: &TestRepo<'_>, index: i64, text: &str) -> i64 {
    let (code, body) = repo.api(
        "POST",
        &format!("issues/{index}/comments"),
        Some(&format!(r#"{{"body":"{text}"}}"#)),
    );
    assert!((200..300).contains(&code), "seeding a comment failed: HTTP {code}: {body}");
    let comment: serde_json::Value = serde_json::from_str(&body).expect("the created comment");
    comment["id"].as_i64().unwrap_or_else(|| panic!("the comment has no id: {body}"))
}

/// Make the next comment id created in `repo` differ from `index`, by creating and deleting
/// comments until it does.
///
/// Comment ids are **global to the instance and monotonic**; an issue *index* restarts at 1 in
/// every repository. So on a fresh container the very first comment really does get id 1, which
/// is also the first issue's index — and several tests below exist precisely because
/// `/issues/{index}/…` and `/issues/comments/{id}/…` take different *kinds* of number in the same
/// position, so a mix-up addresses a real object rather than 404ing. Two equal numbers make that
/// undetectable.
///
/// Asserting the two differ was the first attempt, and it failed roughly one run in five —
/// whenever this test won the race to the first comment on a new container. Making them differ
/// is the fix; an assertion that depends on test ordering is not a test.
fn burn_colliding_comment_ids(repo: &TestRepo<'_>, index: i64) {
    loop {
        let burner = seed_comment(repo, index, "burning a comment id");
        let (code, body) = repo.api("DELETE", &format!("issues/comments/{burner}"), None);
        assert!(
            (200..300).contains(&code),
            "removing the id-burning comment failed: HTTP {code}: {body}"
        );
        if burner > index {
            return;
        }
    }
}

/// Read an issue back out of band.
fn read_issue(repo: &TestRepo<'_>, index: i64) -> serde_json::Value {
    let (code, body) = repo.api("GET", &format!("issues/{index}"), None);
    assert_eq!(code, 200, "reading issue #{index} back failed: {body}");
    serde_json::from_str(&body).expect("an issue document")
}

/// Fetch a URL's raw bytes with the admin token.
///
/// `curl` and not our own client: the question an attachment test asks is "are the bytes the
/// server stored the bytes we sent", and checking that through the stack that may have mangled
/// them on the way up answers nothing.
fn download(url: &str, token: &str) -> Vec<u8> {
    let out = Command::new("curl")
        .args(["-sSL", "--max-time", "60"])
        .args(["-H", &format!("Authorization: token {token}")])
        .arg(url)
        .output()
        .expect("curl should run");
    assert!(
        out.status.success(),
        "downloading {url} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// One field of every object in a list, sorted — the shape most assertions here want.
///
/// A JSON `null` counts as an empty list rather than as a failure: Gitea answers *some* empty
/// collections with `null` and others with `[]` (the reaction endpoints are the former), and a
/// helper that panicked on the difference would turn "nothing here, as expected" into a crash.
fn names(v: &serde_json::Value, key: &str) -> Vec<String> {
    let Some(items) = v.as_array() else {
        assert!(v.is_null(), "expected a list or null, got {v}");
        return Vec::new();
    };
    let mut out: Vec<String> =
        items.iter().map(|o| o[key].as_str().unwrap_or_default().to_owned()).collect();
    out.sort();
    out
}

/// How many items a list holds, counting `null` as none. See [`names`] for why that matters.
fn count(v: &serde_json::Value) -> usize {
    match v.as_array() {
        Some(items) => items.len(),
        None => {
            assert!(v.is_null(), "expected a list or null, got {v}");
            0
        }
    }
}

// --------------------------------------------------------------------------- issue CRUD

/// The layer-2 issue lifecycle, end to end.
///
/// The read-backs are what make this worth running: `edit-issue` sends a sparse patch, and a
/// field serialised under a name Gitea does not know is answered with `200` and the issue
/// unchanged. Only a fresh `GET` can tell "the server applied it" from "the server ignored it".
///
/// `delete` is included because an issue that survives its own deletion is the one failure a
/// command's exit code structurally cannot report.
#[test]
fn a_raw_issue_can_be_created_read_edited_listed_and_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateIssue",
        "issueGetIssue",
        "issueEditIssue",
        "issueListIssues",
        "issueDelete"
    ]);
    let repo = TestRepo::create(inst, "raw-issue-crud");

    let created = inst
        .gea([
            "raw",
            "issue",
            "create-issue",
            &repo.owner,
            &repo.name,
            "--title",
            "layer two created me",
            "--body",
            "the original body",
        ])
        .assert_ok("gea raw issue create-issue")
        .json();
    let index = created["number"].as_i64().expect("the created issue carries a number");

    let got = inst
        .gea(["raw", "issue", "get-issue", &repo.owner, &repo.name, &index.to_string()])
        .assert_ok("gea raw issue get-issue")
        .json();
    assert_eq!(got["title"], "layer two created me", "get-issue returned a different issue");
    assert_eq!(got["body"], "the original body");

    inst.gea([
        "raw",
        "issue",
        "edit-issue",
        &repo.owner,
        &repo.name,
        &index.to_string(),
        "--title",
        "layer two edited me",
        "--state",
        "closed",
    ])
    .assert_ok("gea raw issue edit-issue");

    let issue = read_issue(&repo, index);
    assert_eq!(issue["title"], "layer two edited me", "the patched title never reached the server");
    assert_eq!(issue["state"], "closed", "the patched state never reached the server");
    assert_eq!(
        issue["body"], "the original body",
        "a sparse patch overwrote a field it never named"
    );

    // `--state all`: the issue is closed by now, so the default filter would hide it and the
    // listing would pass vacuously.
    let listed = inst
        .gea(["raw", "issue", "list-issues", &repo.owner, &repo.name, "--state", "all"])
        .assert_ok("gea raw issue list-issues")
        .json();
    assert!(
        listed.as_array().is_some_and(|a| a.iter().any(|i| i["number"].as_i64() == Some(index))),
        "the issue is missing from list-issues: {listed}"
    );

    inst.gea(["raw", "issue", "delete", &repo.owner, &repo.name, &index.to_string()])
        .assert_ok("gea raw issue delete");
    let (code, body) = repo.api("GET", &format!("issues/{index}"), None);
    assert_eq!(code, 404, "the issue survived its own deletion: HTTP {code}: {body}");
}

/// `/repos/issues/search` is the one issue read that is not scoped to a repository, so it is the
/// one where a query parameter placed in the path — or under the wrong wire name — still returns
/// a plausible-looking list of somebody else's issues.
///
/// `--since` bounds the answer, and the assertion is that *this* issue is in it. Asserting an
/// exact count would be wrong — the instance is shared with every other test running in
/// parallel — and the `since` value is taken from the issue's own `created_at` rather than from
/// this process's clock, which may be minutes away from the container's.
#[test]
fn searching_issues_across_repositories_finds_one_just_created() {
    let inst = instance_or_skip!();
    cover!(raw: ["issueSearchIssues"]);
    let repo = TestRepo::create(inst, "raw-issue-search");

    let index = seed_issue(&repo, "findable by search");
    let since = read_issue(&repo, index)["created_at"]
        .as_str()
        .expect("the issue carries a creation timestamp")
        .to_owned();

    let found = inst
        .gea([
            "raw",
            "issue",
            "search-issues",
            "--type",
            "issues",
            "--state",
            "open",
            "--since",
            &since,
            "--limit",
            "500",
            "--paginate",
        ])
        .assert_ok("gea raw issue search-issues")
        .json();
    let hit = found
        .as_array()
        .unwrap_or_else(|| panic!("search-issues should answer with an array: {found}"))
        .iter()
        .find(|i| {
            i["number"].as_i64() == Some(index)
                && i["repository"]["name"].as_str() == Some(repo.name.as_str())
        });
    assert!(
        hit.is_some(),
        "the issue just created is missing from the cross-repository search, so the query \
         parameters did not reach the server as written"
    );
}

// ----------------------------------------------------------------------------- comments

/// Comments have two route families and Gitea still serves both: `/issues/comments/{id}` and
/// the deprecated `/issues/{index}/comments/{id}`. They take *different kinds of number* in the
/// same position, and a mix-up does not 404 — a comment id is a perfectly good issue index, so
/// the wrong route edits a real, unrelated object.
///
/// Both families are driven here against the same two comments, and every write is read back, so
/// a route that silently edited the wrong row shows up as the *other* comment changing.
#[test]
fn comments_round_trip_through_both_the_current_and_the_deprecated_routes() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateComment",
        "issueGetComments",
        "issueGetComment",
        "issueEditComment",
        "issueDeleteComment",
        "issueEditCommentDeprecated",
        "issueDeleteCommentDeprecated",
        "issueGetRepoComments",
        "issueGetCommentsAndTimeline"
    ]);
    let repo = TestRepo::create(inst, "raw-comments");
    let index = seed_issue(&repo, "talkative");
    let idx = index.to_string();
    burn_colliding_comment_ids(&repo, index);

    let first = inst
        .gea(["raw", "issue", "create-comment", &repo.owner, &repo.name, &idx, "--body", "first"])
        .assert_ok("gea raw issue create-comment")
        .json();
    let first_id = first["id"].as_i64().expect("the comment carries an id");
    let second_id = seed_comment(&repo, index, "second");

    // The two ids must differ from the issue index, or the route mix-up this test exists to
    // catch would be invisible.
    assert_ne!(first_id, index, "the comment id happens to equal the issue index");
    assert_ne!(second_id, index, "the comment id happens to equal the issue index");

    let listed = inst
        .gea(["raw", "issue", "get-comments", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue get-comments")
        .json();
    assert_eq!(names(&listed, "body"), ["first", "second"], "get-comments lost a comment");

    let one = inst
        .gea(["raw", "issue", "get-comment", &repo.owner, &repo.name, &first_id.to_string()])
        .assert_ok("gea raw issue get-comment")
        .json();
    assert_eq!(one["body"], "first", "get-comment answered with a different comment");

    inst.gea([
        "raw",
        "issue",
        "edit-comment",
        &repo.owner,
        &repo.name,
        &first_id.to_string(),
        "--body",
        "first, amended",
    ])
    .assert_ok("gea raw issue edit-comment");

    // The deprecated route takes the issue index *and* the comment id. Sending them the other
    // way round is the mistake; it is only detectable by checking which comment changed.
    inst.gea([
        "raw",
        "issue",
        "edit-comment-deprecated",
        &repo.owner,
        &repo.name,
        &idx,
        &second_id.to_string(),
        "--body",
        "second, amended",
    ])
    .assert_ok("gea raw issue edit-comment-deprecated");

    let (code, body) = repo.api("GET", &format!("issues/{index}/comments"), None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a comment list");
    assert_eq!(
        names(&after, "body"),
        ["first, amended", "second, amended"],
        "an edit landed on the wrong comment, or never landed at all: {body}"
    );

    let repo_wide = inst
        .gea(["raw", "issue", "get-repo-comments", &repo.owner, &repo.name, "--paginate"])
        .assert_ok("gea raw issue get-repo-comments")
        .json();
    assert_eq!(
        names(&repo_wide, "body"),
        ["first, amended", "second, amended"],
        "the repository-wide comment listing does not match the issue's own"
    );

    // The timeline is a superset: it carries state changes as well as comments, so this asserts
    // the comments are present rather than that nothing else is.
    let timeline = inst
        .gea([
            "raw",
            "issue",
            "get-comments-and-timeline",
            &repo.owner,
            &repo.name,
            &idx,
            "--paginate",
        ])
        .assert_ok("gea raw issue get-comments-and-timeline")
        .json();
    let bodies = names(&timeline, "body");
    for want in ["first, amended", "second, amended"] {
        assert!(bodies.iter().any(|b| b == want), "the timeline is missing {want:?}: {timeline}");
    }

    inst.gea(["raw", "issue", "delete-comment", &repo.owner, &repo.name, &first_id.to_string()])
        .assert_ok("gea raw issue delete-comment");
    inst.gea([
        "raw",
        "issue",
        "delete-comment-deprecated",
        &repo.owner,
        &repo.name,
        &idx,
        &second_id.to_string(),
    ])
    .assert_ok("gea raw issue delete-comment-deprecated");

    let (code, body) = repo.api("GET", &format!("issues/{index}/comments"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a comment list");
    assert_eq!(count(&left), 0, "a comment survived its own deletion: {body}");
}

// ------------------------------------------------------------------------------- labels

/// The label collection and the four ways an issue's labels can be written.
///
/// `add` and `replace` are the pair worth testing against a real server: they are the same body
/// shape on the same path with different verbs, and a `POST` where a `PUT` belongs looks like
/// success while quietly *keeping* labels the caller meant to drop. Each write is followed by a
/// read of the issue's label set, so the difference is asserted rather than assumed.
#[test]
fn issue_labels_can_be_added_replaced_removed_and_cleared() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateLabel",
        "issueListLabels",
        "issueGetLabel",
        "issueEditLabel",
        "issueDeleteLabel",
        "issueAddLabel",
        "issueGetLabels",
        "issueReplaceLabels",
        "issueRemoveLabel",
        "issueClearLabels"
    ]);
    let repo = TestRepo::create(inst, "raw-labels");
    let index = seed_issue(&repo, "labelled");
    let idx = index.to_string();

    let mut ids = Vec::new();
    for (name, color) in [("bug", "e11d21"), ("chore", "00ff00"), ("urgent", "0000ff")] {
        let made = inst
            .gea([
                "raw",
                "issue",
                "create-label",
                &repo.owner,
                &repo.name,
                "--name",
                name,
                "--color",
                color,
            ])
            .assert_ok("gea raw issue create-label")
            .json();
        ids.push(made["id"].as_i64().expect("the label carries an id"));
    }

    let listed = inst
        .gea(["raw", "issue", "list-labels", &repo.owner, &repo.name, "--paginate"])
        .assert_ok("gea raw issue list-labels")
        .json();
    assert_eq!(names(&listed, "name"), ["bug", "chore", "urgent"], "list-labels lost a label");

    let one = inst
        .gea(["raw", "issue", "get-label", &repo.owner, &repo.name, &ids[0].to_string()])
        .assert_ok("gea raw issue get-label")
        .json();
    assert_eq!(one["name"], "bug");

    inst.gea([
        "raw",
        "issue",
        "edit-label",
        &repo.owner,
        &repo.name,
        &ids[0].to_string(),
        "--description",
        "something is broken",
    ])
    .assert_ok("gea raw issue edit-label");
    let (code, body) = repo.api("GET", &format!("labels/{}", ids[0]), None);
    assert_eq!(code, 200, "{body}");
    let label: serde_json::Value = serde_json::from_str(&body).expect("a label");
    assert_eq!(label["description"], "something is broken", "the label patch never landed");
    assert_eq!(label["name"], "bug", "a sparse label patch renamed a field it never named");

    // `--labels` is repeatable and arrives as a JSON array; two flags must become two members
    // rather than the last one winning.
    //
    // **Names, not ids — and that is a finding rather than a convenience.** Gitea's
    // `IssueLabelsOption.labels` is a heterogeneous list and the server switches on each item's
    // JSON *type*: a number is a label id, a string is a label name. Everything a command line
    // carries is a string, so `--labels 3` goes out as `"3"`, Gitea looks for a label literally
    // called `3`, finds none, and answers 200 having attached nothing. The id form is therefore
    // unreachable through this flag; `--body-file` below is how layer 2 gets at it, which is
    // exactly why that branch is exercised too.
    inst.gea([
        "raw",
        "issue",
        "add-label",
        &repo.owner,
        &repo.name,
        &idx,
        "--labels",
        "bug",
        "--labels",
        "chore",
    ])
    .assert_ok("gea raw issue add-label");
    let on_issue = inst
        .gea(["raw", "issue", "get-labels", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue get-labels")
        .json();
    assert_eq!(names(&on_issue, "name"), ["bug", "chore"], "add-label did not add both labels");

    // The whole point of PUT: what is not named is dropped. Driven through `--body-file` so the
    // id goes out as a JSON *number*, which is the other half of the union the server switches
    // on — and the half no combination of flags can produce.
    let scratch = Scratch::new("rawlabels");
    let body = scratch.file("labels.json", format!(r#"{{"labels":[{}]}}"#, ids[2]).as_bytes());
    inst.gea([
        "raw",
        "issue",
        "replace-labels",
        &repo.owner,
        &repo.name,
        &idx,
        "--body-file",
        &body.to_string_lossy(),
    ])
    .assert_ok("gea raw issue replace-labels --body-file");
    assert_eq!(
        names(&read_issue(&repo, index)["labels"], "name"),
        ["urgent"],
        "replace-labels behaved like add-label and kept the labels it was not given"
    );

    // `add-label` takes names as happily as ids, while `DELETE …/labels/{id}` takes an id only:
    // Gitea answers a name there with 404 "label does not exist". So the removal is by id, and
    // the name is asserted to be refused — that asymmetry is why `gea issue edit --remove-label`
    // resolves names first.
    inst.gea(["raw", "issue", "add-label", &repo.owner, &repo.name, &idx, "--labels", "bug"])
        .assert_ok("gea raw issue add-label, second time");
    inst.gea(["raw", "issue", "remove-label", &repo.owner, &repo.name, &idx, "bug"])
        .assert_code(5, "gea raw issue remove-label by name, which Gitea does not accept");
    inst.gea(["raw", "issue", "remove-label", &repo.owner, &repo.name, &idx, &ids[0].to_string()])
        .assert_ok("gea raw issue remove-label by id");
    assert_eq!(
        names(&read_issue(&repo, index)["labels"], "name"),
        ["urgent"],
        "remove-label by id removed the wrong label, or none"
    );

    inst.gea(["raw", "issue", "clear-labels", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue clear-labels");
    assert_eq!(
        count(&read_issue(&repo, index)["labels"]),
        0,
        "clear-labels left labels on the issue"
    );

    inst.gea(["raw", "issue", "delete-label", &repo.owner, &repo.name, &ids[2].to_string()])
        .assert_ok("gea raw issue delete-label");
    let (code, _) = repo.api("GET", &format!("labels/{}", ids[2]), None);
    assert_eq!(code, 404, "the label survived its own deletion");
}

/// The `label` porcelain takes **names** where the API takes ids, so `edit`, `delete` and `clone`
/// each begin with a listing whose result decides what the write addresses. A mock chooses that
/// listing's answer for itself, so it proves the calls happen in order and nothing about whether
/// the id it picked exists.
///
/// `clone` is the one command in the group that no single endpoint performs — a list plus one
/// create per label, with existing names skipped so that re-running it is safe. Both halves are
/// asserted: the copy lands, and a second run does not duplicate or overwrite.
#[test]
fn the_label_porcelain_resolves_names_to_ids_for_edit_delete_and_clone() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["label create", "label list", "label edit", "label delete", "label clone"],
        hits: ["issueCreateLabel", "issueListLabels", "issueEditLabel", "issueDeleteLabel"]
    );
    let source = TestRepo::create(inst, "label-src");
    let target = TestRepo::create(inst, "label-dst");

    for (name, color) in [("bug", "e11d21"), ("chore", "00ff00")] {
        inst.gea(["label", "create", name, "-c", color, "-R", &source.slug()])
            .assert_ok("gea label create");
    }
    // A label that already exists in the target under the same name, so `clone`'s skip path is
    // exercised rather than assumed.
    inst.gea(["label", "create", "bug", "-c", "111111", "-d", "mine", "-R", &target.slug()])
        .assert_ok("gea label create in the target");

    let listed = inst
        .gea(["label", "list", "-R", &source.slug(), "--json", "name,color"])
        .assert_ok("gea label list")
        .json();
    assert_eq!(names(&listed, "name"), ["bug", "chore"], "label list lost a label");

    // Rename by name: the command has to find `chore`'s id first.
    inst.gea([
        "label",
        "edit",
        "chore",
        "--name",
        "housekeeping",
        "-d",
        "tidy up",
        "-R",
        &source.slug(),
    ])
    .assert_ok("gea label edit");
    let (code, body) = source.api("GET", "labels", None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a label list");
    assert_eq!(
        names(&after, "name"),
        ["bug", "housekeeping"],
        "the rename landed on the wrong label, or not at all: {body}"
    );
    let renamed = after
        .as_array()
        .expect("a label list")
        .iter()
        .find(|l| l["name"] == "housekeeping")
        .expect("the renamed label");
    assert_eq!(renamed["description"], "tidy up", "the description was dropped by the rename");

    inst.gea(["label", "clone", &source.slug(), "-R", &target.slug()]).assert_ok("gea label clone");
    let (code, body) = target.api("GET", "labels", None);
    assert_eq!(code, 200, "{body}");
    let cloned: serde_json::Value = serde_json::from_str(&body).expect("a label list");
    assert_eq!(names(&cloned, "name"), ["bug", "housekeeping"], "clone did not copy every label");
    let kept = cloned
        .as_array()
        .expect("a label list")
        .iter()
        .find(|l| l["name"] == "bug")
        .expect("the pre-existing label");
    assert_eq!(
        kept["description"], "mine",
        "clone overwrote a label that already existed, which is what --overwrite is for"
    );

    inst.gea(["label", "delete", "housekeeping", "--yes", "-R", &source.slug()])
        .assert_ok("gea label delete");
    let (code, body) = source.api("GET", "labels", None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a label list");
    assert_eq!(names(&left, "name"), ["bug"], "delete removed the wrong label, or none: {body}");
}

// --------------------------------------------------------------------------- milestones

/// The layer-2 milestone lifecycle.
///
/// `edit-milestone` is a sparse patch like `edit-issue`, and the same failure applies: a field
/// the server does not recognise is answered `200` with nothing changed. `--state closed` is
/// included because closing a milestone is what makes it vanish from the default listing, which
/// is the behaviour the porcelain's `--state all` exists to work around.
#[test]
fn a_raw_milestone_can_be_created_read_edited_listed_and_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateMilestone",
        "issueGetMilestonesList",
        "issueGetMilestone",
        "issueEditMilestone",
        "issueDeleteMilestone"
    ]);
    let repo = TestRepo::create(inst, "raw-milestone");

    let made = inst
        .gea([
            "raw",
            "issue",
            "create-milestone",
            &repo.owner,
            &repo.name,
            "--title",
            "v1.0",
            "--description",
            "the first one",
        ])
        .assert_ok("gea raw issue create-milestone")
        .json();
    let id = made["id"].as_i64().expect("the milestone carries an id");

    let got = inst
        .gea(["raw", "issue", "get-milestone", &repo.owner, &repo.name, &id.to_string()])
        .assert_ok("gea raw issue get-milestone")
        .json();
    assert_eq!(got["title"], "v1.0", "get-milestone answered with a different milestone");

    inst.gea([
        "raw",
        "issue",
        "edit-milestone",
        &repo.owner,
        &repo.name,
        &id.to_string(),
        "--description",
        "the first stable release",
        "--state",
        "closed",
    ])
    .assert_ok("gea raw issue edit-milestone");
    let (code, body) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let stone: serde_json::Value = serde_json::from_str(&body).expect("a milestone");
    assert_eq!(stone["description"], "the first stable release", "the patch never landed");
    assert_eq!(stone["state"], "closed", "the state change never landed");
    assert_eq!(stone["title"], "v1.0", "a sparse patch rewrote a field it never named");

    // Now that it is closed, `--state open` must hide it. That is the proof `state` reached the
    // server as a query parameter rather than being dropped on the floor.
    //
    // Gitea's milestone listing defaults to `open` when `state` is absent — the unfiltered
    // listing does NOT contain a closed milestone (measured against 1.27.3; Forgejo does the
    // opposite and returns everything). So `all` is the listing that must contain it, and `open`
    // the one that must not; a dropped `state` parameter fails the first assertion.
    let every = inst
        .gea([
            "raw",
            "issue",
            "get-milestones-list",
            &repo.owner,
            &repo.name,
            "--state",
            "all",
            "--paginate",
        ])
        .assert_ok("gea raw issue get-milestones-list --state all")
        .json();
    assert!(
        every.as_array().is_some_and(|a| a.iter().any(|m| m["id"].as_i64() == Some(id))),
        "the milestone is missing from --state all, so the filter never reached the server: \
         {every}"
    );
    let open = inst
        .gea([
            "raw",
            "issue",
            "get-milestones-list",
            &repo.owner,
            &repo.name,
            "--state",
            "open",
            "--paginate",
        ])
        .assert_ok("gea raw issue get-milestones-list --state open")
        .json();
    assert!(
        !open.as_array().is_some_and(|a| a.iter().any(|m| m["id"].as_i64() == Some(id))),
        "a closed milestone appeared under --state open, so the filter never reached the \
         server: {open}"
    );

    inst.gea(["raw", "issue", "delete-milestone", &repo.owner, &repo.name, &id.to_string()])
        .assert_ok("gea raw issue delete-milestone");
    let (code, _) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 404, "the milestone survived its own deletion");
}

/// Every `milestone` subcommand takes a **title**, and Gitea happily allows two milestones with
/// the same one — so the group's title lookup searches open *and* closed milestones and refuses
/// an ambiguous match. Closing a milestone and then editing it by title is the sequence that
/// breaks the moment the lookup forgets `--state all`, and it is untestable against a mock,
/// which would return whatever list the test author wrote.
#[test]
fn the_milestone_porcelain_finds_closed_milestones_by_title() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "milestone create", "milestone list", "milestone edit",
            "milestone close", "milestone reopen", "milestone issues", "milestone delete"
        ],
        hits: [
            "issueCreateMilestone", "issueGetMilestonesList", "issueEditMilestone",
            "issueDeleteMilestone", "issueListIssues"
        ]
    );
    let repo = TestRepo::create(inst, "milestone-porcelain");

    inst.gea(["milestone", "create", "1.0", "--description", "first", "-R", &repo.slug()])
        .assert_ok("gea milestone create");

    let listed = inst
        .gea(["milestone", "list", "-R", &repo.slug(), "--json", "title,state"])
        .assert_ok("gea milestone list")
        .json();
    assert_eq!(names(&listed, "title"), ["1.0"], "milestone list lost the milestone");

    // An issue in the milestone, so `milestone issues` has something to find and the
    // milestone's own counters have something to count.
    let index = seed_issue(&repo, "in the milestone");
    let (code, body) = repo.api("GET", "milestones", None);
    assert_eq!(code, 200, "{body}");
    let stones: serde_json::Value = serde_json::from_str(&body).expect("a milestone list");
    let id = stones[0]["id"].as_i64().expect("the milestone id");
    let (code, body) =
        repo.api("PATCH", &format!("issues/{index}"), Some(&format!(r#"{{"milestone":{id}}}"#)));
    assert!((200..300).contains(&code), "attaching the milestone failed: HTTP {code}: {body}");

    let in_milestone = inst
        .gea(["milestone", "issues", "1.0", "-R", &repo.slug(), "--json", "number,title"])
        .assert_ok("gea milestone issues")
        .json();
    assert!(
        in_milestone
            .as_array()
            .is_some_and(|a| a.iter().any(|i| i["number"].as_i64() == Some(index))),
        "milestone issues did not find the issue attached to it: {in_milestone}"
    );

    inst.gea(["milestone", "close", "1.0", "-R", &repo.slug()]).assert_ok("gea milestone close");
    let (code, body) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let stone: serde_json::Value = serde_json::from_str(&body).expect("a milestone");
    assert_eq!(stone["state"], "closed", "milestone close did not close it: {body}");

    // The edit-by-title on a *closed* milestone: the lookup has to search closed ones too, or
    // this fails with "no milestone called 1.0" on a milestone that plainly exists.
    inst.gea([
        "milestone",
        "edit",
        "1.0",
        "--title",
        "1.0.1",
        "--description",
        "patched",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea milestone edit on a closed milestone");
    let (code, body) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let stone: serde_json::Value = serde_json::from_str(&body).expect("a milestone");
    assert_eq!(stone["title"], "1.0.1", "the retitle never landed: {body}");
    assert_eq!(stone["description"], "patched");
    assert_eq!(stone["state"], "closed", "editing a closed milestone reopened it");

    inst.gea(["milestone", "reopen", "1.0.1", "-R", &repo.slug()])
        .assert_ok("gea milestone reopen");
    let (code, body) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let stone: serde_json::Value = serde_json::from_str(&body).expect("a milestone");
    assert_eq!(stone["state"], "open", "milestone reopen did not reopen it: {body}");

    inst.gea(["milestone", "delete", "1.0.1", "--yes", "-R", &repo.slug()])
        .assert_ok("gea milestone delete");
    let (code, _) = repo.api("GET", &format!("milestones/{id}"), None);
    assert_eq!(code, 404, "the milestone survived `gea milestone delete`");
}

// ---------------------------------------------------------------------------- reactions

/// Issue reactions and comment reactions are **different collections on different routes**, and
/// both are addressed by a bare integer: `/issues/{index}/reactions` takes the number a human
/// sees, `/issues/comments/{id}/reactions` takes a database row id. A mix-up therefore does not
/// 404 — it reacts to a real, unrelated object — so this test deliberately reacts to *both* with
/// *different* contents and asserts neither collection contains the other's reaction.
///
/// Both layers are exercised on both targets: layer 2 with `+1`, the porcelain with `heart`.
/// Gitea allows one reaction per user per content, so using two contents is what lets one test
/// cover the add/list/remove cycle twice.
#[test]
fn issue_reactions_and_comment_reactions_stay_in_their_own_collections() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issuePostIssueReaction",
        "issueGetIssueReactions",
        "issueDeleteIssueReaction",
        "issuePostCommentReaction",
        "issueGetCommentReactions",
        "issueDeleteCommentReaction"
    ]);
    cover!(
        porcelain: ["reaction add", "reaction list", "reaction remove"],
        hits: [
            "issuePostIssueReaction", "issueGetIssueReactions", "issueDeleteIssueReaction",
            "issuePostCommentReaction", "issueGetCommentReactions", "issueDeleteCommentReaction"
        ]
    );
    let repo = TestRepo::create(inst, "reactions");
    let index = seed_issue(&repo, "react to me");
    burn_colliding_comment_ids(&repo, index);
    let comment_id = seed_comment(&repo, index, "and to me");
    let idx = index.to_string();
    let cid = comment_id.to_string();
    assert_ne!(comment_id, index, "the comment id equals the issue index, so a mix-up would hide");

    // Layer 2 on the issue.
    inst.gea([
        "raw",
        "issue",
        "post-issue-reaction",
        &repo.owner,
        &repo.name,
        &idx,
        "--content",
        "+1",
    ])
    .assert_ok("gea raw issue post-issue-reaction");
    // The porcelain on the issue, with a different content so both survive side by side.
    inst.gea(["reaction", "add", "heart", "--issue", &idx, "-R", &repo.slug()])
        .assert_ok("gea reaction add --issue");

    // Layer 2 on the comment, with contents chosen so that a route mix-up is visible: the
    // comment gets `rocket`, which the issue never receives.
    inst.gea([
        "raw",
        "issue",
        "post-comment-reaction",
        &repo.owner,
        &repo.name,
        &cid,
        "--content",
        "rocket",
    ])
    .assert_ok("gea raw issue post-comment-reaction");
    inst.gea(["reaction", "add", "eyes", "--comment", &cid, "-R", &repo.slug()])
        .assert_ok("gea reaction add --comment");

    let on_issue = inst
        .gea(["raw", "issue", "get-issue-reactions", &repo.owner, &repo.name, &idx, "--paginate"])
        .assert_ok("gea raw issue get-issue-reactions")
        .json();
    assert_eq!(
        names(&on_issue, "content"),
        ["+1", "heart"],
        "the issue's reactions are wrong — a comment reaction leaked in, or one never landed"
    );

    let on_comment = inst
        .gea(["raw", "issue", "get-comment-reactions", &repo.owner, &repo.name, &cid])
        .assert_ok("gea raw issue get-comment-reactions")
        .json();
    assert_eq!(
        names(&on_comment, "content"),
        ["eyes", "rocket"],
        "the comment's reactions are wrong — an issue reaction leaked in, or one never landed"
    );

    // The porcelain listing must agree with the endpoint it wraps, for both targets.
    let porcelain_issue = inst
        .gea(["reaction", "list", "--issue", &idx, "-R", &repo.slug(), "--json", "content"])
        .assert_ok("gea reaction list --issue")
        .json();
    assert_eq!(names(&porcelain_issue, "content"), ["+1", "heart"]);
    let porcelain_comment = inst
        .gea(["reaction", "list", "--comment", &cid, "-R", &repo.slug(), "--json", "content"])
        .assert_ok("gea reaction list --comment")
        .json();
    assert_eq!(names(&porcelain_comment, "content"), ["eyes", "rocket"]);

    // Removal, one per route and one per layer, then a read of each collection: a `DELETE` that
    // ignored its body would take away everything rather than the one content named.
    inst.gea([
        "raw",
        "issue",
        "delete-issue-reaction",
        &repo.owner,
        &repo.name,
        &idx,
        "--content",
        "+1",
    ])
    .assert_ok("gea raw issue delete-issue-reaction");
    inst.gea(["reaction", "remove", "eyes", "--comment", &cid, "-R", &repo.slug()])
        .assert_ok("gea reaction remove --comment");
    inst.gea([
        "raw",
        "issue",
        "delete-comment-reaction",
        &repo.owner,
        &repo.name,
        &cid,
        "--content",
        "rocket",
    ])
    .assert_ok("gea raw issue delete-comment-reaction");

    let (code, body) = repo.api("GET", &format!("issues/{index}/reactions"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a reaction list");
    assert_eq!(
        names(&left, "content"),
        ["heart"],
        "removing one reaction took the others with it: {body}"
    );

    inst.gea(["reaction", "remove", "heart", "--issue", &idx, "-R", &repo.slug()])
        .assert_ok("gea reaction remove --issue");
    let (code, body) = repo.api("GET", &format!("issues/{index}/reactions"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a reaction list");
    assert_eq!(count(&left), 0, "a reaction survived its removal: {body}");

    let (code, body) = repo.api("GET", &format!("issues/comments/{comment_id}/reactions"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a reaction list");
    assert_eq!(count(&left), 0, "a comment reaction survived: {body}");
}

// -------------------------------------------------------------------------- attachments

/// Issue attachments go up as `multipart/form-data`, which is the one body shape a mock cannot
/// meaningfully check: a boundary off by a byte, a missing `Content-Disposition`, or a body read
/// in one chunk when it should have been streamed all produce a request the server accepts and
/// stores *something* for.
///
/// So the bytes are compared after the round trip, and the payload is deliberately binary and
/// larger than one buffer so a chunking bug cannot hide in a short ASCII string.
#[test]
fn issue_attachment_bytes_survive_the_multipart_round_trip() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateIssueAttachment",
        "issueListIssueAttachments",
        "issueGetIssueAttachment",
        "issueEditIssueAttachment",
        "issueDeleteIssueAttachment"
    ]);
    let repo = TestRepo::create(inst, "issue-assets");
    let index = seed_issue(&repo, "with attachments");
    let idx = index.to_string();

    let scratch = Scratch::new("issueassets");
    // Not text, and past any single read buffer: a truncation at 8 KiB or 64 KiB shows up.
    let blob: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    // `.log`, not `.bin`. Gitea enforces `attachment.ALLOWED_TYPES`, whose default list is by
    // *extension*, and a `.bin` upload is refused with 422 "This file extension or type is not
    // allowed to be uploaded". A matching extension is the whole check, so the payload underneath
    // stays binary and the round trip still proves what it is here to prove.
    let path = scratch.file("payload.log", &blob);

    let made = inst
        .gea([
            "raw",
            "issue",
            "create-issue-attachment",
            &repo.owner,
            &repo.name,
            &idx,
            "--attachment",
            &path.to_string_lossy(),
            "--name",
            "payload.log",
        ])
        .assert_ok("gea raw issue create-issue-attachment")
        .json();
    let asset_id = made["id"].as_i64().expect("the attachment carries an id");
    assert_eq!(
        made["size"].as_u64(),
        Some(blob.len() as u64),
        "the server stored a different number of bytes than were sent, so the multipart body \
         was truncated or padded: {made}"
    );

    let listed = inst
        .gea(["raw", "issue", "list-issue-attachments", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue list-issue-attachments")
        .json();
    assert_eq!(
        names(&listed, "name"),
        ["payload.log"],
        "the attachment is missing from the listing"
    );

    let one = inst
        .gea([
            "raw",
            "issue",
            "get-issue-attachment",
            &repo.owner,
            &repo.name,
            &idx,
            &asset_id.to_string(),
        ])
        .assert_ok("gea raw issue get-issue-attachment")
        .json();
    let url = one["browser_download_url"].as_str().expect("a download URL").to_owned();

    // The assertion this test exists for: the stored bytes, not the stored length.
    let back = download(&url, &inst.token);
    assert_eq!(
        back.len(),
        blob.len(),
        "the download is {} bytes but {} were uploaded",
        back.len(),
        blob.len()
    );
    assert!(back == blob, "the downloaded bytes differ from the uploaded ones");

    inst.gea([
        "raw",
        "issue",
        "edit-issue-attachment",
        &repo.owner,
        &repo.name,
        &idx,
        &asset_id.to_string(),
        "--name",
        "renamed.log",
    ])
    .assert_ok("gea raw issue edit-issue-attachment");
    let (code, body) = repo.api("GET", &format!("issues/{index}/assets/{asset_id}"), None);
    assert_eq!(code, 200, "{body}");
    let asset: serde_json::Value = serde_json::from_str(&body).expect("an attachment");
    assert_eq!(asset["name"], "renamed.log", "the rename never landed: {body}");
    assert_eq!(
        asset["size"].as_u64(),
        Some(blob.len() as u64),
        "renaming an attachment changed its size, so the patch replaced the object"
    );

    inst.gea([
        "raw",
        "issue",
        "delete-issue-attachment",
        &repo.owner,
        &repo.name,
        &idx,
        &asset_id.to_string(),
    ])
    .assert_ok("gea raw issue delete-issue-attachment");
    let (code, _) = repo.api("GET", &format!("issues/{index}/assets/{asset_id}"), None);
    assert_eq!(code, 404, "the attachment survived its own deletion");
}

/// The same round trip on the *comment* attachment routes, which are a separate family keyed by
/// comment id rather than issue index — `/issues/comments/{id}/assets`. They are easy to wire to
/// the issue's routes by accident, and the mistake is silent: a comment id is a valid issue
/// index, so the upload succeeds and lands on the wrong object.
///
/// This test therefore asserts the attachment is on the *comment* and that the issue has none.
#[test]
fn comment_attachment_bytes_survive_the_multipart_round_trip() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateIssueCommentAttachment",
        "issueListIssueCommentAttachments",
        "issueGetIssueCommentAttachment",
        "issueEditIssueCommentAttachment",
        "issueDeleteIssueCommentAttachment"
    ]);
    let repo = TestRepo::create(inst, "comment-assets");
    let index = seed_issue(&repo, "commented on");
    // Without this the two route families can be given the same number, and "the upload landed
    // on the comment, not the issue" stops being a distinguishable claim.
    burn_colliding_comment_ids(&repo, index);
    let comment_id = seed_comment(&repo, index, "here is a file");
    let cid = comment_id.to_string();
    assert_ne!(comment_id, index, "the comment id equals the issue index, so a mix-up would hide");

    let scratch = Scratch::new("commentassets");
    let blob: Vec<u8> = (0..120_000u32).map(|i| ((i * 7) % 253) as u8).collect();
    // A `.log` name for the reason the issue-attachment test above records: Gitea's default
    // attachment allow-list is by extension.
    let path = scratch.file("comment.log", &blob);

    let made = inst
        .gea([
            "raw",
            "issue",
            "create-issue-comment-attachment",
            &repo.owner,
            &repo.name,
            &cid,
            "--attachment",
            &path.to_string_lossy(),
            "--name",
            "comment.log",
        ])
        .assert_ok("gea raw issue create-issue-comment-attachment")
        .json();
    let asset_id = made["id"].as_i64().expect("the attachment carries an id");

    let listed = inst
        .gea(["raw", "issue", "list-issue-comment-attachments", &repo.owner, &repo.name, &cid])
        .assert_ok("gea raw issue list-issue-comment-attachments")
        .json();
    assert_eq!(names(&listed, "name"), ["comment.log"], "the comment attachment is not listed");

    // It must be on the comment and *only* on the comment.
    let (code, body) = repo.api("GET", &format!("issues/{index}/assets"), None);
    assert_eq!(code, 200, "{body}");
    let on_issue: serde_json::Value = serde_json::from_str(&body).expect("an attachment list");
    assert_eq!(
        count(&on_issue),
        0,
        "the comment's attachment landed on the issue, so the two route families are crossed: {body}"
    );

    let one = inst
        .gea([
            "raw",
            "issue",
            "get-issue-comment-attachment",
            &repo.owner,
            &repo.name,
            &cid,
            &asset_id.to_string(),
        ])
        .assert_ok("gea raw issue get-issue-comment-attachment")
        .json();
    let url = one["browser_download_url"].as_str().expect("a download URL").to_owned();
    let back = download(&url, &inst.token);
    assert_eq!(back.len(), blob.len(), "the comment attachment came back a different length");
    assert!(back == blob, "the downloaded comment attachment differs from what was uploaded");

    inst.gea([
        "raw",
        "issue",
        "edit-issue-comment-attachment",
        &repo.owner,
        &repo.name,
        &cid,
        &asset_id.to_string(),
        "--name",
        "renamed-comment.log",
    ])
    .assert_ok("gea raw issue edit-issue-comment-attachment");
    let (code, body) = repo.api("GET", &format!("issues/comments/{comment_id}/assets"), None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("an attachment list");
    assert_eq!(names(&after, "name"), ["renamed-comment.log"], "the rename never landed: {body}");

    inst.gea([
        "raw",
        "issue",
        "delete-issue-comment-attachment",
        &repo.owner,
        &repo.name,
        &cid,
        &asset_id.to_string(),
    ])
    .assert_ok("gea raw issue delete-issue-comment-attachment");
    let (code, body) = repo.api("GET", &format!("issues/comments/{comment_id}/assets"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("an attachment list");
    assert_eq!(count(&left), 0, "the attachment survived deletion: {body}");
}

// -------------------------------------------------------------------------- tracked time

/// Tracked time, through both layers.
///
/// The API's unit is an integer number of seconds and the porcelain's is a Go-style duration, so
/// `times add 42 1h25m` has to arrive as `5100`. That conversion is the single most valuable
/// thing to check against a real server: a wrong multiplier still produces a valid request, a
/// `201`, and a timesheet that is quietly wrong by a factor of sixty.
///
/// `reset` is the destructive one — it drops *every* user's entries on the issue — so it is
/// driven last and confirmed by a listing rather than by its exit code.
#[test]
fn tracked_time_survives_the_round_trip_in_seconds() {
    let inst = instance_or_skip!();
    cover!(raw: ["issueAddTime", "issueTrackedTimes", "issueDeleteTime", "issueResetTime"]);
    cover!(
        porcelain: ["times add", "times list", "times delete", "times reset"],
        hits: [
            "issueAddTime", "issueTrackedTimes", "issueDeleteTime", "issueResetTime",
            "repoTrackedTimes"
        ]
    );
    let repo = TestRepo::create(inst, "tracked-time");
    let index = seed_issue(&repo, "timed work");
    let idx = index.to_string();

    let added = inst
        .gea(["raw", "issue", "add-time", &repo.owner, &repo.name, &idx, "--time", "3600"])
        .assert_ok("gea raw issue add-time")
        .json();
    let raw_entry = added["id"].as_i64().expect("the tracked-time entry carries an id");

    // The porcelain's duration parser, checked in the only place that can prove it: the value
    // the server stored.
    inst.gea(["times", "add", &idx, "1h25m", "-R", &repo.slug()]).assert_ok("gea times add");

    let entries = inst
        .gea(["raw", "issue", "tracked-times", &repo.owner, &repo.name, &idx, "--paginate"])
        .assert_ok("gea raw issue tracked-times")
        .json();
    let mut seconds: Vec<i64> = entries
        .as_array()
        .unwrap_or_else(|| panic!("tracked-times should answer with an array: {entries}"))
        .iter()
        .map(|t| t["time"].as_i64().unwrap_or_default())
        .collect();
    seconds.sort_unstable();
    assert_eq!(
        seconds,
        [3600, 5100],
        "1h25m did not arrive as 5100 seconds, so the porcelain's duration conversion is wrong"
    );

    // The repository-wide listing (`times list` with no issue) is a different endpoint from the
    // per-issue one, and it must see the same entries.
    let repo_wide = inst
        .gea(["times", "list", "-R", &repo.slug(), "--json", "id,time"])
        .assert_ok("gea times list for the whole repository")
        .json();
    let mut wide: Vec<i64> = repo_wide
        .as_array()
        .unwrap_or_else(|| panic!("times list should answer with an array: {repo_wide}"))
        .iter()
        .map(|t| t["time"].as_i64().unwrap_or_default())
        .collect();
    wide.sort_unstable();
    assert_eq!(wide, [3600, 5100], "the repository-wide listing disagrees with the issue's own");

    // One entry deleted by id through layer 2; the other must survive.
    inst.gea([
        "raw",
        "issue",
        "delete-time",
        &repo.owner,
        &repo.name,
        &idx,
        &raw_entry.to_string(),
    ])
    .assert_ok("gea raw issue delete-time");
    let (code, body) = repo.api("GET", &format!("issues/{index}/times"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    let left_seconds: Vec<i64> = left
        .as_array()
        .expect("an array")
        .iter()
        .map(|t| t["time"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(
        left_seconds,
        [5100],
        "delete-time removed the wrong entry, or more than the one it was given: {body}"
    );

    // And the porcelain's own delete, on the entry that is left.
    let remaining = left[0]["id"].as_i64().expect("the surviving entry has an id");
    inst.gea(["times", "add", &idx, "45s", "-R", &repo.slug()]).assert_ok("gea times add, again");
    inst.gea(["times", "delete", &idx, &remaining.to_string(), "--yes", "-R", &repo.slug()])
        .assert_ok("gea times delete");
    let (code, body) = repo.api("GET", &format!("issues/{index}/times"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    let left_seconds: Vec<i64> = left
        .as_array()
        .expect("an array")
        .iter()
        .map(|t| t["time"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(left_seconds, [45], "gea times delete removed the wrong entry: {body}");

    // `reset` through layer 2 clears what is left; then the porcelain's reset must be a no-op
    // that still succeeds rather than an error on an empty collection.
    inst.gea(["raw", "issue", "reset-time", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue reset-time");
    let (code, body) = repo.api("GET", &format!("issues/{index}/times"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(count(&left), 0, "reset-time left entries behind: {body}");

    inst.gea(["times", "add", &idx, "2h", "-R", &repo.slug()]).assert_ok("gea times add for reset");
    inst.gea(["times", "reset", &idx, "--yes", "-R", &repo.slug()]).assert_ok("gea times reset");
    let (code, body) = repo.api("GET", &format!("issues/{index}/times"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(count(&left), 0, "gea times reset left entries behind: {body}");
}

// ---------------------------------------------------------------------------- stopwatch

/// Every stopwatch assertion in this file lives in this one test, on purpose.
///
/// **A stopwatch is per user and instance-wide: at most one runs at a time.** Tests in this file
/// share one container *and* one admin account, and they run on parallel threads, so a second
/// test that started a timer would make this one fail — or, worse, pass against somebody else's
/// stopwatch. Keeping the whole lifecycle in a single test is what makes the invariant hold
/// without a mutex.
///
/// It is also why the test begins by cancelling anything already running: a previous failed run
/// that died between `start` and `stop` would otherwise poison every later run of this file.
///
/// The behaviour worth proving against a real server is that `stop` *records* the elapsed time
/// as a tracked-time entry while `cancel` throws it away. Those are two endpoints that both
/// answer `204`, so the only way to tell them apart is to read the tracked time afterwards.
#[test]
fn stopping_a_stopwatch_records_time_and_cancelling_one_does_not() {
    let inst = instance_or_skip!();
    cover!(raw: ["issueStartStopWatch", "issueStopStopWatch", "issueDeleteStopWatch"]);
    cover!(
        porcelain: ["stopwatch start", "stopwatch status", "stopwatch stop", "stopwatch cancel"],
        hits: [
            "issueStartStopWatch", "issueStopStopWatch", "issueDeleteStopWatch",
            "userGetStopWatches"
        ]
    );
    let repo = TestRepo::create(inst, "stopwatch");
    let first = seed_issue(&repo, "timed by layer two");
    let second = seed_issue(&repo, "timed by the porcelain");

    // Inherited state from a run that died mid-lifecycle. Best effort: nothing running is the
    // normal case and answers with an empty list.
    let (code, body) = inst.api("GET", "user/stopwatches", None);
    if code == 200 {
        let running: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(w) = running.as_array().and_then(|a| a.first()) {
            let (o, r, i) = (
                w["repo_owner_name"].as_str().unwrap_or_default(),
                w["repo_name"].as_str().unwrap_or_default(),
                w["issue_index"].as_i64().unwrap_or_default(),
            );
            let _ = inst.api("DELETE", &format!("repos/{o}/{r}/issues/{i}/stopwatch/delete"), None);
        }
    }

    // ---- layer 2: start, then stop, which must create a tracked-time entry.
    inst.gea(["raw", "issue", "start-stop-watch", &repo.owner, &repo.name, &first.to_string()])
        .assert_ok("gea raw issue start-stop-watch");
    let (code, body) = inst.api("GET", "user/stopwatches", None);
    assert_eq!(code, 200, "{body}");
    let running: serde_json::Value = serde_json::from_str(&body).expect("a stopwatch list");
    assert_eq!(
        running[0]["issue_index"].as_i64(),
        Some(first),
        "the stopwatch is not on the issue it was started on: {body}"
    );

    inst.gea(["raw", "issue", "stop-stop-watch", &repo.owner, &repo.name, &first.to_string()])
        .assert_ok("gea raw issue stop-stop-watch");
    let (code, body) = repo.api("GET", &format!("issues/{first}/times"), None);
    assert_eq!(code, 200, "{body}");
    let times: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(
        count(&times),
        1,
        "stopping the timer did not record a tracked-time entry, so `stop` behaved like \
         `cancel`: {body}"
    );

    // ---- layer 2: start, then delete, which must record nothing.
    inst.gea(["raw", "issue", "start-stop-watch", &repo.owner, &repo.name, &second.to_string()])
        .assert_ok("gea raw issue start-stop-watch on the second issue");
    inst.gea(["raw", "issue", "delete-stop-watch", &repo.owner, &repo.name, &second.to_string()])
        .assert_ok("gea raw issue delete-stop-watch");
    let (code, body) = repo.api("GET", &format!("issues/{second}/times"), None);
    assert_eq!(code, 200, "{body}");
    let times: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(
        count(&times),
        0,
        "deleting the timer recorded time anyway, so `delete` behaved like `stop`: {body}"
    );

    // ---- the porcelain, which adds the read-before-write that names the issue already timed.
    inst.gea(["stopwatch", "start", &second.to_string(), "-R", &repo.slug()])
        .assert_ok("gea stopwatch start");

    let status = inst
        .gea(["stopwatch", "status", "--json", "issue_index,issue_title,seconds"])
        .assert_ok("gea stopwatch status")
        .json();
    assert_eq!(
        status["issue_index"].as_i64(),
        Some(second),
        "stopwatch status reports a different issue from the one started: {status}"
    );

    // Starting a second timer must be refused with the issue that holds it named, rather than
    // with an unexplained conflict — and it must not move the running timer.
    let clash = inst.gea(["stopwatch", "start", &first.to_string(), "-R", &repo.slug()]);
    assert!(
        !clash.ok(),
        "a second stopwatch should not start:\n{}\n{}",
        clash.stdout,
        clash.stderr
    );
    clash.assert_says("already running");
    let (code, body) = inst.api("GET", "user/stopwatches", None);
    assert_eq!(code, 200, "{body}");
    let running: serde_json::Value = serde_json::from_str(&body).expect("a stopwatch list");
    assert_eq!(
        running[0]["issue_index"].as_i64(),
        Some(second),
        "the refused start moved the running timer: {body}"
    );

    inst.gea(["stopwatch", "cancel", "--yes"]).assert_ok("gea stopwatch cancel");
    let (code, body) = repo.api("GET", &format!("issues/{second}/times"), None);
    assert_eq!(code, 200, "{body}");
    let times: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(
        count(&times),
        0,
        "gea stopwatch cancel recorded the time it was supposed to discard: {body}"
    );

    // And `stop` through the porcelain, which works with no repository named because the server
    // already knows which issue is being timed.
    inst.gea(["stopwatch", "start", &second.to_string(), "-R", &repo.slug()])
        .assert_ok("gea stopwatch start, again");
    inst.gea(["stopwatch", "stop"]).assert_ok("gea stopwatch stop with no repository");
    let (code, body) = repo.api("GET", &format!("issues/{second}/times"), None);
    assert_eq!(code, 200, "{body}");
    let times: serde_json::Value = serde_json::from_str(&body).expect("a tracked-time list");
    assert_eq!(count(&times), 1, "gea stopwatch stop did not record the elapsed time: {body}");

    // Nothing running is not an error: a script asking "am I timing anything?" reads the
    // output, not the exit code.
    inst.gea(["stopwatch", "status"]).assert_ok("gea stopwatch status with nothing running");
}

// ------------------------------------------------------------------------- dependencies

/// `/issues/{i}/dependencies` holds what must be finished **before** `i`; `/issues/{i}/blocks`
/// holds what is waiting **on** `i`. Two endpoints, the same request body, the same response
/// shape — which is exactly the pair where a wiring mistake looks fine from either side alone.
///
/// The removals matter as much as the additions: `DELETE` on these routes carries a *body*
/// naming the other issue, and a body that never arrives deletes nothing while answering `200`.
#[test]
fn issue_dependencies_and_blocks_are_written_and_removed_independently() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueCreateIssueDependencies",
        "issueListIssueDependencies",
        "issueRemoveIssueDependencies",
        "issueCreateIssueBlocking",
        "issueListBlocks",
        "issueRemoveIssueBlocking"
    ]);
    cover!(porcelain: ["issue depends remove"], hits: ["issueRemoveIssueDependencies"]);
    let repo = TestRepo::create(inst, "raw-depends");
    let subject = seed_issue(&repo, "the subject");
    let blocker = seed_issue(&repo, "the blocker");
    let blocked = seed_issue(&repo, "the blocked");
    let subj = subject.to_string();

    inst.gea([
        "raw",
        "issue",
        "create-issue-dependencies",
        &repo.owner,
        &repo.name,
        &subj,
        "--body-owner",
        &repo.owner,
        "--body-repo",
        &repo.name,
        "--body-index",
        &blocker.to_string(),
    ])
    .assert_ok("gea raw issue create-issue-dependencies");
    inst.gea([
        "raw",
        "issue",
        "create-issue-blocking",
        &repo.owner,
        &repo.name,
        &subj,
        "--body-owner",
        &repo.owner,
        "--body-repo",
        &repo.name,
        "--body-index",
        &blocked.to_string(),
    ])
    .assert_ok("gea raw issue create-issue-blocking");

    let deps = inst
        .gea([
            "raw",
            "issue",
            "list-issue-dependencies",
            &repo.owner,
            &repo.name,
            &subj,
            "--paginate",
        ])
        .assert_ok("gea raw issue list-issue-dependencies")
        .json();
    assert_eq!(
        names(&deps, "title"),
        ["the blocker"],
        "what blocks the subject is wrong — the two directions are crossed: {deps}"
    );

    let blocks = inst
        .gea(["raw", "issue", "list-blocks", &repo.owner, &repo.name, &subj, "--paginate"])
        .assert_ok("gea raw issue list-blocks")
        .json();
    assert_eq!(
        names(&blocks, "title"),
        ["the blocked"],
        "what the subject blocks is wrong — the two directions are crossed: {blocks}"
    );

    // Removing the blocking edge must leave the dependency edge alone.
    inst.gea([
        "raw",
        "issue",
        "remove-issue-blocking",
        &repo.owner,
        &repo.name,
        &subj,
        "--body-owner",
        &repo.owner,
        "--body-repo",
        &repo.name,
        "--body-index",
        &blocked.to_string(),
    ])
    .assert_ok("gea raw issue remove-issue-blocking");
    let (code, body) = repo.api("GET", &format!("issues/{subject}/blocks"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("an issue list");
    assert_eq!(count(&left), 0, "the blocking edge survived removal: {body}");
    let (code, body) = repo.api("GET", &format!("issues/{subject}/dependencies"), None);
    assert_eq!(code, 200, "{body}");
    let still: serde_json::Value = serde_json::from_str(&body).expect("an issue list");
    assert_eq!(
        names(&still, "title"),
        ["the blocker"],
        "removing a blocking edge also removed a dependency: {body}"
    );

    // The porcelain's removal, on the edge that is left. `--blocked-by` is the direction that
    // reaches `/dependencies`, which is what makes this cover the same endpoint from layer 3.
    inst.gea([
        "issue",
        "depends",
        "remove",
        &subj,
        "--blocked-by",
        &blocker.to_string(),
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea issue depends remove --blocked-by");
    let (code, body) = repo.api("GET", &format!("issues/{subject}/dependencies"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("an issue list");
    assert_eq!(count(&left), 0, "the dependency survived removal: {body}");

    // Layer 2's own removal, on an edge recreated for it, so the operation is genuinely driven
    // rather than credited from the porcelain's call.
    inst.gea([
        "raw",
        "issue",
        "create-issue-dependencies",
        &repo.owner,
        &repo.name,
        &subj,
        "--body-owner",
        &repo.owner,
        "--body-repo",
        &repo.name,
        "--body-index",
        &blocker.to_string(),
    ])
    .assert_ok("gea raw issue create-issue-dependencies, again");
    inst.gea([
        "raw",
        "issue",
        "remove-issue-dependencies",
        &repo.owner,
        &repo.name,
        &subj,
        "--body-owner",
        &repo.owner,
        "--body-repo",
        &repo.name,
        "--body-index",
        &blocker.to_string(),
    ])
    .assert_ok("gea raw issue remove-issue-dependencies");
    let (code, body) = repo.api("GET", &format!("issues/{subject}/dependencies"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("an issue list");
    assert_eq!(count(&left), 0, "remove-issue-dependencies did nothing: {body}");
}

// ------------------------------------------------------------------------ subscriptions

/// Subscriptions have an asymmetry that only a real server exposes: `check` answers for the
/// *authenticated* user with no user in the path, while `add`/`delete` name a user in the path
/// and need issue-manager rights. Three shapes on one collection, and a mock would agree with
/// whichever one the test author wrote down.
///
/// A second account is minted because the interesting case is subscribing somebody else:
/// Gitea subscribes the author automatically, so a test that only watches its own
/// subscription cannot tell "the write worked" from "it was already true".
#[test]
fn subscribing_another_user_to_an_issue_is_visible_and_reversible() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueAddSubscription",
        "issueCheckSubscription",
        "issueSubscriptions",
        "issueDeleteSubscription"
    ]);
    let Ok(watcher) = inst.scoped_user("watcher", &["all"]) else {
        println!("SKIPPED: could not mint a second account to subscribe");
        return;
    };
    let repo = TestRepo::create(inst, "subscriptions");
    let index = seed_issue(&repo, "watch me");
    let idx = index.to_string();

    // The author is subscribed automatically, so this is the control: `check` must say so.
    let before = inst
        .gea(["raw", "issue", "check-subscription", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue check-subscription")
        .json();
    assert_eq!(
        before["subscribed"].as_bool(),
        Some(true),
        "the issue's author is not reported as subscribed, so `check` is not answering for the \
         authenticated user: {before}"
    );

    inst.gea(["raw", "issue", "add-subscription", &repo.owner, &repo.name, &idx, &watcher.name])
        .assert_ok("gea raw issue add-subscription");
    let watchers = inst
        .gea(["raw", "issue", "subscriptions", &repo.owner, &repo.name, &idx, "--paginate"])
        .assert_ok("gea raw issue subscriptions")
        .json();
    assert!(
        watchers
            .as_array()
            .is_some_and(|a| a.iter().any(|u| u["login"].as_str() == Some(watcher.name.as_str()))),
        "the user just subscribed is missing from the subscriber list: {watchers}"
    );

    inst.gea(["raw", "issue", "delete-subscription", &repo.owner, &repo.name, &idx, &watcher.name])
        .assert_ok("gea raw issue delete-subscription");
    let (code, body) = repo.api("GET", &format!("issues/{index}/subscriptions"), None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a subscriber list");
    assert!(
        !left
            .as_array()
            .is_some_and(|a| a.iter().any(|u| u["login"].as_str() == Some(watcher.name.as_str()))),
        "the unsubscribed user is still watching the issue: {body}"
    );

    // Unsubscribing the author must also work, and `check` must then say false — which is the
    // half that proves `check` reads state rather than returning a constant.
    inst.gea(["raw", "issue", "delete-subscription", &repo.owner, &repo.name, &idx, &inst.user])
        .assert_ok("gea raw issue delete-subscription for the author");
    let after = inst
        .gea(["raw", "issue", "check-subscription", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue check-subscription after unsubscribing")
        .json();
    assert_eq!(
        after["subscribed"].as_bool(),
        Some(false),
        "check-subscription still reports a subscription that was deleted: {after}"
    );
}

// --------------------------------------------------------------------- pinning, deadline

/// The pinning *success* path, and the position arithmetic that goes with it.
///
/// `porcelain.rs` covers the refusal of `--position 0` — that nothing is written when the flag
/// is rejected. What is left, and what this covers, is that pinning actually pins, that
/// `move-issue-pin` reorders rather than re-pinning, and that unpinning returns `pin_order` to
/// zero. All three endpoints answer with no body, so every assertion here has to come from a
/// fresh read.
#[test]
fn pinning_two_issues_and_reordering_them_changes_their_pin_order() {
    let inst = instance_or_skip!();
    cover!(raw: ["pinIssue", "moveIssuePin", "unpinIssue"]);
    cover!(porcelain: ["issue pin", "issue unpin"], hits: ["pinIssue", "moveIssuePin", "unpinIssue"]);
    let repo = TestRepo::create(inst, "pinning");
    let first = seed_issue(&repo, "pinned first");
    let second = seed_issue(&repo, "pinned second");

    inst.gea(["raw", "issue", "pin-issue", &repo.owner, &repo.name, &first.to_string()])
        .assert_ok("gea raw issue pin-issue");
    inst.gea(["raw", "issue", "pin-issue", &repo.owner, &repo.name, &second.to_string()])
        .assert_ok("gea raw issue pin-issue, second");
    assert_eq!(
        read_issue(&repo, first)["pin_order"].as_i64(),
        Some(1),
        "the first issue pinned did not land at position 1"
    );
    assert_eq!(
        read_issue(&repo, second)["pin_order"].as_i64(),
        Some(2),
        "the second issue pinned did not land at position 2"
    );

    // Moving the second to the front must demote the first, rather than leaving two issues
    // claiming the same position.
    inst.gea(["raw", "issue", "move-issue-pin", &repo.owner, &repo.name, &second.to_string(), "1"])
        .assert_ok("gea raw issue move-issue-pin");
    assert_eq!(
        read_issue(&repo, second)["pin_order"].as_i64(),
        Some(1),
        "move-issue-pin did not move the pin"
    );
    assert_eq!(
        read_issue(&repo, first)["pin_order"].as_i64(),
        Some(2),
        "moving one pin left another issue at the same position"
    );

    inst.gea(["raw", "issue", "unpin-issue", &repo.owner, &repo.name, &first.to_string()])
        .assert_ok("gea raw issue unpin-issue");
    assert_eq!(
        read_issue(&repo, first)["pin_order"].as_i64(),
        Some(0),
        "unpin-issue left the issue pinned"
    );

    // The porcelain re-pins it and moves it in one invocation — two calls behind one command,
    // which is what earns `issue pin` its place over `gea raw`.
    inst.gea(["issue", "pin", &first.to_string(), "--position", "1", "-R", &repo.slug()])
        .assert_ok("gea issue pin --position 1");
    assert_eq!(
        read_issue(&repo, first)["pin_order"].as_i64(),
        Some(1),
        "gea issue pin --position did not pin and move in one go"
    );

    // Pinning something already pinned is a 400 from Gitea; the porcelain reads first so the
    // ordinary "pin it again at a new position" invocation works.
    inst.gea(["issue", "pin", &first.to_string(), "--position", "2", "-R", &repo.slug()])
        .assert_ok("gea issue pin on an already-pinned issue");
    assert_eq!(read_issue(&repo, first)["pin_order"].as_i64(), Some(2));

    inst.gea(["issue", "unpin", &first.to_string(), "-R", &repo.slug()])
        .assert_ok("gea issue unpin");
    assert_eq!(
        read_issue(&repo, first)["pin_order"].as_i64(),
        Some(0),
        "gea issue unpin did nothing"
    );
}

/// The deadline is a `POST` to its own sub-resource rather than a field of `edit-issue`, and its
/// body carries a timestamp. A timestamp serialised in the wrong format is the classic silent
/// failure here: Gitea takes only the date part and ignores the time of day, so an encoding
/// that is off by a timezone still parses and lands on the wrong day.
#[test]
fn setting_an_issue_deadline_stores_the_date_it_was_given() {
    let inst = instance_or_skip!();
    cover!(raw: ["issueEditIssueDeadline"]);
    let repo = TestRepo::create(inst, "deadline");
    let index = seed_issue(&repo, "due eventually");

    let before = read_issue(&repo, index);
    assert!(before["due_date"].is_null(), "a fresh issue already has a deadline: {before}");

    inst.gea([
        "raw",
        "issue",
        "edit-issue-deadline",
        &repo.owner,
        &repo.name,
        &index.to_string(),
        "--due-date",
        "2030-06-15T00:00:00Z",
    ])
    .assert_ok("gea raw issue edit-issue-deadline");

    let after = read_issue(&repo, index);
    let due =
        after["due_date"].as_str().unwrap_or_else(|| panic!("no deadline was stored: {after}"));
    assert!(
        due.starts_with("2030-06-15"),
        "the deadline came back as {due}, not the date it was given — the timestamp encoding is \
         wrong by at least a timezone"
    );
}

// --------------------------------------------------------------- the issue porcelain

/// The `issue` porcelain's own lifecycle, which is several calls per command.
///
/// `create` resolves label and milestone names to ids (covered in `porcelain.rs`); what is left,
/// and what this drives, is the rest of the group: `--body-file`, listing with filters, viewing,
/// `edit --add-label/--remove-label` — which deliberately use the label endpoints rather than
/// `PUT /labels`, because a replace would silently drop labels the caller never mentioned —
/// `close -c`, `reopen`, `comment --edit`, and `delete`.
///
/// Every one of those is read back, because the failure this guards against is a command that
/// reports success having sent a patch the server ignored.
#[test]
fn the_issue_porcelain_drives_a_whole_issue_from_body_file_to_deletion() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "issue create", "issue list", "issue view", "issue edit",
            "issue close", "issue reopen", "issue comment", "issue delete"
        ],
        hits: [
            "issueCreateIssue", "issueListIssues", "issueGetIssue", "issueEditIssue",
            "issueCreateComment", "issueGetComments", "issueEditComment",
            "issueAddLabel", "issueRemoveLabel", "issueDelete"
        ]
    );
    let repo = TestRepo::create(inst, "issue-porcelain");
    let scratch = Scratch::new("issuebody");
    let body_file = scratch.file("body.md", b"the body came from a file\n");

    for name in ["bug", "chore"] {
        let (code, body) =
            repo.api("POST", "labels", Some(&format!(r#"{{"name":"{name}","color":"00ff00"}}"#)));
        assert!((200..300).contains(&code), "seeding label {name} failed: HTTP {code}: {body}");
    }

    // `-F` reads the body from disk; `-a @me` resolves the caller's own login.
    inst.gea_in(
        scratch.path(),
        [
            "issue",
            "create",
            "--title",
            "from a file",
            "-F",
            &body_file.to_string_lossy(),
            "-a",
            "@me",
            "-R",
            &repo.slug(),
        ],
    )
    .assert_ok("gea issue create -F --assignee @me");

    let (code, body) = repo.api("GET", "issues?state=all", None);
    assert_eq!(code, 200, "{body}");
    let issues: serde_json::Value = serde_json::from_str(&body).expect("an issue list");
    let index = issues[0]["number"].as_i64().expect("the created issue has a number");
    let idx = index.to_string();
    // `issue comment --edit` takes a comment id where every other subcommand takes an issue
    // index; see `burn_colliding_comment_ids` for why the two must not be the same number here.
    burn_colliding_comment_ids(&repo, index);
    let issue = read_issue(&repo, index);
    assert_eq!(
        issue["body"].as_str().map(str::trim),
        Some("the body came from a file"),
        "the body file never reached the server: {issue}"
    );
    assert_eq!(
        issue["assignees"][0]["login"].as_str(),
        Some(inst.user.as_str()),
        "'@me' did not resolve to the authenticated user: {issue}"
    );

    let listed = inst
        .gea(["issue", "list", "-R", &repo.slug(), "--json", "number,title"])
        .assert_ok("gea issue list")
        .json();
    assert!(
        listed.as_array().is_some_and(|a| a.iter().any(|i| i["number"].as_i64() == Some(index))),
        "gea issue list did not find the issue: {listed}"
    );

    let viewed = inst.gea(["issue", "view", &idx, "-R", &repo.slug()]);
    viewed.assert_ok("gea issue view");
    viewed.assert_says("from a file");

    // `--add-label` twice then `--remove-label` once. A `PUT /labels` implementation would pass
    // the first assertion and fail the second by dropping the label it was not given.
    inst.gea([
        "issue",
        "edit",
        &idx,
        "--title",
        "renamed by edit",
        "--add-label",
        "bug",
        "--add-label",
        "chore",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea issue edit --add-label");
    let issue = read_issue(&repo, index);
    assert_eq!(issue["title"], "renamed by edit", "the title patch never landed: {issue}");
    assert_eq!(names(&issue["labels"], "name"), ["bug", "chore"], "both labels should be attached");

    inst.gea(["issue", "edit", &idx, "--remove-label", "bug", "-R", &repo.slug()])
        .assert_ok("gea issue edit --remove-label");
    assert_eq!(
        names(&read_issue(&repo, index)["labels"], "name"),
        ["chore"],
        "--remove-label removed the wrong label, or replaced the whole set"
    );

    // `close -c` comments first and closes second, because closing and then failing to explain
    // why is the worse order. Both halves are checked.
    inst.gea(["issue", "close", &idx, "-c", "not reproducible", "-R", &repo.slug()])
        .assert_ok("gea issue close -c");
    let issue = read_issue(&repo, index);
    assert_eq!(issue["state"], "closed", "gea issue close did not close the issue: {issue}");
    let (code, body) = repo.api("GET", &format!("issues/{index}/comments"), None);
    assert_eq!(code, 200, "{body}");
    let comments: serde_json::Value = serde_json::from_str(&body).expect("a comment list");
    assert_eq!(
        names(&comments, "body"),
        ["not reproducible"],
        "the closing comment was not posted: {body}"
    );
    let comment_id = comments[0]["id"].as_i64().expect("the comment has an id");

    inst.gea(["issue", "reopen", &idx, "-R", &repo.slug()]).assert_ok("gea issue reopen");
    assert_eq!(read_issue(&repo, index)["state"], "open", "gea issue reopen did not reopen it");

    inst.gea(["issue", "comment", &idx, "-b", "a second thought", "-R", &repo.slug()])
        .assert_ok("gea issue comment");
    // `--edit` takes a *comment id*, not an issue index: `/issues/comments/{id}` is a different
    // route, and passing one where the other belongs edits a real comment on another issue.
    inst.gea([
        "issue",
        "comment",
        &idx,
        "--edit",
        &comment_id.to_string(),
        "-b",
        "amended by --edit",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea issue comment --edit");
    let (code, body) = repo.api("GET", &format!("issues/{index}/comments"), None);
    assert_eq!(code, 200, "{body}");
    let comments: serde_json::Value = serde_json::from_str(&body).expect("a comment list");
    assert_eq!(
        names(&comments, "body"),
        ["a second thought", "amended by --edit"],
        "`--edit` amended the wrong comment, or added one instead: {body}"
    );

    // `issue view -c` fetches the issue and its comments together; both must reach the output.
    let with_comments = inst.gea(["issue", "view", &idx, "-c", "-R", &repo.slug()]);
    with_comments.assert_ok("gea issue view -c");
    with_comments.assert_says("amended by --edit");

    inst.gea(["issue", "delete", &idx, "--yes", "-R", &repo.slug()])
        .assert_ok("gea issue delete --yes");
    let (code, body) = repo.api("GET", &format!("issues/{index}"), None);
    assert_eq!(code, 404, "the issue survived `gea issue delete`: HTTP {code}: {body}");
}

/// Assignees through their own routes, and locking.
///
/// The assignee routes are additive (`POST`) and subtractive (`DELETE`) on a set, where the issue
/// `PATCH` replaces it — the distinction a user relies on to avoid unassigning a colleague. The
/// check routes answer `204` for "may be assigned" with no body, which is what they assert.
///
/// A lock is only visible as `is_locked` on the issue, so it is read back out of band both ways.
#[test]
fn assignees_are_added_checked_and_removed_and_an_issue_locks_and_unlocks() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "issueAddAssignees",
        "issueCheckAssignee",
        "repoCheckAssignee",
        "issueRemoveAssignees",
        "issueLockIssue",
        "issueUnlockIssue",
    ]);
    let repo = TestRepo::create(inst, "assign-lock");
    let index = seed_issue(&repo, "to be assigned and locked");
    let idx = index.to_string();
    let me = inst.user.as_str();

    inst.gea(["raw", "repo", "check-assignee", &repo.owner, &repo.name, me])
        .assert_ok("gea raw repo check-assignee, for the owner");
    // On Gitea this answers whether the user *may be* assigned to the issue — 204 before any
    // assignment too (measured, 1.27.3) — so it is the issue itself that shows the assignment.
    inst.gea(["raw", "issue", "check-assignee", &repo.owner, &repo.name, &idx, me])
        .assert_ok("gea raw issue check-assignee before assigning");

    inst.gea(["raw", "issue", "add-assignees", &repo.owner, &repo.name, &idx, "--assignees", me])
        .assert_ok("gea raw issue add-assignees");
    assert_eq!(names(&read_issue(&repo, index)["assignees"], "login"), [me]);
    inst.gea(["raw", "issue", "check-assignee", &repo.owner, &repo.name, &idx, me])
        .assert_ok("gea raw issue check-assignee after assigning");

    inst.gea([
        "raw",
        "issue",
        "remove-assignees",
        &repo.owner,
        &repo.name,
        &idx,
        "--assignees",
        me,
    ])
    .assert_ok("gea raw issue remove-assignees");
    assert_eq!(count(&read_issue(&repo, index)["assignees"]), 0, "the assignee survived removal");

    inst.gea([
        "raw",
        "issue",
        "lock-issue",
        &repo.owner,
        &repo.name,
        &idx,
        "--lock-reason",
        "Resolved",
    ])
    .assert_ok("gea raw issue lock-issue");
    assert_eq!(read_issue(&repo, index)["is_locked"], true, "the lock did not land");
    inst.gea(["raw", "issue", "unlock-issue", &repo.owner, &repo.name, &idx])
        .assert_ok("gea raw issue unlock-issue");
    assert_eq!(read_issue(&repo, index)["is_locked"], false, "the unlock did not land");
}

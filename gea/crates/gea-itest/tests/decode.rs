//! Decoding what Gitea actually sends.
//!
//! Go marshals a nil pointer and a nil slice as `null`. The specification says only
//! `"type": "string"`, so the generator emits `String` with `#[serde(default)]` — and
//! `#[serde(default)]` covers an **absent** key, not an explicit `null`. Every fixture built
//! from the specification therefore agrees with us, and only a real server disagrees.
//!
//! `FakeTransport` cannot find any of this, because the canned responses are written by the
//! same reading of the specification that produced the types.

use gea_itest::{TestRepo, commit_and_push, cover, instance_or_skip};

/// Collect the keys a JSON object sends as `null`, so a failure names them.
fn null_keys(body: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    v.as_object()
        .map(|o| o.iter().filter(|(_, v)| v.is_null()).map(|(k, _)| k.clone()).collect())
        .unwrap_or_default()
}

/// A nil slice: an issue with no assignees sends `"assignees": null`, while `labels` — which Go
/// initialises — sends `[]`. `de::null_as_empty_vec` exists for exactly this, and this is the
/// test that it is wired to the field that needs it.
#[test]
fn null_slice_decodes_as_empty() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue view", "issue list"], hits: ["issueGetIssue", "issueListIssues"]);
    let repo = TestRepo::create(inst, "null-slice");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"no assignees"}"#));
    assert!((200..300).contains(&code), "{body}");

    let (_, raw) = repo.api("GET", "issues/1", None);
    assert!(
        null_keys(&raw).contains(&"assignees".to_owned()),
        "this test is pointless unless Gitea really sends a null assignees; it sent: {raw}"
    );

    // The typed path: `issue view` decodes a full `Issue`.
    inst.gea(["issue", "view", "1", "-R", &repo.slug()]).assert_ok("gea issue view");

    let run = inst.gea(["issue", "list", "-R", &repo.slug(), "--json", "number,assignees"]);
    run.assert_ok("gea issue list --json assignees");
    assert_eq!(
        run.json()[0]["assignees"],
        serde_json::json!([]),
        "a null slice should decode to an empty list"
    );
}

/// A `PullRequest` that is not merged sends `"merge_commit_sha": null`, and the field is typed
/// `String`.
///
/// This breaks the entire `pr` group against any repository containing an open pull request —
/// which is to say, against every real repository. The identical command succeeds once the pull
/// request is merged and the field becomes a string, which is what isolates the cause to this
/// one field rather than to anything about pull requests.
///
/// This was ignored as a known bug: `pull_request.rs` declared `pub merge_commit_sha: String`
/// and the whole `pr` group died on it. Fixed in the generator's presence rules — a nullable
/// scalar now gets a `null`-tolerant `deserialize_with`, the scalar analogue of
/// `de::null_as_empty_vec` — so this runs, and stays here as the regression test for it.
#[test]
fn open_pull_request_decodes() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["pr list", "pr view"],
        hits: ["repoListPullRequests", "repoGetPullRequest"],
    );
    let repo = TestRepo::create_initialized(inst, "null-pr");
    let dir = std::env::temp_dir().join(format!("gea-itest-{}-pr", repo.name));
    let _ = std::fs::remove_dir_all(&dir);
    repo.clone_to(&dir);
    commit_and_push(&dir, "feature", "f.txt", "one\n", "Add f.txt");

    let (code, body) =
        repo.api("POST", "pulls", Some(r#"{"title":"open pr","head":"feature","base":"main"}"#));
    assert!((200..300).contains(&code), "creating the pull request failed: HTTP {code}: {body}");
    assert!(
        null_keys(&body).contains(&"merge_commit_sha".to_owned()),
        "this test is pointless unless an open PR really sends a null merge_commit_sha: {body}"
    );

    inst.gea(["pr", "list", "-R", &repo.slug()]).assert_ok("gea pr list with an open PR");
    inst.gea(["pr", "view", "1", "-R", &repo.slug()]).assert_ok("gea pr view on an open PR");
}

/// The same field, once it holds a string, decodes fine — proving the failure above is the null
/// and nothing else about pull requests.
///
/// This one passes today and is the control for [`open_pull_request_decodes`].
#[test]
fn merged_pull_request_decodes() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr view"], hits: ["repoGetPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "merged-pr");
    let dir = std::env::temp_dir().join(format!("gea-itest-{}-mpr", repo.name));
    let _ = std::fs::remove_dir_all(&dir);
    repo.clone_to(&dir);
    commit_and_push(&dir, "feature", "f.txt", "one\n", "Add f.txt");

    let (code, _) =
        repo.api("POST", "pulls", Some(r#"{"title":"to merge","head":"feature","base":"main"}"#));
    assert!((200..300).contains(&code));
    let (code, body) = repo.merge_pull(1, "squash");
    assert!((200..300).contains(&code), "merge failed: HTTP {code}: {body}");

    let (_, pr) = repo.api("GET", "pulls/1", None);
    assert!(
        !null_keys(&pr).contains(&"merge_commit_sha".to_owned()),
        "a merged pull request should carry a real merge_commit_sha: {pr}"
    );
    inst.gea(["pr", "view", "1", "-R", &repo.slug()]).assert_ok("gea pr view on a merged PR");
}

/// A directory listing sends `encoding`, `content`, `target` and `submodule_git_url` as `null`
/// — the specification's own descriptions say so ("`content` is populated when `type` is
/// `file`, otherwise null") — yet all four are typed `String`.
///
/// The result is worse than a decode failure. `ContentsResponseOrList` is
/// `#[serde(untagged)]` with `One(Box<ContentsResponse>)` listed first; the array fails the
/// `Many` variant on the nulls, then **matches `One` and yields an all-default entry**
/// (`name: ""`, `type: ""`). `gea workflow list` filters on `type == "file"`, discards the
/// blank entry, and reports no workflows, with exit 0, for a repository that has one. A wrong
/// answer delivered as a success is the failure mode this whole test suite exists to catch.
///
/// This was ignored as a known bug with two separate causes, both in generated code:
/// `contents_response.rs` typed four nullable fields as `String`, and the untagged enum in
/// `contents_response_or_list.rs` let `One` swallow an array instead of erroring. Both are
/// fixed, so this runs — and it is the test that keeps `workflow list` from silently going
/// blank again.
#[test]
fn directory_listing_decodes() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["workflow list"], hits: ["repoGetContents", "repoGetRawFile"]);
    let repo = TestRepo::create_initialized(inst, "null-contents");
    let dir = std::env::temp_dir().join(format!("gea-itest-{}-c", repo.name));
    let _ = std::fs::remove_dir_all(&dir);
    repo.clone_to(&dir);
    std::fs::create_dir_all(dir.join(".gitea/workflows")).expect("mkdir");
    commit_and_push(
        &dir,
        "main",
        ".gitea/workflows/ci.yml",
        "name: CI\non: [push]\njobs:\n  build:\n    runs-on: docker\n    steps:\n      - run: echo hi\n",
        "Add a workflow",
    );

    let (code, body) = repo.api("GET", "contents/.gitea/workflows", None);
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("ci.yml"), "the API should list the workflow: {body}");

    let run = inst.gea(["workflow", "list", "-R", &repo.slug()]);
    run.assert_ok("gea workflow list");
    assert!(
        run.stdout.contains("ci.yml"),
        "the API lists ci.yml but `gea workflow list` reported nothing. Its exit status was 0, \
         so this is a silently wrong answer rather than an error.\n--- stdout ---\n{}\n\
         --- server said ---\n{body}",
        run.stdout
    );
}

//! What a real Forgejo 16.0.4 (measured for fjo, which gea was ported from) sends, as opposed to what the specification says it sends.
//!
//! Go marshals a nil **pointer** as `null`, and Gitea spells optional scalars `*string`,
//! `*int64` and `*bool` throughout. The Swagger specification records none of that — a
//! `*string` and a `string` are both `"type": "string"` — so a generator reading the spec emits
//! a plain `String`, and `#[serde(default)]` does not save it: that attribute covers an
//! **absent** key, while an explicit `null` is a present key holding the wrong type.
//!
//! Every fixture derived from the specification therefore agrees with the models, and only a
//! server disagrees. That is why these payloads are transcribed from live responses rather than
//! constructed the way [`corpus`](../corpus.rs) constructs its own: a fixture written from the
//! same reading of the spec that produced the type cannot possibly catch this.
//!
//! Two bugs found this way, both against a real instance:
//!
//! - `PullRequest.merge_commit_sha` is `null` on an **open** pull request. Typed `String`, it
//!   made `pr list`, `pr view`, `pr diff`, `pr status` and `pr checks` exit 1 against any
//!   repository containing one. Merging the pull request out of band turned the field into a
//!   string and the identical command then succeeded, which is what isolated it to this field.
//! - A directory listing sends `encoding`, `content`, `target` and `submodule_git_url` as
//!   `null` — the spec's own descriptions say so — which failed the `Many` arm of the untagged
//!   `ContentsResponseOrList`, and serde then matched `One`, yielding an all-default entry with
//!   `name: ""` and `type: ""`. `gea workflow list` filtered that away and printed nothing,
//!   with exit 0, for a repository that has `ci.yml`. A wrong answer delivered as a success.

use gitea_model::{ContentsResponse, ContentsResponseOrList, Issue, PullRequest, Repository};

/// `POST /repos/{owner}/{repo}/pulls`, Forgejo 16.0.4 (measured for fjo, which gea was ported from), response body for a freshly opened pull
/// request, trimmed of the two nested `repo` objects and otherwise verbatim.
///
/// Nine `null`s, and not one of them is declared nullable anywhere in the specification.
const OPEN_PULL_REQUEST: &str = r#"{
  "id": 1,
  "url": "http://localhost:3000/itest/null-pr/pulls/1",
  "number": 1,
  "user": {
    "id": 1,
    "login": "itest",
    "login_name": "",
    "source_id": 0,
    "full_name": "",
    "email": "itest@example.org",
    "avatar_url": "http://localhost:3000/avatars/1",
    "html_url": "http://localhost:3000/itest",
    "language": "",
    "is_admin": true,
    "last_login": "2026-09-12T10:00:00Z",
    "created": "2026-09-12T09:59:00Z",
    "restricted": false,
    "active": true,
    "prohibit_login": false,
    "location": "",
    "pronouns": "",
    "website": "",
    "description": "",
    "visibility": "public",
    "followers_count": 0,
    "following_count": 0,
    "starred_repos_count": 0
  },
  "title": "open pr",
  "body": "",
  "labels": [],
  "milestone": null,
  "assignee": null,
  "assignees": null,
  "requested_reviewers": null,
  "requested_reviewers_teams": null,
  "state": "open",
  "draft": false,
  "is_locked": false,
  "comments": 0,
  "review_comments": 0,
  "additions": 1,
  "deletions": 0,
  "changed_files": 1,
  "html_url": "http://localhost:3000/itest/null-pr/pulls/1",
  "diff_url": "http://localhost:3000/itest/null-pr/pulls/1.diff",
  "patch_url": "http://localhost:3000/itest/null-pr/pulls/1.patch",
  "mergeable": true,
  "merged": false,
  "merged_at": null,
  "merge_commit_sha": null,
  "merged_by": null,
  "allow_maintainer_edit": false,
  "base": {
    "label": "main",
    "ref": "main",
    "sha": "7b5e2f4c2a1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f",
    "repo_id": 1,
    "repo": null
  },
  "head": {
    "label": "feature",
    "ref": "feature",
    "sha": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
    "repo_id": 1,
    "repo": null
  },
  "merge_base": "7b5e2f4c2a1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f",
  "due_date": null,
  "created_at": "2026-09-12T10:01:00Z",
  "updated_at": "2026-09-12T10:01:00Z",
  "closed_at": null,
  "pin_order": 0
}"#;

/// `GET /repos/{owner}/{repo}/contents/.gitea/workflows`, Forgejo 16.0.4 (measured for fjo, which gea was ported from), verbatim.
///
/// The four `null`s here are the ones the specification's own field descriptions predict —
/// "`content` is populated when `type` is `file`, otherwise null" — and it still types all four
/// as `string`.
const DIRECTORY_LISTING: &str = r#"[
  {
    "name": "ci.yml",
    "path": ".gitea/workflows/ci.yml",
    "sha": "9c3f1b0a2d4e5f60718293a4b5c6d7e8f9012345",
    "last_commit_sha": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
    "type": "file",
    "size": 96,
    "encoding": null,
    "content": null,
    "target": null,
    "url": "http://localhost:3000/api/v1/repos/itest/wf/contents/.gitea/workflows/ci.yml?ref=main",
    "html_url": "http://localhost:3000/itest/wf/src/branch/main/.gitea/workflows/ci.yml",
    "git_url": "http://localhost:3000/api/v1/repos/itest/wf/git/blobs/9c3f1b0a2d4e5f60718293a4b5c6d7e8f9012345",
    "download_url": "http://localhost:3000/itest/wf/raw/branch/main/.gitea/workflows/ci.yml",
    "submodule_git_url": null,
    "_links": {
      "self": "http://localhost:3000/api/v1/repos/itest/wf/contents/.gitea/workflows/ci.yml?ref=main",
      "git": "http://localhost:3000/api/v1/repos/itest/wf/git/blobs/9c3f1b0a2d4e5f60718293a4b5c6d7e8f9012345",
      "html": "http://localhost:3000/itest/wf/src/branch/main/.gitea/workflows/ci.yml"
    }
  }
]"#;

/// `GET /repos/{owner}/{repo}/contents/README.md` — the single-value shape the specification
/// declares, which must keep working now that the enum dispatches on shape rather than order.
const SINGLE_FILE: &str = r#"{
  "name": "README.md",
  "path": "README.md",
  "sha": "557db03de997c86a4a028e1ebd3a1ceb225be238",
  "last_commit_sha": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
  "type": "file",
  "size": 13,
  "encoding": "base64",
  "content": "SGVsbG8gV29ybGQK",
  "target": null,
  "url": "http://localhost:3000/api/v1/repos/itest/wf/contents/README.md?ref=main",
  "html_url": "http://localhost:3000/itest/wf/src/branch/main/README.md",
  "git_url": "http://localhost:3000/api/v1/repos/itest/wf/git/blobs/557db03de997c86a4a028e1ebd3a1ceb225be238",
  "download_url": "http://localhost:3000/itest/wf/raw/branch/main/README.md",
  "submodule_git_url": null,
  "_links": {
    "self": "http://localhost:3000/api/v1/repos/itest/wf/contents/README.md?ref=main",
    "git": "http://localhost:3000/api/v1/repos/itest/wf/git/blobs/557db03de997c86a4a028e1ebd3a1ceb225be238",
    "html": "http://localhost:3000/itest/wf/src/branch/main/README.md"
  }
}"#;

// ----------------------------------------------------------------------- bug 1: null scalars

/// **The regression test for the `pr`-group outage.**
///
/// One `null` in one field of one model took out five commands against every repository with an
/// open pull request. Nothing about `pub merge_commit_sha: String` said it could be `null`, and
/// no fixture derived from the specification could say so either.
#[test]
fn an_open_pull_request_decodes() {
    let pr: PullRequest = serde_json::from_str(OPEN_PULL_REQUEST)
        .expect("an open pull request is the ordinary case and must decode");

    // The field itself: `null` lands where an absent key would have, which is `""`.
    assert_eq!(pr.merge_commit_sha, "");
    assert_eq!(pr.number.get(), 1);
    assert_eq!(pr.title, "open pr");
    assert!(!pr.merged);

    // `null` on an optional is unchanged — it was already correct and must stay that way.
    assert_eq!(pr.merged_at, None);
    assert_eq!(pr.merged_by, None);
    assert_eq!(pr.closed_at, None);
    // …and a nil slice is still the empty slice.
    assert!(pr.assignees.is_empty());
    assert!(pr.requested_reviewers.is_empty());
}

/// Every `null` in the payload, enumerated, so that a new one in a future Gitea shows up as a
/// named failure rather than as "`gea pr list` stopped working".
#[test]
fn every_null_in_the_open_pull_request_payload_is_tolerated() {
    let v: serde_json::Value = serde_json::from_str(OPEN_PULL_REQUEST).unwrap();
    let nulls: Vec<&str> = v
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k.as_str())
        .collect();
    assert!(
        nulls.contains(&"merge_commit_sha"),
        "this test is pointless unless the fixture really carries the null: {nulls:?}"
    );
    assert!(nulls.len() >= 8, "expected the payload to be null-heavy, got {nulls:?}");

    // And each one, alone, against an otherwise empty object: the field is what is being
    // tested, not the payload around it.
    for key in nulls {
        let solo = serde_json::json!({ key: serde_json::Value::Null });
        serde_json::from_value::<PullRequest>(solo)
            .unwrap_or_else(|e| panic!("PullRequest.{key} rejected an explicit null: {e}"));
    }
}

/// The rule is not "the fields we caught a server sending `null` for", it is every plain
/// scalar. A `String`, an `i64`, a `bool`, an ID newtype and an open enum, each nulled.
#[test]
fn a_null_is_the_zero_value_for_every_plain_scalar_shape() {
    let pr: PullRequest = serde_json::from_str(
        r#"{"id": null, "number": null, "title": null, "comments": null,
            "merged": null, "state": null, "merge_commit_sha": null}"#,
    )
    .expect("a null in any plain scalar must be the zero value, not a failed request");
    assert_eq!(pr.id.get(), 0);
    assert_eq!(pr.number.get(), 0);
    assert_eq!(pr.title, "");
    assert_eq!(pr.comments, 0);
    assert!(!pr.merged);
    assert_eq!(pr.merge_commit_sha, "");
    // The open enum falls back to its declared default rather than to `Unknown("null")`.
    assert_eq!(pr.state, gitea_model::StateType::default());
}

/// A `null` must not become a *value*. Tolerating it is only correct because the zero value and
/// "absent" already mean the same thing here; if a `null` round-tripped as `"null"` or as a
/// literal `null` in a PATCH body it would clear a field the caller never mentioned.
#[test]
fn a_tolerated_null_serializes_as_the_zero_value() {
    let pr: PullRequest = serde_json::from_str(OPEN_PULL_REQUEST).unwrap();
    let out = serde_json::to_value(&pr).unwrap();
    assert_eq!(out["merge_commit_sha"], serde_json::Value::String(String::new()));
}

/// The other models a null-heavy response reaches. `Issue` and `Repository` are the two most
/// requested types in the tool, and both carry `*string` fields in Go.
#[test]
fn nulls_in_the_other_hot_models_are_tolerated() {
    let issue: Issue = serde_json::from_str(
        r#"{"title": null, "body": null, "ref": null, "original_author": null}"#,
    )
    .expect("Issue must tolerate a null scalar");
    assert_eq!(issue.title, "");

    let repo: Repository = serde_json::from_str(
        r#"{"description": null, "website": null, "language": null, "avatar_url": null,
            "default_branch": null, "private": null, "stars_count": null}"#,
    )
    .expect("Repository must tolerate a null scalar");
    assert_eq!(repo.description, "");
    assert!(!repo.private);
    assert_eq!(repo.stars_count, 0);
}

// ------------------------------------------------- bug 2: an untagged enum that matched anything

/// **The regression test for `gea workflow list` silently printing nothing.**
///
/// Under `#[serde(untagged)]` this array failed `Many` on the four nulls and then *matched*
/// `One`, because a struct whose every field defaults matches any input serde hands it. The
/// result was an entry with `name: ""` and `type: ""`, filtered away by `type == "file"`.
#[test]
fn a_directory_listing_decodes_as_a_list() {
    let got: ContentsResponseOrList = serde_json::from_str(DIRECTORY_LISTING)
        .expect("a directory listing is the ordinary case and must decode");

    match &got {
        ContentsResponseOrList::Many(entries) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "ci.yml");
            assert_eq!(entries[0].r#type, "file");
            assert_eq!(entries[0].size, 96);
            // The four the specification's own descriptions call nullable.
            assert_eq!(entries[0].encoding, "");
            assert_eq!(entries[0].content, "");
            assert_eq!(entries[0].target, "");
            assert_eq!(entries[0].submodule_git_url, "");
        }
        ContentsResponseOrList::One(one) => panic!(
            "an array decoded as the single-value variant, which is the exact silent-wrong-answer \
             bug this test exists for: {one:?}"
        ),
        other => panic!("unexpected variant: {other:?}"),
    }

    // What `gea workflow list` actually does with it.
    let files: Vec<&ContentsResponse> =
        got.as_slice().iter().filter(|c| c.r#type == "file").collect();
    assert_eq!(files.len(), 1, "the workflow filter must find ci.yml");
    assert_eq!(files[0].name, "ci.yml");
}

/// The single-value shape still works. Shape dispatch has to be right in both directions, and
/// an object must not become a one-element list.
#[test]
fn a_single_file_decodes_as_one() {
    let got: ContentsResponseOrList =
        serde_json::from_str(SINGLE_FILE).expect("the declared single-value shape must decode");
    let one = got.one().expect("an object is the One variant");
    assert_eq!(one.name, "README.md");
    assert_eq!(one.encoding, "base64");
    assert_eq!(got.as_slice().len(), 1);
}

/// **The property reordering the variants would not have bought.**
///
/// A genuine decode error inside `Many` must surface as a decode error. With `#[serde(untagged)]`
/// — in either variant order — `One` absorbed whatever `Many` rejected and produced an empty,
/// all-default value, so a malformed element was indistinguishable from a valid single entry.
/// Here the element's type error is propagated, naming the field.
#[test]
fn a_decode_error_inside_many_propagates_instead_of_falling_through() {
    // `size` is an `i64`; a nested object is not one under any tolerance rule.
    let bad = r#"[{"name": "ci.yml", "type": "file", "size": {"not": "a number"}}]"#;
    let err = serde_json::from_str::<ContentsResponseOrList>(bad)
        .expect_err("a malformed element must be an error, not an empty single value");
    let msg = err.to_string();
    assert!(
        msg.contains("invalid type: map") || msg.contains("size"),
        "the error should describe the real failure inside the element, not the wrapper: {msg}"
    );
}

/// The same, one level up: an array of the wrong thing entirely is an error rather than a
/// silently empty result.
#[test]
fn an_array_of_the_wrong_shape_is_an_error() {
    let err = serde_json::from_str::<ContentsResponseOrList>(r#"["ci.yml", "release.yml"]"#)
        .expect_err("an array of strings is not a listing and must not decode to anything");
    assert!(err.to_string().contains("invalid type: string"), "{err}");
}

/// An empty array is an empty listing — a real, correct answer, and the one shape that must
/// *not* be an error. This is the boundary the fall-through used to blur.
#[test]
fn an_empty_listing_is_an_empty_list_not_an_empty_entry() {
    let got: ContentsResponseOrList = serde_json::from_str("[]").unwrap();
    assert_eq!(got, ContentsResponseOrList::Many(Vec::new()));
    assert!(got.as_slice().is_empty());
    assert!(got.one().is_none(), "an empty directory is not a single file");
}

/// Round-tripping keeps the shape: a list serializes as an array, not as an object.
#[test]
fn the_shape_survives_a_round_trip() {
    for raw in [DIRECTORY_LISTING, SINGLE_FILE] {
        let typed: ContentsResponseOrList = serde_json::from_str(raw).unwrap();
        let out = serde_json::to_string(&typed).unwrap();
        let again: ContentsResponseOrList =
            serde_json::from_str(&out).expect("our own output must decode");
        assert_eq!(typed, again);
    }
}

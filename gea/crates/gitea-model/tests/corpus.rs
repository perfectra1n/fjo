//! A fixture corpus: real-shaped JSON for the models people actually use, asserted to
//! deserialize and to survive a round trip.
//!
//! Why a corpus rather than more unit tests on the generator: the generator's tests prove it
//! renders the IR faithfully, and the IR's tests prove lowering read the spec faithfully.
//! Neither notices if the *specification itself* disagrees with what a Gitea server sends —
//! and it does, in exactly the places Go's `encoding/json` is loose. This file is where that
//! shows up, because these are payloads, not derivations.
//!
//! Each test names the bug it prevents. The single highest-value assertion in the file is
//! [`go_zero_time_on_an_unmerged_pull_request_is_none`]: without it, `gea pr list` prints
//! "2025 years ago" in the merged column of every open pull request.

use gitea_model::{
    ActionWorkflowRun, Attachment, Branch, Comment, Commit, Hook, Issue, Label, Milestone,
    Organization, PullRequest, Release, Repository, StateType, Team, User,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

// ------------------------------------------------------------------------------- the corpus

const USER: &str = r#"{
  "id": 1,
  "login": "perf3ct",
  "login_name": "",
  "full_name": "Jon",
  "email": "jon@example.org",
  "avatar_url": "https://git.example.org/avatars/1",
  "html_url": "https://git.example.org/perf3ct",
  "is_admin": true,
  "restricted": false,
  "active": true,
  "prohibit_login": false,
  "location": "",
  "website": "",
  "description": "",
  "visibility": "public",
  "followers_count": 3,
  "following_count": 5,
  "starred_repos_count": 12,
  "language": "en-US",
  "created": "2024-01-02T03:04:05Z",
  "last_login": "2026-09-01T00:00:00Z"
}"#;

const REPOSITORY: &str = r#"{
  "id": 42,
  "name": "gea",
  "full_name": "perf3ct/gea",
  "description": "A Gitea CLI",
  "empty": false,
  "private": false,
  "fork": false,
  "template": false,
  "mirror": false,
  "size": 1024,
  "language": "Rust",
  "html_url": "https://git.example.org/perf3ct/gea",
  "url": "https://git.example.org/api/v1/repos/perf3ct/gea",
  "ssh_url": "git@git.example.org:perf3ct/gea.git",
  "clone_url": "https://git.example.org/perf3ct/gea.git",
  "default_branch": "main",
  "object_format_name": "sha256",
  "default_merge_style": "merge",
  "topics": ["rust", "gitea"],
  "stars_count": 7,
  "forks_count": 1,
  "watchers_count": 2,
  "open_issues_count": 3,
  "open_pr_counter": 1,
  "release_counter": 0,
  "has_issues": true,
  "has_wiki": true,
  "has_pull_requests": true,
  "has_actions": true,
  "archived": false,
  "created_at": "2025-06-01T12:00:00Z",
  "updated_at": "2026-09-12T10:30:00Z",
  "owner": {"id": 1, "login": "perf3ct"},
  "permissions": {"admin": true, "push": true, "pull": true},
  "internal_tracker": {"enable_time_tracker": true},
  "parent": {
    "id": 9,
    "name": "gea",
    "full_name": "upstream/gea",
    "owner": {"id": 2, "login": "upstream"}
  }
}"#;

/// An **open** pull request: `merged_at` carries Go's zero time, which is what Gitea actually
/// sends. See [`go_zero_time_on_an_unmerged_pull_request_is_none`].
const PULL_REQUEST_OPEN: &str = r#"{
  "id": 4212,
  "number": 17,
  "url": "https://git.example.org/perf3ct/gea/pulls/17",
  "html_url": "https://git.example.org/perf3ct/gea/pulls/17",
  "diff_url": "https://git.example.org/perf3ct/gea/pulls/17.diff",
  "patch_url": "https://git.example.org/perf3ct/gea/pulls/17.patch",
  "state": "open",
  "title": "Add the models emitter",
  "body": "Closes #12",
  "draft": false,
  "merged": false,
  "mergeable": true,
  "is_locked": false,
  "comments": 4,
  "review_comments": 2,
  "additions": 900,
  "deletions": 12,
  "changed_files": 7,
  "merge_base": "6bf0f49",
  "created_at": "2026-09-10T08:00:00Z",
  "updated_at": "2026-09-12T09:00:00Z",
  "closed_at": null,
  "merged_at": "0001-01-01T00:00:00Z",
  "merge_commit_sha": "",
  "merged_by": null,
  "due_date": null,
  "user": {"id": 1, "login": "perf3ct"},
  "assignees": [{"id": 1, "login": "perf3ct"}],
  "labels": [{"id": 3, "name": "enhancement", "color": "a2eeef"}],
  "head": {"label": "feat/models", "ref": "feat/models", "sha": "deadbeef"},
  "base": {"label": "main", "ref": "main", "sha": "cafebabe"}
}"#;

const ISSUE: &str = r#"{
  "id": 918273,
  "number": 12,
  "url": "https://git.example.org/api/v1/repos/perf3ct/gea/issues/12",
  "html_url": "https://git.example.org/perf3ct/gea/issues/12",
  "state": "closed",
  "title": "Timestamps render as 2025 years ago",
  "body": "Go zero time",
  "comments": 2,
  "is_locked": false,
  "original_author": "",
  "original_author_id": 0,
  "created_at": "2026-08-01T00:00:00Z",
  "updated_at": "2026-08-02T00:00:00Z",
  "closed_at": "2026-08-02T00:00:00Z",
  "due_date": null,
  "ref": "main",
  "user": {"id": 1, "login": "perf3ct"},
  "labels": [{"id": 4, "name": "bug", "color": "d73a4a"}],
  "assignees": [],
  "assets": []
}"#;

const LABEL: &str = r#"{
  "id": 3,
  "name": "enhancement",
  "color": "a2eeef",
  "description": "New feature or request",
  "exclusive": false,
  "is_archived": false,
  "url": "https://git.example.org/api/v1/repos/perf3ct/gea/labels/3"
}"#;

const MILESTONE: &str = r#"{
  "id": 5,
  "title": "v0.1.0",
  "description": "First release",
  "state": "open",
  "open_issues": 4,
  "closed_issues": 9,
  "created_at": "2026-01-01T00:00:00Z",
  "updated_at": "2026-09-01T00:00:00Z",
  "closed_at": null,
  "due_on": "2026-12-31T00:00:00Z"
}"#;

const RELEASE: &str = r#"{
  "id": 77,
  "tag_name": "v0.1.0",
  "target_commitish": "main",
  "name": "v0.1.0",
  "body": "First release",
  "url": "https://git.example.org/api/v1/repos/perf3ct/gea/releases/77",
  "html_url": "https://git.example.org/perf3ct/gea/releases/tag/v0.1.0",
  "tarball_url": "https://git.example.org/perf3ct/gea/archive/v0.1.0.tar.gz",
  "zipball_url": "https://git.example.org/perf3ct/gea/archive/v0.1.0.zip",
  "upload_url": "https://git.example.org/api/v1/repos/perf3ct/gea/releases/77/assets",
  "draft": false,
  "prerelease": false,
  "created_at": "2026-09-01T00:00:00Z",
  "published_at": "2026-09-01T00:00:00Z",
  "author": {"id": 1, "login": "perf3ct"},
  "assets": [
    {
      "id": 8,
      "name": "gea-x86_64.tar.gz",
      "size": 4096,
      "download_count": 3,
      "uuid": "aa-bb-cc",
      "browser_download_url": "https://git.example.org/attachments/aa-bb-cc",
      "created_at": "2026-09-01T00:00:00Z"
    }
  ]
}"#;

const COMMENT: &str = r#"{
  "id": 555,
  "body": "Looks good",
  "html_url": "https://git.example.org/perf3ct/gea/issues/12#issuecomment-555",
  "issue_url": "https://git.example.org/api/v1/repos/perf3ct/gea/issues/12",
  "pull_request_url": "",
  "original_author": "",
  "original_author_id": 0,
  "created_at": "2026-08-01T10:00:00Z",
  "updated_at": "2026-08-01T10:05:00Z",
  "user": {"id": 1, "login": "perf3ct"},
  "assets": []
}"#;

const BRANCH: &str = r#"{
  "name": "main",
  "protected": true,
  "required_approvals": 1,
  "enable_status_check": true,
  "status_check_contexts": ["ci/check"],
  "user_can_merge": true,
  "user_can_push": false,
  "effective_branch_protection_name": "main",
  "commit": {
    "id": "6bf0f49",
    "message": "Add emitter plumbing",
    "url": "https://git.example.org/perf3ct/gea/commit/6bf0f49",
    "timestamp": "2026-09-12T10:00:00Z",
    "added": [],
    "removed": [],
    "modified": ["Cargo.toml"]
  }
}"#;

const COMMIT: &str = r#"{
  "sha": "6bf0f4900000000000000000000000000000000a",
  "url": "https://git.example.org/api/v1/repos/perf3ct/gea/git/commits/6bf0f49",
  "html_url": "https://git.example.org/perf3ct/gea/commit/6bf0f49",
  "created": "2026-09-12T10:00:00Z",
  "author": {"id": 1, "login": "perf3ct"},
  "committer": {"id": 1, "login": "perf3ct"},
  "parents": [{"sha": "ee49efe", "url": "https://git.example.org/x"}],
  "stats": {"total": 12, "additions": 10, "deletions": 2},
  "files": [{"filename": "Cargo.toml", "status": "modified"}]
}"#;

const TEAM: &str = r#"{
  "id": 6,
  "name": "reviewers",
  "description": "Code review",
  "permission": "write",
  "includes_all_repositories": false,
  "can_create_org_repo": false,
  "units": ["repo.code", "repo.issues"],
  "units_map": {"repo.code": "write", "repo.issues": "read"},
  "organization": {"id": 2, "username": "acme", "visibility": "limited"}
}"#;

const ORGANIZATION: &str = r#"{
  "id": 2,
  "name": "acme",
  "username": "acme",
  "full_name": "ACME Corp",
  "email": "ops@example.org",
  "description": "",
  "location": "",
  "website": "",
  "avatar_url": "https://git.example.org/avatars/org/2",
  "visibility": "limited",
  "repo_admin_change_team_access": true
}"#;

const ACTION_RUN: &str = r#"{
  "id": 314,
  "run_number": 12,
  "run_attempt": 1,
  "display_title": "ci",
  "status": "completed",
  "conclusion": "success",
  "event": "push",
  "head_branch": "main",
  "head_sha": "6bf0f49",
  "path": "ci.yml@refs/heads/main",
  "html_url": "https://git.example.org/perf3ct/gea/actions/runs/12",
  "started_at": "2026-09-12T10:00:05Z",
  "completed_at": "2026-09-12T10:01:37Z",
  "repository_id": 42,
  "actor": {"id": 1, "login": "perf3ct"},
  "trigger_actor": {"id": 1, "login": "perf3ct"},
  "repository": {"id": 42, "name": "gea", "full_name": "perf3ct/gea"}
}"#;

const ATTACHMENT: &str = r#"{
  "id": 8,
  "name": "gea-x86_64.tar.gz",
  "size": 4096,
  "download_count": 3,
  "uuid": "aa-bb-cc",
  "browser_download_url": "https://git.example.org/attachments/aa-bb-cc",
  "created_at": "2026-09-01T00:00:00Z"
}"#;

const HOOK: &str = r#"{
  "id": 11,
  "type": "gitea",
  "active": true,
  "branch_filter": "*",
  "authorization_header": "",
  "events": ["push", "pull_request"],
  "config": {"content_type": "json", "url": "https://ci.example.org/hook"},
  "created_at": "2026-05-01T00:00:00Z",
  "updated_at": "2026-05-02T00:00:00Z"
}"#;

// ---------------------------------------------------------------------------------- the tests

/// Every fixture must deserialize. A field the spec spells differently from the server shows up
/// here as a dropped key in [`round_trip`], not as a failure — this test only proves nothing in
/// the payload is *fatal*.
#[test]
fn the_whole_corpus_deserializes() {
    fn ok<T: DeserializeOwned>(name: &str, raw: &str) {
        if let Err(e) = serde_json::from_str::<T>(raw) {
            panic!("{name} failed to deserialize: {e}");
        }
    }
    ok::<User>("User", USER);
    ok::<Repository>("Repository", REPOSITORY);
    ok::<PullRequest>("PullRequest", PULL_REQUEST_OPEN);
    ok::<Issue>("Issue", ISSUE);
    ok::<Label>("Label", LABEL);
    ok::<Milestone>("Milestone", MILESTONE);
    ok::<Release>("Release", RELEASE);
    ok::<Comment>("Comment", COMMENT);
    ok::<Branch>("Branch", BRANCH);
    ok::<Commit>("Commit", COMMIT);
    ok::<Team>("Team", TEAM);
    ok::<Organization>("Organization", ORGANIZATION);
    ok::<ActionWorkflowRun>("ActionWorkflowRun", ACTION_RUN);
    ok::<Attachment>("Attachment", ATTACHMENT);
    ok::<Hook>("Hook", HOOK);
}

/// **The highest-value assertion in this milestone.**
///
/// Go's `time.Time` zero value marshals to `"0001-01-01T00:00:00Z"`, and Gitea sends it for
/// `merged_at` on a pull request that has not been merged. Deserialized literally it is a real
/// instant in the year 1, which `timeago` renders as "2025 years ago" in the merged column of
/// every open pull request — a wrong answer that looks like a plausible one.
#[test]
fn go_zero_time_on_an_unmerged_pull_request_is_none() {
    let pr: PullRequest = serde_json::from_str(PULL_REQUEST_OPEN).unwrap();
    assert_eq!(pr.merged_at, None, "Go's zero time must not survive as a real instant");
    assert!(!pr.merged);
    // And an explicit null is the same answer, by a different code path.
    assert_eq!(pr.closed_at, None);
    // A real timestamp still arrives intact — the tolerance must not swallow everything.
    assert!(pr.created_at.is_some());
}

/// `Value -> T -> Value` must preserve every key the server sent.
///
/// This is the test that catches a field name the generator got wrong. Because no struct uses
/// `deny_unknown_fields` — deliberately, so a newer Gitea cannot break us — a misspelled field
/// deserializes *successfully* and silently discards the data. The only way to notice is to
/// check that what went in comes back out.
#[test]
fn round_tripping_preserves_every_key_the_server_sent() {
    round_trip::<User>("User", USER);
    round_trip::<Repository>("Repository", REPOSITORY);
    round_trip::<Issue>("Issue", ISSUE);
    round_trip::<Label>("Label", LABEL);
    round_trip::<Milestone>("Milestone", MILESTONE);
    round_trip::<Release>("Release", RELEASE);
    round_trip::<Comment>("Comment", COMMENT);
    round_trip::<Branch>("Branch", BRANCH);
    round_trip::<Commit>("Commit", COMMIT);
    round_trip::<Team>("Team", TEAM);
    round_trip::<Organization>("Organization", ORGANIZATION);
    round_trip::<ActionWorkflowRun>("ActionWorkflowRun", ACTION_RUN);
    round_trip::<Attachment>("Attachment", ATTACHMENT);
    round_trip::<Hook>("Hook", HOOK);
}

/// A pull request round-trips too, except for the one key that is *supposed* to disappear.
#[test]
fn a_pull_request_round_trips_apart_from_the_go_zero_time() {
    let mut input: Value = serde_json::from_str(PULL_REQUEST_OPEN).unwrap();
    // `merged_at` is absence in disguise, so dropping it from the output is the whole point;
    // every other key must survive.
    let dropped = input.as_object_mut().unwrap().remove("merged_at");
    assert_eq!(dropped, Some(Value::from("0001-01-01T00:00:00Z")));

    let typed: PullRequest = serde_json::from_value(input.clone()).unwrap();
    assert_subset(&input, &serde_json::to_value(&typed).unwrap(), "PullRequest");
}

/// `{}` must deserialize into every model.
///
/// This is what container-level `#[serde(default)]` buys, and it is not academic: Gitea omits
/// whole objects on some endpoints, and an older server omits fields a newer spec declares. A
/// model that needs a key present is a model that fails against half the fleet.
#[test]
fn an_empty_object_deserializes_into_every_model() {
    fn empty<T: DeserializeOwned + Default + PartialEq>(name: &str) {
        let v: T = serde_json::from_str("{}").unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(v == T::default(), "{name}: an empty object is not the default value");
    }
    empty::<User>("User");
    empty::<Repository>("Repository");
    empty::<PullRequest>("PullRequest");
    empty::<Issue>("Issue");
    empty::<Label>("Label");
    empty::<Milestone>("Milestone");
    empty::<Release>("Release");
    empty::<Comment>("Comment");
    empty::<Branch>("Branch");
    empty::<Commit>("Commit");
    empty::<Team>("Team");
    empty::<Organization>("Organization");
    empty::<ActionWorkflowRun>("ActionWorkflowRun");
    empty::<Attachment>("Attachment");
    empty::<Hook>("Hook");
}

/// A field a newer Gitea adds must be ignored, not fatal.
#[test]
fn an_unknown_field_from_a_newer_server_is_ignored() {
    let raw = r#"{"id": 1, "login": "perf3ct", "quantum_flux": {"nested": [1, 2]}}"#;
    let u: User = serde_json::from_str(raw).expect("no model may use deny_unknown_fields");
    assert_eq!(u.login, "perf3ct");
}

/// An explicit `null` in a plain scalar is the zero value, not an error.
///
/// This test used to assert the opposite, on the following reasoning: `#[serde(default)]`
/// covers a *missing* key rather than an explicit `null`, and Go's `encoding/json` marshals a
/// `string` as `""` and an `int64` as `0`, so only *pointer* fields could ever arrive as `null`
/// — and those, the argument went, are the `date-time` and `$ref` fields we already type as
/// `Option`.
///
/// The last step is false. Gitea uses `*string`, `*int64` and `*bool` for optional scalars
/// throughout, and the specification records none of it: a `*string` and a `string` are both
/// `"type": "string"`. So a plain scalar is exactly as exposed as a `Vec` was, and for the same
/// reason. `PullRequest.merge_commit_sha` — `null` on every open pull request, typed `String` —
/// is what proved it, by failing five commands against any repository with one open.
///
/// The tight-typing policy is untouched by the fix: a `null` is a third spelling of "absent",
/// alongside the missing key and the empty string, and all three land on the zero value. See
/// `tests/nulls.rs` for the real payloads, and `crate::de::null_as_default` for the mechanism.
#[test]
fn an_explicit_null_in_a_scalar_is_the_zero_value() {
    let u: User = serde_json::from_str(r#"{"login": null, "id": null, "is_admin": null}"#)
        .expect("a null scalar must not fail the whole response");
    assert_eq!(u.login, "");
    assert_eq!(u.id.get(), 0);
    assert!(!u.is_admin);

    // The boundary that remains: a `null` *element* inside a collection field is still an error.
    // Go does marshal a `[]*User` with a nil member as `[null]`; Gitea has been observed doing it
    // only at the top level of a listing (`GET /user/repos`), which the paginator handles by
    // dropping the row *and reporting it* (see `gitea_core::http`). Inside a model there is no
    // such channel, and silently dropping an entry would be a worse answer than failing.
    let err = serde_json::from_str::<Issue>(r#"{"assignees": [null]}"#).unwrap_err();
    assert!(err.to_string().contains("invalid type: null"), "{err}");
}

/// `Repository.parent` is `Option<Box<Repository>>`.
///
/// This test exists to *compile*. `Repository` refers to itself, and without the
/// strongly-connected-component pass in lowering assigning `OptionalBoxed`, the crate does not
/// build at all — rustc reports "recursive type has infinite size" against a struct nobody
/// wrote, somewhere inside 8,000 generated lines. The explicit `Box` in the annotation below is
/// what makes the assertion about the *shape* rather than just about the value.
#[test]
fn repository_parent_is_a_boxed_option() {
    let repo: Repository = serde_json::from_str(REPOSITORY).unwrap();
    let parent: Option<Box<Repository>> = repo.parent;
    let parent = parent.expect("the fixture has a parent");
    assert_eq!(parent.full_name, "upstream/gea");
    // And the recursion genuinely nests: a fork of a fork is representable.
    assert_eq!(parent.parent, None);
}

/// Curated ID newtypes keep a global row id and a per-repository counter apart.
///
/// Gitea carries both on the same object, and the API is inconsistent about which a path
/// parameter wants. Passing the wrong one either 404s or silently operates on a different, real
/// issue. Here the two are different types, so the wrong one does not compile.
#[test]
fn ids_and_indexes_are_different_types() {
    let pr: PullRequest = serde_json::from_str(PULL_REQUEST_OPEN).unwrap();
    assert_eq!(pr.id.get(), 4212);
    assert_eq!(pr.number.get(), 17);
    // The wire format stays a bare integer: a newtype that serialized as `{"0": 17}` would
    // break every request body it appeared in.
    assert_eq!(serde_json::to_value(pr.number).unwrap(), Value::from(17));
}

/// An enum value this build has never heard of must not fail the request.
#[test]
fn an_unknown_enum_value_round_trips_verbatim() {
    let raw = PULL_REQUEST_OPEN.replace(r#""state": "open""#, r#""state": "draft""#);
    let pr: PullRequest = serde_json::from_str(&raw).expect("a newer state must not be fatal");
    assert_eq!(pr.state, StateType::Unknown("draft".into()));
    assert!(!pr.state.is_known());
    // Re-serializing must not corrupt a value we did not understand.
    assert_eq!(serde_json::to_value(&pr.state).unwrap(), Value::from("draft"));
}

/// The two `format: uint64` fields accept what Go actually emits.
#[test]
fn a_quoted_uint64_deserializes() {
    // `format: uint64` is not valid Swagger 2.0; Gitea emits it because the Go type is
    // `uint64`, and such values do not reliably arrive as JSON numbers.
    let raw = r#"{"position": "7", "original_position": null}"#;
    let c: gitea_model::PullReviewComment = serde_json::from_str(raw).unwrap();
    assert_eq!(c.position, 7);
    assert_eq!(c.original_position, 0);
}

/// Absent optional fields must not serialize as explicit nulls.
///
/// A request body built from a default-constructed option struct has to be *empty*, because
/// Gitea treats an explicit `null` as "set this to null" on several PATCH endpoints — sending
/// one would clear a field the user never mentioned.
#[test]
fn a_default_struct_serializes_without_its_absent_options() {
    let json = serde_json::to_value(Repository::default()).unwrap();
    let obj = json.as_object().unwrap();
    assert!(!obj.contains_key("owner"), "an absent Option must be omitted, not null");
    assert!(!obj.contains_key("created_at"));
    assert!(!obj.contains_key("parent"));
    // Scalars and collections do serialize, at their zero value: they are `T`, not `Option<T>`,
    // deliberately, so no call site needs `.unwrap_or_default()`.
    assert_eq!(obj.get("name"), Some(&Value::from("")));
    assert_eq!(obj.get("topics"), Some(&Value::Array(vec![])));
}

// ------------------------------------------------------------------------------------ helpers

fn round_trip<T: DeserializeOwned + Serialize>(name: &str, raw: &str) {
    let input: Value = serde_json::from_str(raw).expect("the fixture is valid JSON");
    let typed: T = serde_json::from_value(input.clone())
        .unwrap_or_else(|e| panic!("{name} failed to deserialize: {e}"));
    let output = serde_json::to_value(&typed).expect("models always serialize");
    assert_subset(&input, &output, name);
}

/// Asserts every key present in `input` is present in `output` with an equal value.
///
/// Structural rather than a flat `assert_eq!`, because `output` legitimately carries *more*:
/// `#[serde(default)]` scalars the input omitted come back at their zero value. What must not
/// happen is a key going missing, which is the signature of a field name the generator got
/// wrong.
fn assert_subset(input: &Value, output: &Value, path: &str) {
    match (input, output) {
        (Value::Object(want), Value::Object(got)) => {
            for (k, v) in want {
                let Some(g) = got.get(k) else {
                    // An explicit `null` and an absent key mean the same thing to us, and the
                    // absence is deliberate: several Gitea PATCH endpoints read an explicit
                    // `null` as "clear this field", so echoing one back would clear something
                    // the caller never mentioned.
                    assert!(
                        v.is_null(),
                        "{path}.{k} was dropped: the model does not carry this field"
                    );
                    continue;
                };
                assert_subset(v, g, &format!("{path}.{k}"));
            }
        }
        (Value::Array(want), Value::Array(got)) => {
            assert_eq!(want.len(), got.len(), "{path} changed length");
            for (i, (v, g)) in want.iter().zip(got).enumerate() {
                assert_subset(v, g, &format!("{path}[{i}]"));
            }
        }
        (want, got) => assert_eq!(want, got, "{path} changed value"),
    }
}

//! Error classification against real server responses.
//!
//! The renderings are snapshot-tested elsewhere against hand-written `ServerBody` values. What
//! that cannot establish is whether Gitea actually *produces* those bodies — whether a 403
//! really names a scope, whether a 422 on topics really carries `invalidTopics`, whether the
//! extra probe on a 404 really fires. Each test here provokes the status from the real server.
//!
//! The governing rule, from the design: never swallow a server message. `tea`'s worst habit is
//! reporting "failed to merge PR, is it still open?" for every refusal while discarding the
//! reason the server gave.

use gea_itest::{TestRepo, cover, instance_or_skip};

/// A 404 on the repository itself is genuinely ambiguous, and the message has to say so rather
/// than picking one of the three causes and sounding confident.
#[test]
fn missing_repository_names_all_three_causes() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoGet"]);
    let run = inst.gea(["raw", "repo", "get", &inst.user, "definitely-not-a-real-repo"]);
    run.assert_code(5, "a missing repository");
    run.assert_says("renamed, transferred, or deleted");
    run.assert_says("private");
    run.assert_says("check the configured server");
}

/// On a 404 *under* an existing repository the runtime fires one extra `GET /repos/{o}/{r}` to
/// tell "the repository is fine, the issue is not" from "the repository is missing or private".
///
/// The probe is the interesting part, so it is verified twice: the rendering must claim the
/// repository was found, and the message must not offer the three-causes text that belongs to
/// the other branch.
#[test]
fn missing_resource_probes_the_repository_first() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue view"], hits: ["issueGetIssue", "repoGet"]);
    let repo = TestRepo::create(inst, "probe-404");

    let run = inst.gea(["issue", "view", "99999", "-R", &repo.slug()]);
    run.assert_code(5, "a missing issue in a repository that exists");
    run.assert_says("(found)");
    assert!(
        !run.stderr.contains("check these possible causes"),
        "the probe found the repository, so the ambiguous three-cause message is wrong here:\n{}",
        run.stderr
    );
}

/// A token without the scope an operation needs must be told which scope, named from `OpMeta`
/// rather than guessed from the path.
#[test]
fn insufficient_scope_names_the_scope() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue create"], hits: ["issueCreateIssue"]);
    let Ok(user) = inst.scoped_user("scoped", &["read:user", "read:repository", "read:issue"])
    else {
        // Creating a second account needs admin; against a borrowed instance we may not have it.
        println!("SKIPPED: could not mint a scope-limited token on this instance");
        return;
    };

    let repo = TestRepo::create(inst, "scope-403");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    let run = inst.gea_as(
        &user.token,
        ["issue", "create", "--title", "t", "--body", "b", "-R", &repo.slug()],
    );
    assert!(!run.ok(), "a read-only token should not be able to create an issue");
    run.assert_says("403");
    run.assert_says("write:issue");
    // The remedy has to say scopes cannot be added to an existing token, because trying to do
    // that is the obvious next move and it is impossible in Gitea.
    run.assert_says("existing token scopes cannot be");
    run.assert_says("changed.");
}

/// A 409 must carry the server's explanation.
///
/// Creating a repository that already exists is the cheapest real 409 Gitea offers. (Creating
/// a duplicate *label* is not: Gitea permits two labels with the same name, which is itself
/// worth knowing — it is the sort of assumption a mock would happily have confirmed.)
#[test]
fn conflict_repeats_what_the_server_said() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["repo create"], hits: ["createCurrentUserRepo"]);
    let repo = TestRepo::create(inst, "conflict-409");

    let run = inst.gea(["repo", "create", &repo.name, "--private"]);
    assert!(!run.ok(), "creating a repository that exists should fail:\n{}", run.stdout);
    assert!(
        run.stderr.contains("already exists"),
        "the server's own reason must survive into the message:\n{}",
        run.stderr
    );
}

/// A 422 on `PUT /repos/{o}/{r}/topics` answers with an `invalidTopics` array, and the message
/// has to name the offending topics rather than saying "validation failed".
///
/// Driven through `gea api` on purpose: `gea topic set` validates topic names locally and
/// never sends the request, so it cannot exercise the classifier. (That local validation is
/// itself worth knowing about — it reports "HTTP 422" and "the request reached the server" for
/// a request that was never made.)
#[test]
fn invalid_topics_are_named() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "topics-422");

    let scratch =
        std::env::temp_dir().join(format!("gea-itest-topics-{}.json", std::process::id()));
    std::fs::write(&scratch, r#"{"topics":["good-one","bad topic with spaces"]}"#).expect("write");

    let run = inst.gea([
        "api",
        "-X",
        "PUT",
        &format!("repos/{}/topics", repo.slug()),
        "--input",
        &scratch.to_string_lossy(),
    ]);
    let _ = std::fs::remove_file(&scratch);

    assert!(!run.ok(), "an invalid topic should be refused:\n{}", run.stdout);
    run.assert_says("422");
    run.assert_says("invalidTopics");
    run.assert_says("bad topic with spaces");
}

/// The same endpoint answers `"invalidTopics": null` when the failure is the count limit rather
/// than a bad name — a nil slice in the error body. The renderer must fall back to the server's
/// message instead of printing an empty list or failing to parse.
#[test]
fn validation_survives_a_null_invalid_topics() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "topics-null");

    let topics: Vec<String> = (0..30).map(|i| format!("\"topic-{i}\"")).collect();
    let scratch = std::env::temp_dir().join(format!("gea-itest-many-{}.json", std::process::id()));
    std::fs::write(&scratch, format!(r#"{{"topics":[{}]}}"#, topics.join(","))).expect("write");

    let run = inst.gea([
        "api",
        "-X",
        "PUT",
        &format!("repos/{}/topics", repo.slug()),
        "--input",
        &scratch.to_string_lossy(),
    ]);
    let _ = std::fs::remove_file(&scratch);

    assert!(!run.ok(), "exceeding the topic limit should be refused:\n{}", run.stdout);
    run.assert_says("422");
    run.assert_says("Exceeding maximum number of topics");
}

/// A 404 answering a POST to a *collection* means the request was bad, not that some object is
/// missing — there is no identifier in the path to be wrong about.
///
/// # This test used to assert something the server never says
///
/// It was written asserting that the rendering contains `no-such-branch`, on the stated premise
/// that Gitea answers with
/// `{"errors":["could not find 'no-such-branch' to be a commit, branch or tag …"]}`. It does not.
/// Forgejo 16.0.4 (measured for fjo, which gea was ported from) answers **every** shape of this request — bad head, bad base, owner-qualified
/// head, even head equal to base — with exactly:
///
/// ```json
/// {"message":"The target couldn't be found.","url":"…/api/swagger","errors":[]}
/// ```
///
/// captured directly from the server rather than inferred. The branch name is never in it.
///
/// That false premise travelled: into this comment, into the `#[ignore]` reason, into the task
/// tracker, and into a synthetic snapshot in `gitea-core` that passes precisely because it
/// feeds itself the sentence the server withholds. Two separate agents then reasoned from it,
/// one "fixing" it and one reporting it still broken. Both were describing a message that does
/// not exist.
///
/// `not_found_message` is therefore doing the right thing by returning `None` here: it
/// recognises "The target couldn't be found." as a restatement of the status, which is exactly
/// what it is. `server_message` remains worth carrying for the 404s that *do* say something —
/// it simply has nothing to carry on this route.
///
/// So what is asserted is what gea can actually know: that it does not invent a missing object,
/// and that it points at the request body, which is where the wrong name really is. gea is
/// more useful than the server here, not less.
#[test]
fn post_to_a_collection_reports_the_real_reason() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr create"], hits: ["repoCreatePullRequest"]);
    let repo = TestRepo::create_initialized(inst, "post-404");

    let run = inst.gea([
        "pr",
        "create",
        "--title",
        "t",
        "--body",
        "b",
        "--head",
        "no-such-branch",
        "--base",
        "main",
        "-R",
        &repo.slug(),
    ]);
    assert!(!run.ok(), "creating a pull request from a missing branch should fail");
    assert!(
        !run.stderr.contains("the identifier is what is wrong"),
        "a POST to a collection has no identifier to be wrong:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("--state all"),
        "listing existing pull requests is useless advice for a create:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("request body") || run.stderr.contains("--head"),
        "the wrong name travelled in the body, and the remedy must say so:\n{}",
        run.stderr
    );
}

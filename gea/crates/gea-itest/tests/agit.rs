//! The AGit flow: a pull request with no fork and no branch.
//!
//! `gea pr create --agit --topic <t>` pushes to `refs/for/<base>/<topic>`, and Gitea turns
//! that push into a pull request. Nothing about it can be faked usefully — the whole mechanism
//! is a git push interpreted by the server, so a `FakeGit` test proves only that we assembled
//! the argv we meant to. This is the feature `gh` structurally cannot match, so it is worth
//! proving against a server rather than against ourselves.

use std::path::PathBuf;

use gea_itest::{TestRepo, cover, git, instance_or_skip};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-agit-{}-{tag}", std::process::id()));
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

/// The defining property: a pull request exists afterwards, and no branch was created for it.
///
/// The exit status *is* asserted, and the history of why it once was not is worth keeping.
///
/// It was originally ignored because reading the new pull request back decodes
/// `merge_commit_sha`, which is null while the request is open, and the command therefore
/// reported failure after the push had already succeeded. That was a real bug and it is fixed
/// (`decode::open_pull_request_decodes` covers it and no longer needs `#[ignore]`), so the
/// justification is spent.
///
/// Leaving it spent was expensive. When this test failed in CI it asserted only that a pull
/// request existed, found none, and said `left: 0, right: 1` — with the command's exit code and
/// its entire stderr discarded, so the one artifact naming the cause was thrown away. That is
/// the same "swallowed the real reason" failure this project exists to avoid, and it cost a
/// round trip through CI to notice.
///
/// So: assert the status, and put stderr in the message. A test that can fail must be able to
/// say why.
#[test]
fn agit_creates_a_pull_request_without_a_branch() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr create"], hits: ["repoGet", "repoGetPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "agit");
    let scratch = Scratch::new("create");
    repo.clone_to(scratch.path());

    git(scratch.path(), &["checkout", "--quiet", "-b", "local-only"]);
    std::fs::write(scratch.path().join("a.txt"), "agit\n").expect("write");
    git(scratch.path(), &["add", "-A"]);
    git(scratch.path(), &["commit", "--quiet", "-m", "AGit: add a.txt"]);

    let before = branch_names(&repo);

    let run = inst.gea_in(
        scratch.path(),
        [
            "pr",
            "create",
            "--agit",
            "--topic",
            "my-topic",
            "--title",
            "AGit PR",
            "--body",
            "no fork, no branch",
            "-R",
            &repo.slug(),
        ],
    );
    assert!(
        run.ok(),
        "the AGit push failed (exit {:?}). The push IS the API call here, so its stderr is the \
         only account of what the server refused:\n--- stderr ---\n{}\n--- stdout ---\n{}",
        run.code,
        run.stderr,
        run.stdout
    );

    let (code, body) = repo.api("GET", "pulls?state=all", None);
    assert_eq!(code, 200, "{body}");
    let pulls: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    assert_eq!(pulls.len(), 1, "AGit should have opened exactly one pull request: {body}");
    assert_eq!(pulls[0]["title"], "AGit PR");

    // The whole point of AGit: the contribution exists without a branch on the server.
    let after = branch_names(&repo);
    assert_eq!(
        before, after,
        "AGit must not create a branch, but the branch list changed from {before:?} to {after:?}"
    );
    assert!(
        !after.contains(&"my-topic".to_owned()),
        "the topic must not become a branch: {after:?}"
    );
}

/// Pushing the same topic again updates the existing pull request instead of opening a second
/// one. Getting this wrong would spray duplicate pull requests at a project, so it is worth its
/// own assertion.
#[test]
fn agit_reuses_the_pull_request_for_the_same_topic() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["pr create"], hits: ["repoGet", "repoGetPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "agit-update");
    let scratch = Scratch::new("update");
    repo.clone_to(scratch.path());

    git(scratch.path(), &["checkout", "--quiet", "-b", "local-only"]);
    for (n, msg) in [("one\n", "AGit: first"), ("one\ntwo\n", "AGit: second")] {
        std::fs::write(scratch.path().join("a.txt"), n).expect("write");
        git(scratch.path(), &["add", "-A"]);
        git(scratch.path(), &["commit", "--quiet", "-m", msg]);
        let run = inst.gea_in(
            scratch.path(),
            [
                "pr",
                "create",
                "--agit",
                "--topic",
                "same-topic",
                "--title",
                "AGit PR",
                "--body",
                "b",
                "-R",
                &repo.slug(),
            ],
        );
        // Named, because this loop runs twice and "it failed" would not say which push.
        assert!(
            run.ok(),
            "the AGit push for {msg:?} failed (exit {:?}):\n--- stderr ---\n{}",
            run.code,
            run.stderr
        );
    }

    let (_, body) = repo.api("GET", "pulls?state=all", None);
    let pulls: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    assert_eq!(
        pulls.len(),
        1,
        "the same topic must update one pull request, not open several: {body}"
    );
}

/// A dry run that was told the base branch must not need the server to tell it the base branch.
///
/// `gea pr create` opened with an unconditional `GET /repos/{owner}/{repo}`, whose only consumer
/// was `repo.default_branch` as the fallback for `--base`. Supplying `--base` did not save the
/// request, so a command that changes nothing, prints one line, and had already been given the
/// one value the request would have supplied still needed the network and a working token — it
/// exited 4 on a stale token and 6 with no route to the host.
///
/// That mattered here more than anywhere else. For AGit the push IS the protocol, and this
/// request runs before it, so any failure means no push happened at all: the visible result is a
/// pull request that never appeared, with nothing on stdout to say why. A scheme-guessing bug in
/// this harness read as "AGit is broken" for a full CI round trip on exactly that shape.
///
/// A deliberately invalid token is the instrument: any request would come back 401, so exit 0
/// proves no request was made. That is a stronger claim than pointing at a dead host, which only
/// proves the *connection* was not attempted.
#[test]
fn a_dry_run_with_an_explicit_base_asks_the_server_nothing() {
    let inst = instance_or_skip!();
    // No `hits:`: the assertion is that this reaches the server for nothing.
    cover!(porcelain: ["pr create"]);
    let repo = TestRepo::create_initialized(inst, "agit-dry");
    let scratch = Scratch::new("dry");
    repo.clone_to(scratch.path());
    git(scratch.path(), &["checkout", "--quiet", "-b", "local-only"]);

    let run = inst.gea_env(
        scratch.path(),
        &[("GEA_TOKEN", "definitely-not-a-valid-token")],
        [
            "pr",
            "create",
            "--agit",
            "--dry-run",
            "--base",
            "main",
            "--topic",
            "my-topic",
            "--title",
            "AGit PR",
            "-R",
            &repo.slug(),
        ],
    );
    assert!(
        run.ok(),
        "a dry run given --base must reach the server for nothing, but it exited {:?}. A 401 \
         here means the unconditional repo fetch is back:\n--- stderr ---\n{}",
        run.code,
        run.stderr
    );
    assert!(
        run.stdout.contains("HEAD:refs/for/main/my-topic"),
        "the dry run must print the push it would make: {}",
        run.stdout
    );
}

/// A push the server accepted must never be reported as a failure by what happens after it.
///
/// `agit()` ended with `locate(...).await?`, so any error reading the new pull request back — a
/// 401, a decode, a dropped connection — exited non-zero for a push that had already landed. The
/// caller then printed "your title and body were saved; re-run with --recover", inviting a retry
/// of work that was done, with the pull request URL sitting in git's own output three lines above.
///
/// This is the bug the `#[ignore]` on the test above was originally about — a null
/// `merge_commit_sha` failing the decode of a pull request that had just been created. That was
/// fixed in the decoder; the same shape survived one call further out, where every other possible
/// error reaches it too.
///
/// The instrument is an invalid API token with a valid git credential: `TestRepo::push_url`
/// embeds its own token in the remote, so the push still lands while every API call 401s. That
/// isolates "the write succeeded" from "the read failed", which is the whole situation.
#[test]
fn a_push_the_server_accepted_is_not_reported_as_a_failure() {
    let inst = instance_or_skip!();
    // No `hits:`: the token is deliberately invalid, so every API call here is a 401.
    cover!(porcelain: ["pr create"]);
    let repo = TestRepo::create_initialized(inst, "agit-readback");
    let scratch = Scratch::new("readback");
    repo.clone_to(scratch.path());

    git(scratch.path(), &["checkout", "--quiet", "-b", "local-only"]);
    std::fs::write(scratch.path().join("a.txt"), "readback\n").expect("write");
    git(scratch.path(), &["add", "-A"]);
    git(scratch.path(), &["commit", "--quiet", "-m", "AGit: read-back"]);

    let run = inst.gea_env(
        scratch.path(),
        &[("GEA_TOKEN", "definitely-not-a-valid-token")],
        [
            "pr",
            "create",
            "--agit",
            "--base",
            "main",
            "--topic",
            "readback",
            "--title",
            "AGit PR",
            "-R",
            &repo.slug(),
        ],
    );

    // The push landed: that is the fact everything else has to agree with.
    let (_, body) = repo.api("GET", "pulls?state=all", None);
    let pulls: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
    assert_eq!(pulls.len(), 1, "the push should have created a pull request: {body}");

    assert!(
        run.ok(),
        "the pull request exists, so this must not report failure — it exited {:?}:\n--- stderr \
         ---\n{}",
        run.code,
        run.stderr
    );
    assert!(
        !run.stderr.contains("--recover"),
        "offering to recover a draft invites retrying work that succeeded:\n{}",
        run.stderr
    );
    // And it must SAY so, on stderr, with no terminal attached. `support::note` is TTY-gated by
    // design, which would make this a silent exit 0 with an empty stdout in a script — the
    // situation where the message matters most. Hence `support::warn`.
    assert!(
        run.stderr.contains("push succeeded and pull request #1 was created"),
        "a caller whose stdout is empty has to be told why, even with no terminal:\n{}",
        run.stderr
    );
}

fn branch_names(repo: &TestRepo<'_>) -> Vec<String> {
    let (_, body) = repo.api("GET", "branches", None);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let mut names: Vec<String> = v
        .as_array()
        .map(|a| a.iter().filter_map(|b| b["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    names.sort();
    names
}

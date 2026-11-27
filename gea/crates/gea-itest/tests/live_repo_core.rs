//! Repository lifecycles against a real Gitea: create, settings, collaborators, topics,
//! transfer, forks and the protection rules.
//!
//! This is the group where a mock is least useful, because almost every command here is a
//! *state machine on the server*. `FakeTransport` can prove that `repo archive` sends
//! `{"archived": true}`; only a real Gitea can say whether the repository is afterwards
//! read-only. Same for a fork that has to fall behind its parent before `merge-upstream` will do
//! anything, for a transfer that is an offer rather than a move, and for `repo rename`, whose
//! whole point is that the old path stops resolving.
//!
//! Two live findings are recorded in the tests below rather than in a changelog, because a
//! comment beside the assertion is where the next person will look:
//!
//! * `POST /user/repos` really requires the `write:user` scope, not the `write:repository` the
//!   vendored specification advertises — see [`a_repository_is_reachable_by_its_numeric_id_and_by_a_search_for_its_name`].
//! * `gea transfer start` cannot offer a repository to a *user*, only to an organization, because
//!   it always sends `team_ids` and Gitea refuses a non-nil `team_ids` for a user recipient.
//!   See [`offering_a_repository_to_an_organization_the_offerer_owns_transfers_it_immediately`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gea_itest::{Instance, TestRepo, commit_and_push, cover, git, instance_or_skip};

// --------------------------------------------------------------------------------- fixtures

/// A scratch directory that cleans up after itself, for the tests that need a git checkout.
///
/// Private to this file on purpose: the harness is shared by nine test binaries and adding a
/// helper to it would be a change to somebody else's file.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("gea-itest-repocore-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a scratch directory for the checkout");
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

/// An organization that deletes itself, and the repositories it owns along with it.
///
/// A fork of your own repository needs a different owner, and so does a transfer that has to be
/// accepted; an organization is the cheapest second owner on one instance. Gitea refuses to
/// delete an organization that still owns a repository, so `Drop` empties it first.
struct TestOrg<'a> {
    inst: &'a Instance,
    name: String,
    /// Repositories created under this organization by the test, deleted before the org itself.
    repos: Vec<String>,
}

impl<'a> TestOrg<'a> {
    fn create(inst: &'a Instance, prefix: &str) -> Self {
        // Organization names may not carry a hyphen-delimited counter the way repository names
        // do without looking odd, but they must still be unique across the parallel threads in
        // this binary, so the repository-name generator supplies the uniqueness and the hyphens
        // are stripped back out.
        let name = inst.unique_repo_name(prefix).replace('-', "");
        let (code, body) = inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{name}"}}"#)));
        assert!(
            (200..300).contains(&code),
            "could not create the organization {name}: {code} {body}"
        );
        Self { inst, name, repos: Vec::new() }
    }

    /// Remember a repository this organization owns, so `Drop` can clear the way for the delete.
    fn owns(&mut self, repo: &str) {
        self.repos.push(repo.to_owned());
    }
}

impl Drop for TestOrg<'_> {
    fn drop(&mut self) {
        for repo in &self.repos {
            let _ = self.inst.api("DELETE", &format!("repos/{}/{repo}", self.name), None);
        }
        let _ = self.inst.api("DELETE", &format!("orgs/{}", self.name), None);
    }
}

/// The repository as the server sees it, so an assertion is never made through the code under
/// test. Every mutation below is read back through this.
fn repo_json(inst: &Instance, slug: &str) -> serde_json::Value {
    let (code, body) = inst.api("GET", &format!("repos/{slug}"), None);
    assert_eq!(code, 200, "could not read {slug} back: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{slug} did not come back as JSON ({e}): {body}"))
}

fn exists(inst: &Instance, slug: &str) -> bool {
    inst.api("GET", &format!("repos/{slug}"), None).0 == 200
}

/// The commit a branch points at, for the fork-sync assertions.
fn branch_head(inst: &Instance, slug: &str, branch: &str) -> String {
    let (code, body) = inst.api("GET", &format!("repos/{slug}/branches/{branch}"), None);
    assert_eq!(code, 200, "could not read {slug}@{branch}: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("a branch");
    v["commit"]["id"].as_str().unwrap_or_default().to_owned()
}

/// The sha the repository's default branch is on, which several write endpoints require.
fn head_sha(inst: &Instance, slug: &str) -> String {
    let (code, body) = inst.api("GET", &format!("repos/{slug}/commits?limit=1"), None);
    assert_eq!(code, 200, "could not list commits of {slug}: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("a commit list");
    v[0]["sha"].as_str().unwrap_or_else(|| panic!("no commit in {body}")).to_owned()
}

/// The `login` values in a JSON array of users, as a set, so order never makes a test flaky.
fn logins(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|u| u["login"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

// ------------------------------------------------------------------- repository CRUD

/// The `repo create` / `repo view` / `repo list` / `repo delete` quartet, driven end to end.
///
/// `TestRepo` creates and deletes over the API, so the commands users actually type for those two
/// things are otherwise never exercised. The delete half is the one worth a live test: a
/// `FakeTransport` test can only prove a `DELETE` was composed, and what matters is that the
/// repository is gone from the server afterwards.
///
/// Deliberately no `TestRepo`: this test owns the whole lifecycle, and wrapping the repository in
/// something that also deletes it would hide a `repo delete` that silently did nothing.
#[test]
fn creating_a_repository_through_the_porcelain_lands_with_the_options_it_was_given() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["repo create", "repo view", "repo list", "repo delete"],
           hits: ["createCurrentUserRepo", "repoGet", "repoDelete"]);

    let name = inst.unique_repo_name("rc-create");
    let slug = format!("{}/{name}", inst.user);

    inst.gea(["repo", "create", &name, "--private", "--description", "made by the porcelain"])
        .assert_ok("gea repo create");

    let repo = repo_json(inst, &slug);
    assert_eq!(
        repo["private"],
        serde_json::json!(true),
        "--private did not reach the server: {repo}"
    );
    assert_eq!(repo["description"], serde_json::json!("made by the porcelain"), "{repo}");
    assert_eq!(repo["empty"], serde_json::json!(true), "no seeding was asked for: {repo}");

    inst.gea(["repo", "view", &slug, "--no-readme"]).assert_ok("gea repo view").assert_says(&slug);
    // -L rather than the global --limit: `repo list` shadows it, and the default page could
    // otherwise be filled by repositories the other tests in this binary create in parallel.
    inst.gea(["repo", "list", "-L", "100"]).assert_ok("gea repo list").assert_says(&slug);

    inst.gea(["repo", "delete", &slug, "--yes"]).assert_ok("gea repo delete");
    assert!(!exists(inst, &slug), "{slug} still exists after `gea repo delete`");
}

/// `--add-readme`, `--license` and `--default-branch` are three separate fields of one
/// `CreateRepoOption`, and getting `default_branch` wrong is invisible until the first pull
/// request targets the wrong branch. A mock cannot tell the difference between a branch named
/// and a branch created.
#[test]
fn seeding_a_new_repository_gives_it_the_default_branch_that_was_asked_for() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["repo create"], hits: ["createCurrentUserRepo"]);

    let name = inst.unique_repo_name("rc-seed");
    let slug = format!("{}/{name}", inst.user);

    inst.gea([
        "repo",
        "create",
        &name,
        "--public",
        "--add-readme",
        "--license",
        "MIT",
        "--default-branch",
        "trunk",
    ])
    .assert_ok("gea repo create with seeding");

    let repo = repo_json(inst, &slug);
    assert_eq!(repo["default_branch"], serde_json::json!("trunk"), "{repo}");
    assert_eq!(repo["empty"], serde_json::json!(false), "--add-readme left it empty: {repo}");
    assert_eq!(
        repo["private"],
        serde_json::json!(false),
        "--public did not reach the server: {repo}"
    );

    // The branch has to exist, not merely be named in the repository record.
    let (code, body) = inst.api("GET", &format!("repos/{slug}/branches/trunk"), None);
    assert_eq!(code, 200, "the seeded branch was not created: {body}");
    let (code, body) = inst.api("GET", &format!("repos/{slug}/contents/LICENSE"), None);
    assert_eq!(code, 200, "--license MIT did not seed a LICENSE file: {body}");

    let _ = inst.api("DELETE", &format!("repos/{slug}"), None);
}

/// `repo create --from-template` is a different endpoint from an ordinary create
/// (`POST /repos/{owner}/{repo}/generate`), and the thing that can silently go wrong is the
/// content not coming with it: an empty repository at the right name looks like success.
#[test]
fn generating_from_a_template_copies_the_template_content_into_the_new_repository() {
    let inst = instance_or_skip!();
    let template = TestRepo::create_initialized(inst, "rc-tmpl");
    cover!(porcelain: ["repo create", "repo edit"], hits: ["generateRepo", "repoEdit"]);

    inst.gea(["repo", "edit", &template.slug(), "--as-template"])
        .assert_ok("gea repo edit --as-template");
    let marked = repo_json(inst, &template.slug());
    assert_eq!(
        marked["template"],
        serde_json::json!(true),
        "--as-template did not stick: {marked}"
    );

    let name = inst.unique_repo_name("rc-gen");
    let slug = format!("{}/{name}", inst.user);
    inst.gea([
        "repo",
        "create",
        &name,
        "--private",
        "--from-template",
        &template.slug(),
        "--description",
        "from a template",
    ])
    .assert_ok("gea repo create --from-template");

    let generated = repo_json(inst, &slug);
    assert_eq!(generated["description"], serde_json::json!("from a template"), "{generated}");
    assert_eq!(
        generated["empty"],
        serde_json::json!(false),
        "the template's content did not come with it: {generated}"
    );
    let (code, body) = inst.api("GET", &format!("repos/{slug}/contents/README.md"), None);
    assert_eq!(code, 200, "the template's README was not copied: {body}");

    let _ = inst.api("DELETE", &format!("repos/{slug}"), None);
}

/// `EditRepoOption` has two dozen fields and `repo edit` sends only the ones that were named.
/// The bug this catches is the opposite of the obvious one: not "the setting did not change" but
/// "something nobody mentioned changed too", which is exactly what a `FakeTransport` test that
/// asserts on the request body cannot see, because the body is not what decides it — the server's
/// merge of `None` fields is.
#[test]
fn editing_a_repository_changes_only_the_settings_that_were_named() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-edit");
    cover!(porcelain: ["repo edit"], hits: ["repoEdit"]);

    let before = repo_json(inst, &repo.slug());

    inst.gea([
        "repo",
        "edit",
        &repo.slug(),
        "--description",
        "edited by the porcelain",
        "--website",
        "https://example.org/project",
        "--visibility",
        "public",
        "--disable-issues",
        "--enable-wiki",
        "--default-merge-style",
        "squash",
    ])
    .assert_ok("gea repo edit");

    let after = repo_json(inst, &repo.slug());
    assert_eq!(after["description"], serde_json::json!("edited by the porcelain"), "{after}");
    assert_eq!(after["website"], serde_json::json!("https://example.org/project"), "{after}");
    assert_eq!(
        after["private"],
        serde_json::json!(false),
        "--visibility public did not apply: {after}"
    );
    assert_eq!(
        after["has_issues"],
        serde_json::json!(false),
        "--disable-issues did not apply: {after}"
    );
    assert_eq!(after["has_wiki"], serde_json::json!(true), "--enable-wiki did not apply: {after}");
    assert_eq!(after["default_merge_style"], serde_json::json!("squash"), "{after}");

    // Untouched settings must be exactly what they were.
    for field in ["default_branch", "has_pull_requests", "has_releases", "allow_rebase"] {
        assert_eq!(
            after[field], before[field],
            "`repo edit` rewrote {field}, which nobody named: {after}"
        );
    }
}

/// Archiving is the one repository setting with an externally visible consequence, so this asserts
/// the consequence rather than the flag: an archived repository refuses a write with HTTP 423.
/// A mock could assert `{"archived": true}` was sent and prove nothing about read-only-ness.
#[test]
fn archiving_a_repository_makes_it_refuse_writes_and_unarchiving_lets_them_through_again() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-arch");
    cover!(porcelain: ["repo archive", "repo unarchive"], hits: ["repoEdit"]);

    inst.gea(["repo", "archive", &repo.slug(), "--yes"]).assert_ok("gea repo archive");
    let archived = repo_json(inst, &repo.slug());
    assert_eq!(archived["archived"], serde_json::json!(true), "{archived}");

    // Out of band, and through a path this test does not otherwise drive, so the claim is about
    // the repository rather than about the command that changed it.
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"while archived"}"#));
    assert_eq!(code, 423, "an archived repository must refuse a write: HTTP {code}: {body}");

    inst.gea(["repo", "unarchive", &repo.slug(), "--yes"]).assert_ok("gea repo unarchive");
    let restored = repo_json(inst, &repo.slug());
    assert_eq!(restored["archived"], serde_json::json!(false), "{restored}");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"after unarchiving"}"#));
    assert!((200..300).contains(&code), "un-archiving did not restore writes: HTTP {code}: {body}");
}

/// A rename is a `PATCH` with a new `name`, and the half that a mock cannot check is that the old
/// path stops resolving. A rename that quietly left a second entry behind would look identical
/// from the request side.
#[test]
fn renaming_a_repository_moves_it_and_leaves_nothing_at_the_old_path() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-rename");
    cover!(porcelain: ["repo rename"], hits: ["repoEdit"]);

    let old = repo.slug();
    let renamed = inst.unique_repo_name("rc-renamed");
    let new = format!("{}/{renamed}", repo.owner);

    inst.gea(["repo", "rename", &old, &renamed]).assert_ok("gea repo rename");
    assert!(exists(inst, &new), "the repository is not at its new path {new}");
    assert!(!exists(inst, &old), "the old path {old} still resolves after a rename");

    // Renamed back so `TestRepo`'s own `Drop` still finds it. Leaving it renamed would leak a
    // repository into a container the rest of the suite is still using.
    inst.gea(["repo", "rename", &new, &repo.name]).assert_ok("gea repo rename back");
    assert!(exists(inst, &old), "the repository did not come back to {old}");
}

/// The three ways a repository can be addressed, checked against one another.
///
/// `repoGetByID` is the one worth having live: `GET /repositories/{id}` is a different route from
/// `GET /repos/{owner}/{repo}` and the only thing that proves the id in a repository record is
/// usable as a path parameter is asking the server for it.
///
/// Note for anyone driving these endpoints with a scoped token: the vendored specification says
/// `POST /user/repos` needs `write:repository`, and Forgejo 16.0.4 (measured for fjo, which gea was ported from) actually rejects such a token
/// with "token does not have at least one of required scope(s): [write:user]". That is a genuine
/// spec-versus-server divergence, and it is why the tests here create repositories with the
/// admin token rather than with `Instance::scoped_user`.
#[test]
fn a_repository_is_reachable_by_its_numeric_id_and_by_a_search_for_its_name() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "rc-find");
    cover!(raw: ["repoGet", "repoGetByID", "repoSearch"]);

    let by_path =
        inst.gea(["raw", "repo", "get", &repo.owner, &repo.name, "--json", "id,full_name"]);
    by_path.assert_ok("gea raw repo get");
    let got = by_path.json();
    assert_eq!(got["full_name"], serde_json::json!(repo.slug()), "{got}");
    let id = got["id"].as_i64().unwrap_or_else(|| panic!("no numeric id in {got}")).to_string();

    let by_id = inst.gea(["raw", "repo", "get-by-id", &id, "--json", "full_name"]);
    by_id.assert_ok("gea raw repo get-by-id");
    assert_eq!(
        by_id.json()["full_name"],
        serde_json::json!(repo.slug()),
        "the id from a repository record did not address the same repository"
    );

    let found = inst.gea(["raw", "repo", "search", "--q", &repo.name, "--json", "data"]);
    found.assert_ok("gea raw repo search");
    let names: BTreeSet<String> = found.json()["data"]
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&repo.slug()), "search for {:?} did not find it: {names:?}", repo.name);
}

// ------------------------------------------------------------------------ collaborators

/// The whole collaborator lifecycle, including the four read endpoints that only become
/// interesting once somebody has been added.
///
/// `repoCheckCollaborator` answers 204 or 404 with no body, so a mock test of it is a test of a
/// constant. Live, it is the only way to prove the delete actually revoked anything — and the
/// assignee and reviewer lists are how the server says the same thing from two other directions.
#[test]
fn adding_a_collaborator_grants_the_permission_asked_for_and_removing_takes_it_away() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-collab");
    let other = match inst.scoped_user("rccollab", &["read:repository", "read:user"]) {
        Ok(u) => u,
        Err(e) => panic!("could not create a second account to collaborate with: {e}"),
    };
    cover!(raw: [
        "repoAddCollaborator", "repoCheckCollaborator", "repoListCollaborators",
        "repoGetRepoPermissions", "repoGetAssignees", "repoGetReviewers", "repoDeleteCollaborator"
    ]);

    inst.gea([
        "raw",
        "repo",
        "add-collaborator",
        &repo.owner,
        &repo.name,
        &other.name,
        "--permission",
        "write",
    ])
    .assert_ok("gea raw repo add-collaborator");

    inst.gea(["raw", "repo", "check-collaborator", &repo.owner, &repo.name, &other.name])
        .assert_ok("gea raw repo check-collaborator after adding");

    let listed =
        inst.gea(["raw", "repo", "list-collaborators", &repo.owner, &repo.name, "--json", "login"]);
    listed.assert_ok("gea raw repo list-collaborators");
    assert!(
        logins(&listed.json()).contains(&other.name),
        "{} is not a collaborator: {}",
        other.name,
        listed.stdout
    );

    let perm = inst.gea([
        "raw",
        "repo",
        "get-repo-permissions",
        &repo.owner,
        &repo.name,
        &other.name,
        "--json",
        "permission",
    ]);
    perm.assert_ok("gea raw repo get-repo-permissions");
    assert_eq!(
        perm.json()["permission"],
        serde_json::json!("write"),
        "the permission asked for is not the one the server recorded: {}",
        perm.stdout
    );

    // A write collaborator becomes assignable and reviewable. Both lists are derived state, so
    // they are the server agreeing with itself about what the `PUT` meant.
    let assignees =
        inst.gea(["raw", "repo", "get-assignees", &repo.owner, &repo.name, "--json", "login"]);
    assignees.assert_ok("gea raw repo get-assignees");
    assert!(
        logins(&assignees.json()).contains(&other.name),
        "a write collaborator must be assignable: {}",
        assignees.stdout
    );
    let reviewers =
        inst.gea(["raw", "repo", "get-reviewers", &repo.owner, &repo.name, "--json", "login"]);
    reviewers.assert_ok("gea raw repo get-reviewers");
    assert!(
        logins(&reviewers.json()).contains(&other.name),
        "a write collaborator must be a possible reviewer: {}",
        reviewers.stdout
    );

    inst.gea(["raw", "repo", "delete-collaborator", &repo.owner, &repo.name, &other.name])
        .assert_ok("gea raw repo delete-collaborator");
    let gone =
        inst.gea(["raw", "repo", "check-collaborator", &repo.owner, &repo.name, &other.name]);
    assert!(
        !gone.ok(),
        "the collaborator is still recognised after being removed:\n{}\n{}",
        gone.stdout,
        gone.stderr
    );
}

// ------------------------------------------------------------------------------- topics

/// `topic add` and `topic remove` loop over the per-topic routes so they cannot discard a topic
/// somebody else added, while `topic set` replaces the whole list in one request. The difference
/// only shows up against a server that keeps the list between calls.
#[test]
fn adding_a_topic_makes_it_visible_to_a_fresh_list_and_setting_replaces_the_whole_list() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "rc-topic");
    cover!(porcelain: ["topic add", "topic list", "topic remove", "topic set"],
           hits: ["repoAddTopic", "repoListTopics", "repoDeleteTopic", "repoUpdateTopics"]);

    // Mixed case on purpose: Gitea lowercases topics, and the command does too, so a round trip
    // is the only way to see that the two agree.
    inst.gea(["topic", "add", "Rust", "cli", "-R", &repo.slug()]).assert_ok("gea topic add");
    assert_eq!(
        topics_on_server(&repo),
        set(["cli", "rust"]),
        "`topic add` did not add both topics"
    );

    let listed = inst.gea(["topic", "list", "-R", &repo.slug()]);
    listed.assert_ok("gea topic list");
    for want in ["rust", "cli"] {
        assert!(
            listed.stdout.contains(want),
            "`topic list` did not show {want:?}: {}",
            listed.stdout
        );
    }

    inst.gea(["topic", "remove", "cli", "-R", &repo.slug()]).assert_ok("gea topic remove");
    assert_eq!(topics_on_server(&repo), set(["rust"]), "`topic remove` removed the wrong thing");

    inst.gea(["topic", "set", "gitea", "testing", "-R", &repo.slug()]).assert_ok("gea topic set");
    assert_eq!(
        topics_on_server(&repo),
        set(["gitea", "testing"]),
        "`topic set` must replace the list, not extend it"
    );
}

fn topics_on_server(repo: &TestRepo<'_>) -> BTreeSet<String> {
    let (code, body) = repo.api("GET", "topics", None);
    assert_eq!(code, 200, "could not read the topics back: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("a topic list");
    v["topics"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn set<const N: usize>(items: [&str; N]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

// ----------------------------------------------------------------------------- transfer

/// A transfer is an *offer*, and that is the whole reason this group cannot be tested with a mock:
/// the pending state lives on the server, between two accounts, and only the recipient's own token
/// can settle it.
///
/// The offer is made with `gea raw repo transfer` rather than `gea transfer start` because of a
/// live defect this test found: `transfer start` always sends `team_ids`, and Gitea answers a
/// non-nil `team_ids` for a *user* recipient with 422 "Teams can only be added to
/// organization-owned repositories". The porcelain's `start` is therefore covered against an
/// organization, in the test below, and the accept/reject halves are covered here.
#[test]
fn an_offered_repository_moves_only_when_the_recipient_accepts_it() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-xfer");
    let other = match inst.scoped_user("rcxfer", &["write:repository", "read:user"]) {
        Ok(u) => u,
        Err(e) => panic!("could not create the second party a transfer needs: {e}"),
    };
    cover!(porcelain: ["transfer status", "transfer accept", "transfer reject"],
           hits: ["repoTransfer", "repoGet", "acceptRepoTransfer", "rejectRepoTransfer"]);

    // Hand it over first. An admin transferring to a user it administers applies immediately, with
    // no offer to accept — which is itself the behaviour the `start` command has to distinguish.
    inst.gea([
        "raw",
        "repo",
        "transfer",
        &repo.owner,
        &repo.name,
        "--new-owner",
        &other.name,
        "--json",
        "full_name",
    ])
    .assert_ok("handing the repository to the second account");
    let theirs = format!("{}/{}", other.name, repo.name);
    assert!(
        exists(inst, &theirs),
        "the immediate transfer did not move the repository to {theirs}"
    );

    // Offered back by somebody with no authority over the recipient: now it has to be accepted.
    let offer = || {
        inst.gea_as(
            &other.token,
            [
                "raw",
                "repo",
                "transfer",
                &other.name,
                &repo.name,
                "--new-owner",
                &repo.owner,
                "--json",
                "full_name",
            ],
        )
        .assert_ok("offering the repository back");
    };
    offer();

    let status = inst.gea(["transfer", "status", "-R", &theirs]);
    status.assert_ok("gea transfer status");
    status.assert_says("pending");
    // Both sides, because the person reading the status is usually not the person who must act.
    status.assert_says(&other.name);
    status.assert_says(&repo.owner);

    inst.gea(["transfer", "reject", "--yes", "-R", &theirs]).assert_ok("gea transfer reject");
    let after_reject = repo_json(inst, &theirs);
    assert_eq!(
        after_reject["repo_transfer"],
        serde_json::Value::Null,
        "the offer survived a rejection: {after_reject}"
    );
    assert_eq!(
        after_reject["full_name"],
        serde_json::json!(theirs),
        "a rejection must not move the repository"
    );

    offer();
    inst.gea(["transfer", "accept", "--yes", "-R", &theirs]).assert_ok("gea transfer accept");
    assert!(
        exists(inst, &repo.slug()),
        "accepting the offer did not move the repository to {}",
        repo.slug()
    );
    assert!(
        !exists(inst, &theirs),
        "the repository is still at {theirs} after the transfer was accepted"
    );
    // It is back where `TestRepo`'s `Drop` expects it, which is why the accept runs last.
}

/// `transfer start` against an owner the offerer administers, which Gitea applies immediately.
///
/// An organization is the recipient on purpose, and not only because it is a convenient second
/// owner: `transfer start` always serialises `team_ids`, and Forgejo 16.0.4 (measured for fjo, which gea was ported from) rejects a non-nil
/// `team_ids` unless the new owner is an organization. So today `gea transfer start <user>` fails
/// with 422 for every user recipient — a defect no `FakeTransport` test can see, because the
/// existing unit test supplies `--team` and therefore never sends the empty list.
#[test]
fn offering_a_repository_to_an_organization_the_offerer_owns_transfers_it_immediately() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-xferorg");
    let mut org = TestOrg::create(inst, "rcxferorg");
    cover!(porcelain: ["transfer start"], hits: ["repoTransfer"]);

    inst.gea(["transfer", "start", &org.name, "--yes", "-R", &repo.slug()])
        .assert_ok("gea transfer start");

    let moved = format!("{}/{}", org.name, repo.name);
    org.owns(&repo.name);
    let after = repo_json(inst, &moved);
    assert_eq!(after["full_name"], serde_json::json!(moved), "{after}");
    assert_eq!(
        after["repo_transfer"],
        serde_json::Value::Null,
        "an owner transferring into their own organization needs no acceptance: {after}"
    );
    assert!(!exists(inst, &repo.slug()), "the repository is still at its old path {}", repo.slug());
    // `TestRepo`'s `Drop` will try the old path and get a 404. That is fine — the drop is
    // best-effort, and `TestOrg` deletes the repository at its new home.
}

// -------------------------------------------------------------------------------- forks

/// A fork, the fork list, and server-side sync through `merge-upstream`.
///
/// The sync half has a precondition nothing but a real server can create: the fork must actually
/// be behind. So this pushes a commit to the parent, syncs, and checks the fork's branch points at
/// the parent's commit — then syncs again and checks an up-to-date fork is a no-op rather than an
/// error. A mock would decide for itself what "behind" meant and prove nothing.
#[test]
fn forking_into_an_organization_records_the_parent_and_the_fork_syncs_from_it() {
    let inst = instance_or_skip!();
    let source = TestRepo::create_initialized(inst, "rc-fork");
    // A fork of a private repository is a different permission path, and this test is about the
    // ordinary one.
    source.api("PATCH", "", Some(r#"{"private":false}"#));
    let mut org = TestOrg::create(inst, "rcforkorg");
    let scratch = Scratch::new("fork");
    cover!(porcelain: ["repo fork", "repo sync"],
           hits: ["createFork", "listForks", "repoGet", "repoMergeUpstream"]);

    inst.gea(["repo", "fork", &source.slug(), "--org", &org.name]).assert_ok("gea repo fork --org");
    org.owns(&source.name);
    let fork = format!("{}/{}", org.name, source.name);

    let forked = repo_json(inst, &fork);
    assert_eq!(forked["fork"], serde_json::json!(true), "{forked}");
    assert_eq!(
        forked["parent"]["full_name"],
        serde_json::json!(source.slug()),
        "the fork does not record its parent: {forked}"
    );

    let listed =
        inst.gea(["raw", "repo", "list-forks", &source.owner, &source.name, "--json", "full_name"]);
    listed.assert_ok("gea raw repo list-forks");
    let names: BTreeSet<String> = listed
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&fork), "the fork is missing from the parent's fork list: {names:?}");

    // Put the fork behind, which is what gives `merge-upstream` something to do.
    let checkout = scratch.path().join("source");
    source.clone_to(&checkout);
    commit_and_push(&checkout, "main", "ahead.txt", "one\n", "a commit the fork does not have");

    // No `-b`: the fork's default branch is looked up, because the endpoint has no default.
    let synced = inst.gea(["repo", "sync", &fork]);
    synced.assert_ok("gea repo sync (default branch)");
    assert_eq!(
        branch_head(inst, &fork, "main"),
        branch_head(inst, &source.slug(), "main"),
        "the fork's default branch did not fast-forward to the parent's"
    );

    // Again with the branch named.
    commit_and_push(
        &checkout,
        "main",
        "ahead.txt",
        "one\ntwo\n",
        "a second commit the fork does not have",
    );
    inst.gea(["repo", "sync", &fork, "-b", "main"]).assert_ok("gea repo sync -b main");
    assert_eq!(
        branch_head(inst, &fork, "main"),
        branch_head(inst, &source.slug(), "main"),
        "the named-branch sync did not fast-forward the fork"
    );

    // An up-to-date fork is a no-op with exit 0, not an error.
    let again = inst.gea(["repo", "sync", &fork, "-b", "main"]);
    again.assert_ok("gea repo sync, already up to date");
}

/// `repo clone` of a fork has to add a second remote for the project it was forked from, which is
/// the whole reason it exists rather than `git clone`. Nothing about that is checkable without a
/// real clone: the parent's URL comes from the server's own repository record.
#[test]
fn cloning_a_fork_adds_a_remote_for_the_repository_it_was_forked_from() {
    let inst = instance_or_skip!();
    let source = TestRepo::create_initialized(inst, "rc-clone");
    source.api("PATCH", "", Some(r#"{"private":false}"#));
    let mut org = TestOrg::create(inst, "rccloneorg");
    let scratch = Scratch::new("clone");
    cover!(porcelain: ["repo clone"], hits: ["createFork", "repoGet"]);

    inst.gea(["repo", "fork", &source.slug(), "--org", &org.name])
        .assert_ok("forking, so there is a parent to wire up");
    org.owns(&source.name);
    let fork = format!("{}/{}", org.name, source.name);

    inst.gea_in(scratch.path(), ["repo", "clone", &fork]).assert_ok("gea repo clone");

    let checkout = scratch.path().join(&source.name);
    assert!(checkout.join(".git").is_dir(), "no checkout at {}", checkout.display());
    let remotes = git(&checkout, &["remote", "-v"]);
    assert!(
        remotes.contains(&format!("{fork}.git")),
        "origin does not point at the fork: {remotes}"
    );
    assert!(
        remotes.contains("upstream") && remotes.contains(&format!("{}.git", source.slug())),
        "a fork's clone must get a remote for the repository it was forked from: {remotes}"
    );
}

/// `repo set-default` writes `remote.<name>.gea-resolved` into git config, and the only thing that
/// proves it is *useful* is a later command resolving the repository with no `-R`. That is a
/// three-way agreement between git, the resolver and the server which no unit test spans.
#[test]
fn recording_a_default_repository_lets_a_later_command_resolve_it_with_no_repo_flag() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-default");
    let scratch = Scratch::new("setdefault");
    cover!(porcelain: ["repo set-default", "topic list"], hits: ["repoListTopics"]);

    let checkout = scratch.path().join("checkout");
    repo.clone_to(&checkout);

    let viewed = inst.gea_in(&checkout, ["repo", "set-default", "--view"]);
    viewed.assert_ok("gea repo set-default --view");
    viewed.assert_says(&repo.slug());

    inst.gea_in(&checkout, ["repo", "set-default", &repo.slug()]).assert_ok("gea repo set-default");
    let config = git(&checkout, &["config", "--list"]);
    assert!(
        config.contains("gea-resolved"),
        "set-default must record the choice in git config, namespaced per remote: {config}"
    );

    // The point of the recorded choice: this has no -R and still has to find the repository.
    inst.gea_in(&checkout, ["topic", "add", "resolved", "-R", &repo.slug()])
        .assert_ok("seeding a topic to look for");
    let listed = inst.gea_in(&checkout, ["topic", "list"]);
    listed.assert_ok("gea topic list with no -R, resolved from the checkout");
    assert!(
        listed.stdout.contains("resolved"),
        "the recorded default did not resolve: {}",
        listed.stdout
    );

    inst.gea_in(&checkout, ["repo", "set-default", "--unset"])
        .assert_ok("gea repo set-default --unset");
    let config = git(&checkout, &["config", "--list"]);
    assert!(!config.contains("gea-resolved"), "--unset left the recorded choice behind: {config}");
}

// -------------------------------------------------------------- repository settings

/// Branch protection, created, read back, edited and deleted.
///
/// `enable_push: false` on the request becomes a whole rule object on the server, and the fields
/// the request did not mention get server-side defaults. Reading the rule back is the only way to
/// see that the two fields we did set survived that.
#[test]
fn a_branch_protection_rule_keeps_the_settings_it_was_created_with_until_it_is_edited() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-bp");
    cover!(raw: [
        "repoCreateBranchProtection", "repoListBranchProtection", "repoGetBranchProtection",
        "repoEditBranchProtection", "repoDeleteBranchProtection"
    ]);

    let created = inst.gea([
        "raw",
        "repo",
        "create-branch-protection",
        &repo.owner,
        &repo.name,
        "--rule-name",
        "main",
        "--enable-push=false",
        "--block-on-rejected-reviews=true",
        "--json",
        "rule_name,enable_push,block_on_rejected_reviews",
    ]);
    created.assert_ok("gea raw repo create-branch-protection");
    let created = created.json();
    assert_eq!(created["rule_name"], serde_json::json!("main"), "{created}");
    assert_eq!(created["enable_push"], serde_json::json!(false), "{created}");

    let listed = inst.gea([
        "raw",
        "repo",
        "list-branch-protection",
        &repo.owner,
        &repo.name,
        "--json",
        "rule_name",
    ]);
    listed.assert_ok("gea raw repo list-branch-protection");
    let rules: BTreeSet<String> = listed
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["rule_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert_eq!(rules, set(["main"]), "the rule is not in the repository's rule list");

    let got = inst.gea([
        "raw",
        "repo",
        "get-branch-protection",
        &repo.owner,
        &repo.name,
        "main",
        "--json",
        "block_on_rejected_reviews",
    ]);
    got.assert_ok("gea raw repo get-branch-protection");
    assert_eq!(got.json()["block_on_rejected_reviews"], serde_json::json!(true), "{}", got.stdout);

    let edited = inst.gea([
        "raw",
        "repo",
        "edit-branch-protection",
        &repo.owner,
        &repo.name,
        "main",
        "--block-on-rejected-reviews=false",
        "--json",
        "rule_name,block_on_rejected_reviews,enable_push",
    ]);
    edited.assert_ok("gea raw repo edit-branch-protection");
    let edited = edited.json();
    assert_eq!(edited["block_on_rejected_reviews"], serde_json::json!(false), "{edited}");
    assert_eq!(
        edited["enable_push"],
        serde_json::json!(false),
        "a PATCH rewrote a field it was not given: {edited}"
    );

    inst.gea(["raw", "repo", "delete-branch-protection", &repo.owner, &repo.name, "main"])
        .assert_ok("gea raw repo delete-branch-protection");
    let gone = inst.gea(["raw", "repo", "get-branch-protection", &repo.owner, &repo.name, "main"]);
    assert!(!gone.ok(), "the rule survived its delete:\n{}\n{}", gone.stdout, gone.stderr);
}

/// Tag protection, whose id is assigned by the server and is what every later call is addressed
/// by — so a lifecycle here is the only way to prove the id in the create response is the one the
/// `GET`, `PATCH` and `DELETE` routes accept.
///
/// `--whitelist-usernames` is not optional in practice: Gitea refuses a rule whose two
/// whitelists are both empty, which the specification does not say.
#[test]
fn a_tag_protection_rule_is_addressable_by_the_id_the_server_assigned_it() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-tp");
    cover!(raw: [
        "repoCreateTagProtection", "repoListTagProtection", "repoGetTagProtection",
        "repoEditTagProtection", "repoDeleteTagProtection"
    ]);

    let created = inst.gea([
        "raw",
        "repo",
        "create-tag-protection",
        &repo.owner,
        &repo.name,
        "--name-pattern",
        "v*",
        "--whitelist-usernames",
        &repo.owner,
        "--json",
        "id,name_pattern",
    ]);
    created.assert_ok("gea raw repo create-tag-protection");
    let created = created.json();
    assert_eq!(created["name_pattern"], serde_json::json!("v*"), "{created}");
    let id = created["id"].as_i64().unwrap_or_else(|| panic!("no id in {created}")).to_string();

    let listed = inst.gea([
        "raw",
        "repo",
        "list-tag-protection",
        &repo.owner,
        &repo.name,
        "--json",
        "id,name_pattern",
    ]);
    listed.assert_ok("gea raw repo list-tag-protection");
    assert!(listed.stdout.contains("v*"), "the rule is missing from the list: {}", listed.stdout);

    let got = inst.gea([
        "raw",
        "repo",
        "get-tag-protection",
        &repo.owner,
        &repo.name,
        &id,
        "--json",
        "name_pattern",
    ]);
    got.assert_ok("gea raw repo get-tag-protection");
    assert_eq!(
        got.json()["name_pattern"],
        serde_json::json!("v*"),
        "the id from the create reply addressed something else"
    );

    let edited = inst.gea([
        "raw",
        "repo",
        "edit-tag-protection",
        &repo.owner,
        &repo.name,
        &id,
        "--name-pattern",
        "rel-*",
        "--whitelist-usernames",
        &repo.owner,
        "--json",
        "name_pattern",
    ]);
    edited.assert_ok("gea raw repo edit-tag-protection");
    assert_eq!(edited.json()["name_pattern"], serde_json::json!("rel-*"), "{}", edited.stdout);

    inst.gea(["raw", "repo", "delete-tag-protection", &repo.owner, &repo.name, &id])
        .assert_ok("gea raw repo delete-tag-protection");
    let gone = inst.gea(["raw", "repo", "get-tag-protection", &repo.owner, &repo.name, &id]);
    assert!(!gone.ok(), "the rule survived its delete:\n{}\n{}", gone.stdout, gone.stderr);
}

/// Team access, which only exists on an organization-owned repository — the reason these four
/// endpoints are easy to leave untested and easy to get wrong.
#[test]
fn granting_a_team_access_to_an_organization_repository_shows_up_in_its_team_list() {
    let inst = instance_or_skip!();
    let mut org = TestOrg::create(inst, "rcteamorg");
    cover!(raw: ["repoAddTeam", "repoCheckTeam", "repoListTeams", "repoDeleteTeam"]);

    let team = "reviewers";
    let (code, body) = inst.api(
        "POST",
        &format!("orgs/{}/teams", org.name),
        Some(&format!(r#"{{"name":"{team}","permission":"write","units":["repo.code"]}}"#)),
    );
    assert!((200..300).contains(&code), "could not create a team to grant: {code} {body}");

    let name = inst.unique_repo_name("rc-team");
    let (code, body) = inst.api(
        "POST",
        &format!("orgs/{}/repos", org.name),
        Some(&format!(r#"{{"name":"{name}","auto_init":true}}"#)),
    );
    assert!(
        (200..300).contains(&code),
        "could not create an organization repository: {code} {body}"
    );
    org.owns(&name);

    inst.gea(["raw", "repo", "add-team", &org.name, &name, team])
        .assert_ok("gea raw repo add-team");
    let checked = inst.gea(["raw", "repo", "check-team", &org.name, &name, team, "--json", "name"]);
    checked.assert_ok("gea raw repo check-team");
    assert_eq!(checked.json()["name"], serde_json::json!(team), "{}", checked.stdout);

    let listed = inst.gea(["raw", "repo", "list-teams", &org.name, &name, "--json", "name"]);
    listed.assert_ok("gea raw repo list-teams");
    let teams: BTreeSet<String> = listed
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        teams.contains(team),
        "the granted team is not in the repository's team list: {teams:?}"
    );

    inst.gea(["raw", "repo", "delete-team", &org.name, &name, team])
        .assert_ok("gea raw repo delete-team");
    let gone = inst.gea(["raw", "repo", "check-team", &org.name, &name, team]);
    assert!(
        !gone.ok(),
        "the team still has access after being removed:\n{}\n{}",
        gone.stdout,
        gone.stderr
    );
}

/// Stargazers and subscribers, seeded by a *second* account so neither list is trivially the
/// owner. Gitea subscribes an owner to their own repository automatically, which would make a
/// single-account version of this pass without either endpoint working.
///
/// The star half briefly could not be written at all: while `[federation] ENABLED` was set on the
/// shared instance, Forgejo 16.0.4 (measured for fjo, which gea was ported from) answered `PUT /user/starred/{owner}/{repo}` with HTTP 500
/// (`StarRepo: client: invalid host for HostMatcher: nil client host(s)`), unaffected by
/// `OFFLINE_MODE` or `ALLOWED_HOST_LIST`. Federation now lives on its own instance, so this is a
/// 204 again — and the strong assertion is back rather than a count cross-check standing in for
/// it. Worth knowing if federation is ever enabled here again.
#[test]
fn a_second_account_starring_and_watching_a_repository_appears_in_both_of_its_lists() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-watch");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));
    let fan = match inst.scoped_user("rcwatch", &["write:user", "write:repository", "read:user"]) {
        Ok(u) => u,
        Err(e) => panic!("could not create the account that stars and watches: {e}"),
    };
    cover!(raw: ["repoListStargazers", "repoListSubscribers"]);

    let (code, body) =
        inst.api_as(&fan.token, "PUT", &format!("user/starred/{}", repo.slug()), None);
    assert!(
        (200..300).contains(&code),
        "the second account could not star the repository: {code} {body}"
    );
    let (code, body) =
        inst.api_as(&fan.token, "PUT", &format!("repos/{}/subscription", repo.slug()), None);
    assert!(
        (200..300).contains(&code),
        "the second account could not watch the repository: {code} {body}"
    );

    let stars =
        inst.gea(["raw", "repo", "list-stargazers", &repo.owner, &repo.name, "--json", "login"]);
    stars.assert_ok("gea raw repo list-stargazers");
    assert!(
        logins(&stars.json()).contains(&fan.name),
        "the stargazer list does not name who starred it: {}",
        stars.stdout
    );

    let watchers =
        inst.gea(["raw", "repo", "list-subscribers", &repo.owner, &repo.name, "--json", "login"]);
    watchers.assert_ok("gea raw repo list-subscribers");
    assert!(
        logins(&watchers.json()).contains(&fan.name),
        "the subscriber list does not name the watcher: {}",
        watchers.stdout
    );
}

/// Commit statuses, and the field-name asymmetry that only a real server exposes: the request
/// carries `state`, and the reply carries `status`. A `FakeTransport` test writes both halves and
/// would happily agree with itself on either spelling.
#[test]
fn a_commit_status_written_through_raw_comes_back_in_the_list_for_that_commit() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-status");
    cover!(raw: ["repoCreateStatus", "repoListStatuses", "repoCompareDiff"]);

    let sha = head_sha(inst, &repo.slug());
    let created = inst.gea([
        "raw",
        "repo",
        "create-status",
        &repo.owner,
        &repo.name,
        &sha,
        "--state",
        "success",
        "--context",
        "ci/live",
        "--description",
        "checked against a real server",
        "--target-url",
        "https://example.org/build/1",
        "--json",
        "context,status,target_url",
    ]);
    created.assert_ok("gea raw repo create-status");
    let created = created.json();
    assert_eq!(created["context"], serde_json::json!("ci/live"), "{created}");
    assert_eq!(
        created["status"],
        serde_json::json!("success"),
        "the request field is `state` and the reply field is `status`: {created}"
    );

    let listed = inst.gea([
        "raw",
        "repo",
        "list-statuses",
        &repo.owner,
        &repo.name,
        &sha,
        "--json",
        "context,status",
    ]);
    listed.assert_ok("gea raw repo list-statuses");
    assert!(
        listed.stdout.contains("ci/live"),
        "the status is missing from the list for its own commit: {}",
        listed.stdout
    );

    // `basehead` is one path parameter holding two refs separated by `...`, which is the sort of
    // thing that silently gets percent-encoded into something the server does not route.
    let compared = inst.gea([
        "raw",
        "repo",
        "compare-diff",
        &repo.owner,
        &repo.name,
        "main...main",
        "--json",
        "total_commits",
    ]);
    compared.assert_ok("gea raw repo compare-diff");
    assert_eq!(
        compared.json()["total_commits"],
        serde_json::json!(0),
        "a branch compared with itself has no commits: {}",
        compared.stdout
    );
}

/// Applying a diff patch, and reading back the `.editorconfig` it wrote.
///
/// One thing here can only be learned from a server: the patch body must end with a newline or
/// `git apply` inside Gitea answers "corrupt patch". Gitea's options carry no `sha` at all,
/// unlike Forgejo's, so the patch is applied to the branch head it names.
#[test]
fn applying_a_diff_patch_creates_the_files_the_patch_describes() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-patch");
    cover!(raw: ["repoApplyDiffPatch", "repoGetEditorConfig", "repoGetLanguages"]);

    let patch = concat!(
        "diff --git a/.editorconfig b/.editorconfig\n",
        "new file mode 100644\n",
        "--- /dev/null\n",
        "+++ b/.editorconfig\n",
        "@@ -0,0 +1,4 @@\n",
        "+root = true\n",
        "+\n",
        "+[*.md]\n",
        "+indent_size = 4\n",
    );
    inst.gea([
        "raw",
        "repo",
        "apply-diff-patch",
        &repo.owner,
        &repo.name,
        "--branch",
        "main",
        "--message",
        "add an editorconfig by patch",
        "--content",
        patch,
    ])
    .assert_ok("gea raw repo apply-diff-patch");

    let (code, body) = repo.api("GET", "contents/.editorconfig", None);
    assert_eq!(code, 200, "the patch reported success but wrote no file: {body}");

    // The file is not merely present: Gitea parses it, which is a stronger statement about what
    // the patch produced than reading the bytes back would be.
    //
    // No `--json` here, and that is not an oversight: Gitea's spec declares no response body at
    // all, so the operation is typed as an untyped value (`overrides.toml [response_type]`) and
    // `gea` refuses a field selection on it with exit 2. The response is a JSON object, which is
    // exactly the sort of thing the spec can be wrong about and only a live call can settle.
    let ec = inst.gea(["raw", "repo", "get-editor-config", &repo.owner, &repo.name, "README.md"]);
    ec.assert_ok("gea raw repo get-editor-config");
    assert_eq!(
        ec.json()["indent_size"],
        serde_json::json!("4"),
        "the committed .editorconfig was not applied: {}",
        ec.stdout
    );

    inst.gea(["raw", "repo", "get-languages", &repo.owner, &repo.name])
        .assert_ok("gea raw repo get-languages");
}

/// The read-only repository endpoints that have no lifecycle of their own, checked against a
/// freshly created repository so "it answered" is a real claim rather than an accident of whatever
/// state a shared instance happened to be in.
///
/// `signing-key.gpg` is here because it is the one operation in this group that does not return
/// JSON, so a decoder that assumed it did would only fail against a server.
#[test]
fn the_metadata_endpoints_answer_for_a_repository_that_has_only_just_been_created() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-meta");
    cover!(raw: [
        "repoGetIssueConfig", "repoValidateIssueConfig", "repoGetIssueTemplates", "repoNewPinAllowed",
        "repoListPinnedIssues", "repoListActivityFeeds", "repoTrackedTimes", "repoSigningKey",
        "repoSigningKeySSH"
    ]);

    for op in ["get-issue-templates", "list-pinned-issues", "list-activity-feeds", "tracked-times"]
    {
        inst.gea(["raw", "repo", op, &repo.owner, &repo.name])
            .assert_ok(&format!("gea raw repo {op}"));
    }
    // With no signing key configured Gitea answers 404 "no signing key" from the handler, where
    // Forgejo answers 200 with an empty body; the path in the error proves the route was reached.
    for (op, path) in [("signing-key", "signing-key.gpg"), ("signing-key-ssh", "signing-key.pub")] {
        let key = inst.gea(["raw", "repo", op, &repo.owner, &repo.name]);
        key.assert_code(5, &format!("gea raw repo {op} with no key configured"));
        key.assert_says(path);
    }

    let config = inst.gea([
        "raw",
        "repo",
        "get-issue-config",
        &repo.owner,
        &repo.name,
        "--json",
        "blank_issues_enabled",
    ]);
    config.assert_ok("gea raw repo get-issue-config");
    assert_eq!(
        config.json()["blank_issues_enabled"],
        serde_json::json!(true),
        "a repository with no issue templates must still accept a blank issue: {}",
        config.stdout
    );

    let valid = inst.gea([
        "raw",
        "repo",
        "validate-issue-config",
        &repo.owner,
        &repo.name,
        "--json",
        "valid",
    ]);
    valid.assert_ok("gea raw repo validate-issue-config");
    assert_eq!(
        valid.json()["valid"],
        serde_json::json!(true),
        "an absent issue config is a valid one: {}",
        valid.stdout
    );

    let pin =
        inst.gea(["raw", "repo", "new-pin-allowed", &repo.owner, &repo.name, "--json", "issues"]);
    pin.assert_ok("gea raw repo new-pin-allowed");
    assert_eq!(
        pin.json()["issues"],
        serde_json::json!(true),
        "nothing is pinned yet, so another pin must be allowed: {}",
        pin.stdout
    );
}

/// The avatar round trip. `POST /avatar` takes base64 in a JSON field rather than a multipart
/// upload, which is unusual enough to be worth proving against the server, and `DELETE` has to
/// actually clear the URL rather than leave a dangling one.
#[test]
fn setting_a_repository_avatar_publishes_a_url_and_deleting_it_takes_the_url_away() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "rc-avatar");
    cover!(raw: ["repoUpdateAvatar", "repoDeleteAvatar"]);

    // A 1x1 PNG. Small, but a real image: Gitea decodes it before storing.
    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    inst.gea(["raw", "repo", "update-avatar", &repo.owner, &repo.name, "--image", PIXEL])
        .assert_ok("gea raw repo update-avatar");

    let with_avatar = repo_json(inst, &repo.slug());
    let url = with_avatar["avatar_url"].as_str().unwrap_or_default();
    assert!(
        !url.is_empty(),
        "the repository has no avatar URL after one was uploaded: {with_avatar}"
    );

    inst.gea(["raw", "repo", "delete-avatar", &repo.owner, &repo.name])
        .assert_ok("gea raw repo delete-avatar");
    let without = repo_json(inst, &repo.slug());
    assert_eq!(
        without["avatar_url"].as_str().unwrap_or_default(),
        "",
        "deleting the avatar left the URL behind: {without}"
    );
}

/// Push mirrors, whose identity is a remote name the *server* invents, and which are only
/// interesting once one has actually pushed something.
///
/// Every call after the create is addressed by a name this process never chose, so nothing short
/// of a live lifecycle proves the name in the create reply is the one the `GET`, sync and `DELETE`
/// routes accept.
///
/// The mirror targets a second repository on this same instance, at [`Instance::internal_url`]:
/// Gitea is the one that opens the connection, so the address has to make sense inside the
/// container. That is what turns `push_mirrors-sync` from an endpoint that answers into a claim
/// worth making — the destination is checked to end up on the source's commit. Pointed anywhere
/// unreachable the sync is a 500, which is a test of nothing.
#[test]
fn a_push_mirror_is_addressable_by_its_server_assigned_name_and_delivers_what_it_mirrors() {
    let inst = instance_or_skip!();
    let source = TestRepo::create_initialized(inst, "rc-mirror-src");
    let destination = TestRepo::create(inst, "rc-mirror-dst");
    cover!(raw: [
        "repoAddPushMirror", "repoListPushMirrors", "repoGetPushMirrorByRemoteName",
        "repoPushMirrorSync", "repoDeletePushMirror"
    ]);

    let target = format!("{}/{}.git", inst.internal_url(), destination.slug());
    let created = inst.gea([
        "raw",
        "repo",
        "add-push-mirror",
        &source.owner,
        &source.name,
        "--remote-address",
        &target,
        "--remote-username",
        &inst.user,
        "--remote-password",
        &inst.token,
        "--interval",
        "8h0m0s",
        "--sync-on-commit=true",
        "--json",
        "remote_name,remote_address,sync_on_commit",
    ]);
    created.assert_ok("gea raw repo add-push-mirror");
    let created = created.json();
    assert_eq!(created["remote_address"], serde_json::json!(target), "{created}");
    assert_eq!(created["sync_on_commit"], serde_json::json!(true), "{created}");
    let remote = created["remote_name"]
        .as_str()
        .unwrap_or_else(|| panic!("no remote_name in {created}"))
        .to_owned();

    let listed = inst.gea([
        "raw",
        "repo",
        "list-push-mirrors",
        &source.owner,
        &source.name,
        "--json",
        "remote_name",
    ]);
    listed.assert_ok("gea raw repo list-push-mirrors");
    assert!(
        listed.stdout.contains(&remote),
        "the mirror is missing from the repository's list: {}",
        listed.stdout
    );

    let got = inst.gea([
        "raw",
        "repo",
        "get-push-mirror-by-remote-name",
        &source.owner,
        &source.name,
        &remote,
        "--json",
        "remote_address",
    ]);
    got.assert_ok("gea raw repo get-push-mirror-by-remote-name");
    assert_eq!(
        got.json()["remote_address"],
        serde_json::json!(target),
        "the server-assigned name addressed a different mirror"
    );

    inst.gea(["raw", "repo", "push-mirror-sync", &source.owner, &source.name])
        .assert_ok("gea raw repo push-mirror-sync");

    // The sync is queued, so the destination is polled rather than read once. The assertion is
    // about the commit, not about the branch existing: a mirror that pushed an empty branch would
    // satisfy the weaker check.
    assert!(
        wait_for_branch(inst, &destination.slug(), "main"),
        "the push mirror never delivered a `main` branch to {}",
        destination.slug()
    );
    assert_eq!(
        branch_head(inst, &destination.slug(), "main"),
        branch_head(inst, &source.slug(), "main"),
        "the mirror's destination is not on the commit the source is on"
    );

    inst.gea(["raw", "repo", "delete-push-mirror", &source.owner, &source.name, &remote])
        .assert_ok("gea raw repo delete-push-mirror");
    let gone = inst.gea([
        "raw",
        "repo",
        "get-push-mirror-by-remote-name",
        &source.owner,
        &source.name,
        &remote,
    ]);
    assert!(!gone.ok(), "the mirror survived its delete:\n{}\n{}", gone.stdout, gone.stderr);
}

/// Poll for a branch that another process is expected to create, for the asynchronous half of a
/// push mirror. Bounded rather than unbounded: a mirror that never delivers has to fail this test
/// with a message, not hang the run.
fn wait_for_branch(inst: &Instance, slug: &str, branch: &str) -> bool {
    for _ in 0..30 {
        if inst.api("GET", &format!("repos/{slug}/branches/{branch}"), None).0 == 200 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}

// -------------------------------------------------------------------- migration and mirrors

/// Migrate into a pull mirror and sync it.
///
/// One lifecycle rather than two tests because `mirror-sync` has no other fixture: it needs a
/// repository that *is* a mirror, and the only thing that can produce one is a migration — `PATCH /repos/{owner}/{repo}` cannot turn an ordinary
/// repository into one.
///
/// Two things here are structurally beyond a mock. The clone is done **by Gitea**, so the source
/// address has to make sense inside the container: [`Instance::internal_url`] rather than
/// `base_url`, which is the published port on the host and gives a 422 with `Clone: exit status
/// 128`. And "the migration worked" is a claim about content, not about a 201 — a migration that
/// produced an empty repository at the right name would satisfy any assertion made on the reply.
/// So the source is a repository with a real commit, and the mirror is checked to be on exactly
/// that commit.
#[test]
fn migrating_produces_a_mirror_that_syncs() {
    let inst = instance_or_skip!();
    let source = TestRepo::create_initialized(inst, "rc-migrate");
    source.api("PATCH", "", Some(r#"{"private":false}"#));
    cover!(raw: ["repoMigrate", "repoMirrorSync"]);

    // Gitea does the cloning, so this is the address *it* can reach, not the one this process
    // uses. With `base_url` the migration is a 422 naming a git exit status 128.
    let clone_addr = format!("{}/{}.git", inst.internal_url(), source.slug());
    let name = inst.unique_repo_name("rc-mirror");
    let slug = format!("{}/{name}", inst.user);

    let migrated = inst.gea([
        "raw",
        "repo",
        "migrate",
        "--clone-addr",
        &clone_addr,
        "--repo-name",
        &name,
        "--mirror=true",
        "--service",
        "git",
        "--auth-token",
        &inst.token,
        "--private=true",
        "--json",
        "full_name,mirror,empty",
    ]);
    migrated.assert_ok("gea raw repo migrate");
    let migrated = migrated.json();
    assert_eq!(migrated["full_name"], serde_json::json!(slug), "{migrated}");
    assert_eq!(
        migrated["mirror"],
        serde_json::json!(true),
        "--mirror=true did not produce a mirror: {migrated}"
    );

    // The content, not the name. A migration that made an empty repository would pass every
    // assertion above this line.
    assert_eq!(
        branch_head(inst, &slug, "main"),
        branch_head(inst, &source.slug(), "main"),
        "the mirror is not on the commit its source is on, so the clone brought nothing across"
    );

    inst.gea(["raw", "repo", "mirror-sync", &inst.user, &name])
        .assert_ok("gea raw repo mirror-sync");

    // And the operation needs a mirror: an ordinary repository has nothing to sync. Asserted by
    // exit status rather than by message, so a reworded server error does not fail this.
    let again = inst.gea(["raw", "repo", "mirror-sync", &source.owner, &source.name]);
    assert!(
        !again.ok(),
        "`mirror-sync` should refuse a repository that is not a mirror:\n{}\n{}",
        again.stdout,
        again.stderr
    );

    let _ = inst.api("DELETE", &format!("repos/{slug}"), None);
}

// ---------------------------------------------------------------- repository Actions

/// The repository runner lifecycle, which needs no runner process: the registration token is minted
/// through `gea raw`, redeemed the way `act_runner register` would, and the runner is then read,
/// disabled, filtered for and deleted — see [`gea_itest::Instance::drive_runner_lifecycle`].
///
/// Bug this prevents: a `{runner_id}` rendered into the wrong slot, which against a real server
/// is a 404 and against a mock is whatever the fixture says. The job listings ride along
/// because an empty answer is a wrapper object with a zero count, not `null` and not a 404.
#[test]
fn registering_a_runner_makes_it_visible_to_the_repository_until_it_is_deleted() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-runner");
    cover!(raw: [
        "repoCreateRunnerRegistrationToken", "getRepoRunners", "getRepoRunner",
        "updateRepoRunner", "deleteRepoRunner", "listWorkflowJobs"
    ]);

    inst.drive_runner_lifecycle("repo", "repo", &[&repo.owner, &repo.name]);

    let jobs = inst.gea(["raw", "job", "list", &repo.owner, &repo.name]);
    jobs.assert_ok("gea raw job list");
    assert_eq!(jobs.json()["total_count"], 0, "nothing has run here: {}", jobs.stdout);
}

/// Actions secrets, and the property that matters most about them: the value never comes back.
///
/// `PUT /actions/secrets/{name}` answers 201 or 204 with no body, so the only evidence a secret
/// was stored is the listing — and the only evidence it is a *secret* is that the listing does not
/// contain what was stored. Neither claim can be made against a mock, which would return whatever
/// the test told it to.
#[test]
fn a_repository_action_secret_is_listed_by_name_and_never_hands_its_value_back() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-secret");
    cover!(raw: ["updateRepoSecret", "repoListActionsSecrets", "deleteRepoSecret"]);

    const VALUE: &str = "the-value-that-must-not-come-back";
    inst.gea([
        "raw",
        "repo",
        "update-repo-secret",
        &repo.owner,
        &repo.name,
        "DEPLOY_KEY",
        "--data",
        VALUE,
    ])
    .assert_ok("gea raw repo update-repo-secret");

    let listed = inst.gea([
        "raw",
        "repo",
        "list-actions-secrets",
        &repo.owner,
        &repo.name,
        "--json",
        "name",
    ]);
    listed.assert_ok("gea raw repo list-actions-secrets");
    let names: BTreeSet<String> = listed
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert_eq!(names, set(["DEPLOY_KEY"]), "the secret is not on the repository");

    // The whole listing, unfiltered, so the check is over everything the server is willing to say
    // about the secret rather than over the one field this test asked for.
    let whole = inst.gea(["raw", "repo", "list-actions-secrets", &repo.owner, &repo.name]);
    whole.assert_ok("gea raw repo list-actions-secrets, unfiltered");
    assert!(
        !whole.stdout.contains(VALUE),
        "the secret's value came back from the server, which defeats the point of a secret"
    );

    // The same route is create and update, which is worth exercising: a PUT that 409'd on an
    // existing name would make rotating a secret impossible.
    inst.gea([
        "raw",
        "repo",
        "update-repo-secret",
        &repo.owner,
        &repo.name,
        "DEPLOY_KEY",
        "--data",
        "rotated",
    ])
    .assert_ok("gea raw repo update-repo-secret over an existing secret");

    inst.gea(["raw", "repo", "delete-repo-secret", &repo.owner, &repo.name, "DEPLOY_KEY"])
        .assert_ok("gea raw repo delete-repo-secret");
    let after = inst.gea([
        "raw",
        "repo",
        "list-actions-secrets",
        &repo.owner,
        &repo.name,
        "--json",
        "name",
    ]);
    after.assert_ok("gea raw repo list-actions-secrets after the delete");
    assert!(
        after.json().as_array().is_none_or(|a| a.is_empty()),
        "the secret survived its delete: {}",
        after.stdout
    );
}

/// Actions variables, where the server quietly rewrites the name it was given.
///
/// Three things here are only observable live, and all three would bite a caller:
///
/// * Gitea upper-cases a variable name, so `build_target` is stored as `BUILD_TARGET` — a
///   caller that compared the name it sent with the name it got back would decide the write had
///   failed.
/// * The path parameter still resolves the name as it was typed, so both spellings address the
///   same variable.
/// * The request field is `value` and the reply field is `data`, the same request-versus-reply
///   asymmetry this file records for commit statuses.
#[test]
fn a_repository_action_variable_is_upper_cased_by_the_server_and_still_resolves_by_the_name_typed()
{
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "rc-var");
    cover!(raw: [
        "createRepoVariable", "getRepoVariablesList", "getRepoVariable",
        "updateRepoVariable", "deleteRepoVariable"
    ]);

    inst.gea([
        "raw",
        "repo",
        "create-repo-variable",
        &repo.owner,
        &repo.name,
        "build_target",
        "--value",
        "x86_64",
    ])
    .assert_ok("gea raw repo create-repo-variable");

    let listed = inst.gea([
        "raw",
        "repo",
        "get-repo-variables-list",
        &repo.owner,
        &repo.name,
        "--json",
        "name,data",
    ]);
    listed.assert_ok("gea raw repo get-repo-variables-list");
    let listed = listed.json();
    assert_eq!(
        listed[0]["name"],
        serde_json::json!("BUILD_TARGET"),
        "Gitea upper-cases a variable name, and a caller that did not expect it would read this \
         as a failed write: {listed}"
    );
    assert_eq!(
        listed[0]["data"],
        serde_json::json!("x86_64"),
        "the value is carried in `data`: {listed}"
    );

    // Both spellings must address the same variable, or the name the server rewrote would be the
    // only usable one.
    for spelling in ["build_target", "BUILD_TARGET"] {
        let got = inst.gea([
            "raw",
            "repo",
            "get-repo-variable",
            &repo.owner,
            &repo.name,
            spelling,
            "--json",
            "name,data",
        ]);
        got.assert_ok(&format!("gea raw repo get-repo-variable {spelling}"));
        assert_eq!(
            got.json()["data"],
            serde_json::json!("x86_64"),
            "{spelling} did not resolve to the variable that was created: {}",
            got.stdout
        );
    }

    // `update` carries both a new value and an optional new name, and a rename goes through the
    // same upper-casing.
    inst.gea([
        "raw",
        "repo",
        "update-repo-variable",
        &repo.owner,
        &repo.name,
        "build_target",
        "--name",
        "target_arch",
        "--value",
        "aarch64",
    ])
    .assert_ok("gea raw repo update-repo-variable");
    let renamed = inst.gea([
        "raw",
        "repo",
        "get-repo-variable",
        &repo.owner,
        &repo.name,
        "target_arch",
        "--json",
        "name,data",
    ]);
    renamed.assert_ok("gea raw repo get-repo-variable after a rename");
    let renamed = renamed.json();
    assert_eq!(renamed["name"], serde_json::json!("TARGET_ARCH"), "{renamed}");
    assert_eq!(
        renamed["data"],
        serde_json::json!("aarch64"),
        "the update did not change the value: {renamed}"
    );

    inst.gea(["raw", "repo", "delete-repo-variable", &repo.owner, &repo.name, "target_arch"])
        .assert_ok("gea raw repo delete-repo-variable");
    let after = inst.gea([
        "raw",
        "repo",
        "get-repo-variables-list",
        &repo.owner,
        &repo.name,
        "--json",
        "name",
    ]);
    after.assert_ok("gea raw repo get-repo-variables-list after the delete");
    assert!(
        after.json().as_array().is_none_or(|a| a.is_empty()),
        "the variable survived its delete: {}",
        after.stdout
    );
}

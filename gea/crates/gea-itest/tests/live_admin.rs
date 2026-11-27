//! `gea admin` against a real Gitea.
//!
//! Instance administration is the group where a `FakeTransport` proves the least. Almost every
//! command here is a single call, so a mock confirms only that we built the URL we meant to
//! build — never that Gitea routes it, never that it enforces the rule we think it enforces,
//! and never that the side effect actually happened. Two classes of failure live here and
//! none of them are reachable without a server:
//!
//! * **Server-side rules with no client-side shadow.** "An account that still owns repositories cannot be deleted without `--purge`", and
//!   "`read:admin` reads but does not write" are all decided by Gitea. A mock decides them
//!   for us, which is the opposite of a test.
//! * **Asymmetric endpoint pairs.** `POST /admin/hooks` and `GET /admin/hooks` do not describe
//!   the same set — see [`a_webhook_survives_a_round_trip_but_never_reaches_the_admin_listing`].
//!   Only the server knows that.
//!
//! # Sharing one instance with eight other test binaries
//!
//! Everything here is instance-wide by nature, so no assertion counts rows. Each test names its
//! own account, organization, repository or runner via [`gea_itest::Instance::unique_repo_name`]
//! and asserts on *that* name being present or absent. A test that asserted "there are three
//! organizations" would fail the moment another file created one.
//!
//! One thing here is genuinely instance-wide: `admin cron run` runs a task chosen so that sharing
//! is safe — see [`running_the_update_checker_advances_its_execution_count`].

use gea_itest::{Instance, TestRepo, cover, instance_or_skip};

// --------------------------------------------------------------------------------- fixtures

/// An account created over the API that purges itself on drop.
///
/// Over the API rather than through `gea admin user create` on purpose: a fixture built out of
/// the command under test cannot be used to test that command, and half the tests below need an
/// account they did not have to trust `gea` to make. This is the same bargain `TestRepo` strikes.
struct TempUser<'a> {
    inst: &'a Instance,
    name: String,
}

impl<'a> TempUser<'a> {
    fn create(inst: &'a Instance, prefix: &str) -> Self {
        let name = inst.unique_repo_name(prefix);
        let (code, body) = inst.api(
            "POST",
            "admin/users",
            Some(&format!(
                r#"{{"username":"{name}","email":"{name}@example.invalid","password":"gea-itest-admin-pass-1","must_change_password":false}}"#
            )),
        );
        assert!(
            (200..300).contains(&code),
            "could not create the account {name}: HTTP {code}: {body}"
        );
        Self { inst, name }
    }

    fn email(&self) -> String {
        format!("{}@example.invalid", self.name)
    }

    /// What the server currently says about this account, read out of band.
    fn read(&self) -> (i32, serde_json::Value) {
        let (code, body) = self.inst.api("GET", &format!("users/{}", self.name), None);
        (code, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }
}

impl Drop for TempUser<'_> {
    fn drop(&mut self) {
        // `purge=true` because a test may have left repositories on the account, and a delete
        // that 422s would leave the account behind for every later run to trip over.
        let _ = self.inst.api("DELETE", &format!("admin/users/{}?purge=true", self.name), None);
    }
}

/// Real ed25519 public keys, one per test that needs one.
///
/// Gitea refuses a key already registered anywhere on the instance, so two tests sharing a
/// constant would fail whichever ran second — and only when run in parallel, which is the worst
/// possible shape for a flake. Generated once with `ssh-keygen -t ed25519`; the private halves
/// were discarded because nothing here ever authenticates with them.
const SSH_KEY_ADMIN_CREATE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILh+AqIc4PQsBzxA8nLc7jCM3wrIOPXQOchTZOx89Oqn gea-itest-admin-1@example.invalid";

/// Read one account's SSH keys out of band.
fn keys_of(inst: &Instance, user: &str) -> serde_json::Value {
    let (code, body) = inst.api("GET", &format!("users/{user}/keys"), None);
    assert_eq!(code, 200, "listing {user}'s keys: {body}");
    serde_json::from_str(&body).expect("an array of keys")
}

// ------------------------------------------------------------------------------- accounts

/// `admin user create` and `admin user edit` each send one object, and every field in it is a
/// separate chance to send the wrong thing under the right name. The edit is the dangerous half:
/// it is a `PATCH` built from whichever flags were given, so a flag wired to the wrong key
/// silently changes a *different* setting — and the command still prints the account and exits 0.
///
/// So each named setting is read back from the server, and so is one that was **not** named:
/// `visibility` must still be `public` after an edit that never mentioned it. A `PATCH` that
/// serialised its whole struct would reset it, and nothing else in the output would say so.
///
/// `--no-must-change-password` is on the create because the default leaves the account unable to
/// do anything until it picks a new password at a web sign-in, which no test can drive.
#[test]
fn creating_an_account_and_editing_it_changes_exactly_the_settings_named() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin user create", "admin user edit", "admin user list", "admin user delete"],
        hits: ["adminCreateUser", "adminEditUser", "adminSearchUsers", "adminDeleteUser"],
    );

    let name = inst.unique_repo_name("adm-crud");
    let email = format!("{name}@example.invalid");

    inst.gea([
        "admin",
        "user",
        "create",
        &name,
        "--email",
        &email,
        "--password",
        "gea-itest-admin-pass-1",
        "--no-must-change-password",
        "--full-name",
        "Before The Edit",
    ])
    .assert_ok("gea admin user create");

    let (code, before) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 200, "the account should exist after create: {before}");
    let before: serde_json::Value = serde_json::from_str(&before).expect("a user");
    assert_eq!(before["full_name"], "Before The Edit", "--full-name did not reach the server");
    assert_eq!(before["email"], email.as_str(), "--email did not reach the server");
    assert_eq!(before["is_admin"], false, "the account must not be an administrator by default");
    assert_eq!(before["active"], true, "a freshly created account must be able to sign in");
    assert_eq!(before["visibility"], "public");

    // The account has to be findable through the command as well as through the API, and
    // `--paginate` is load-bearing: other test binaries create accounts on this same instance,
    // so a new login can easily land past the first page of a default listing.
    let listed = inst.gea(["admin", "user", "list", "--paginate", "--json", "login"]);
    listed.assert_ok("gea admin user list");
    let logins: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of accounts")
        .iter()
        .map(|u| u["login"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(logins.contains(&name), "admin user list did not include {name}: {logins:?}");

    inst.gea([
        "admin",
        "user",
        "edit",
        &name,
        "--full-name",
        "After The Edit",
        "--deactivate",
        "--restrict",
        "--max-repo-creation",
        "7",
    ])
    .assert_ok("gea admin user edit");

    let (code, after) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 200, "{after}");
    let after: serde_json::Value = serde_json::from_str(&after).expect("a user");
    assert_eq!(after["full_name"], "After The Edit", "--full-name was not applied");
    assert_eq!(after["active"], false, "--deactivate was not applied");
    assert_eq!(after["restricted"], true, "--restrict was not applied");
    // The setting nobody named. A PATCH that serialised the whole option struct would have
    // reset this to the type's default and the command would still have exited 0.
    assert_eq!(after["visibility"], "public", "an unnamed setting was rewritten by the edit");
    assert_eq!(after["is_admin"], false, "an unnamed setting was rewritten by the edit");
    assert_eq!(after["email"], email.as_str(), "an unnamed setting was rewritten by the edit");

    inst.gea(["admin", "user", "delete", &name, "--yes"]).assert_ok("gea admin user delete");

    let (code, body) = inst.api("GET", &format!("users/{name}"), None);
    assert_eq!(code, 404, "the account should be gone after delete: HTTP {code}: {body}");
}

/// Gitea refuses to delete an account that still owns repositories, and that refusal is worth a
/// live test for two reasons. It is enforced entirely server-side, so a mock decides the outcome
/// itself; and the failing call is a `DELETE`, the one shape where "the command exited non-zero"
/// is not enough — the account could have been half-removed. So the account is read back after
/// the refusal and must still be intact.
///
/// It also pins `admin repo create`, which creates *on behalf of another account* rather than
/// for the caller — the one thing that distinguishes `POST /admin/users/{u}/repos` from the
/// ordinary create, and the thing a mock cannot check because ownership is assigned by the
/// server.
#[test]
fn deleting_an_account_that_still_owns_a_repository_is_refused_until_purge() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin repo create", "admin repo list", "admin user delete"],
        hits: ["adminCreateRepo", "repoSearch", "adminDeleteUser"],
    );

    let owner = TempUser::create(inst, "adm-owner");
    let repo = inst.unique_repo_name("adm-owned");

    inst.gea([
        "admin",
        "repo",
        "create",
        &repo,
        "--owner",
        &owner.name,
        "--private",
        "-d",
        "owned by somebody else",
    ])
    .assert_ok("gea admin repo create");

    // Ownership is the whole point of this endpoint and it is decided by the server, so it is
    // read back rather than inferred from the exit code.
    let (code, body) = inst.api("GET", &format!("repos/{}/{repo}", owner.name), None);
    assert_eq!(code, 200, "the repository should exist: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("a repository");
    assert_eq!(created["owner"]["login"], owner.name.as_str(), "created under the wrong account");
    assert_eq!(created["private"], true, "--private did not reach the server");
    assert_eq!(created["description"], "owned by somebody else");

    // Scoped by name rather than by owner: `admin repo list --owner` filters client-side over
    // `/repos/search`, whose `q` matches repository *names*, so it finds nothing for an owner
    // whose name is not part of the repository's. `--query` is the filter that works.
    let listed = inst.gea(["admin", "repo", "list", "--query", &repo, "--json", "full_name"]);
    listed.assert_ok("gea admin repo list --query");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of repositories")
        .iter()
        .map(|r| r["full_name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        names.contains(&format!("{}/{repo}", owner.name)),
        "admin repo list --query did not find {repo}: {names:?}"
    );

    let refused = inst.gea(["admin", "user", "delete", &owner.name, "--yes"]);
    assert!(!refused.ok(), "deleting a repository owner should be refused:\n{}", refused.stderr);
    // gea's own rendering of the class of failure, not the server's sentence — the wording of
    // "user still has ownership of repositories" is Gitea's to change.
    refused.assert_says("HTTP 422");

    let (code, still) = owner.read();
    assert_eq!(code, 200, "a refused delete must leave the account intact: {still}");
    assert_eq!(still["login"], owner.name.as_str());

    inst.gea(["admin", "user", "delete", &owner.name, "--purge", "--yes"])
        .assert_ok("gea admin user delete --purge");

    let (code, body) = owner.read();
    assert_eq!(code, 404, "--purge should have removed the account: HTTP {code}: {body}");
    let (code, body) = inst.api("GET", &format!("repos/{}/{repo}", owner.name), None);
    assert_eq!(code, 404, "--purge should have removed the repository too: HTTP {code}: {body}");
}

/// Adding an SSH key to *somebody else's* account is an admin-only route, and the only evidence
/// it worked is the key appearing on that account rather than on the caller's. A mock returns a
/// `PublicKey` either way.
///
/// The fingerprint is compared as well as the title, because a body that sent the title under
/// the right name and the key material under the wrong one would still produce a 201 and a
/// plausible-looking object.
#[test]
fn an_admin_added_ssh_key_lands_on_the_named_account_and_disappears_with_it() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminCreatePublicKey", "adminDeleteUserPublicKey"]);

    let user = TempUser::create(inst, "adm-key");

    let created = inst.gea([
        "raw",
        "admin",
        "create-public-key",
        &user.name,
        "--title",
        "gea-itest-admin-key",
        "--key",
        SSH_KEY_ADMIN_CREATE,
    ]);
    created.assert_ok("gea raw admin create-public-key");
    let key_id = created.json()["id"].as_i64().expect("the new key's id");

    let keys = keys_of(inst, &user.name);
    let listed = keys.as_array().expect("an array");
    assert_eq!(listed.len(), 1, "expected exactly one key on a fresh account: {keys}");
    assert_eq!(listed[0]["title"], "gea-itest-admin-key", "the title did not reach the server");
    assert_eq!(
        listed[0]["key"], SSH_KEY_ADMIN_CREATE,
        "the server stored different key material than we sent"
    );

    inst.gea(["raw", "admin", "delete-user-public-key", &user.name, &key_id.to_string()])
        .assert_ok("gea raw admin delete-user-public-key");

    let keys = keys_of(inst, &user.name);
    assert_eq!(
        keys.as_array().map(Vec::len),
        Some(0),
        "the key should be gone after delete-user-public-key: {keys}"
    );
}

/// `admin email search` is one leaf over **two** endpoints: a keyword goes to
/// `/admin/emails/search`, and no keyword goes to `/admin/emails`, because searching with an
/// empty keyword returns nothing on some releases and would read as "no such address" for a
/// command that was asked to list everything. That split is invisible to a mock, which answers
/// whichever one the test wires up.
///
/// Both forms are driven, and both must find the same account — which is the only way to notice
/// if the two routes ever stop agreeing.
#[test]
fn searching_for_an_address_finds_the_account_that_owns_it_by_either_route() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin email search"],
        hits: ["adminSearchEmails", "adminGetAllEmails"],
    );

    let user = TempUser::create(inst, "adm-find");

    let found = inst.gea(["admin", "email", "search", &user.name, "--json", "email,username"]);
    found.assert_ok("gea admin email search <keyword>");
    let rows = found.json();
    let rows = rows.as_array().expect("an array of addresses");
    assert_eq!(rows.len(), 1, "a unique keyword should match one address: {rows:?}");
    assert_eq!(rows[0]["email"], user.email().as_str());
    assert_eq!(
        rows[0]["username"],
        user.name.as_str(),
        "the address is attributed to the wrong account"
    );

    // No keyword: the other endpoint. `--paginate` because this lists every address on the
    // instance and other test binaries are adding accounts to it concurrently.
    let all = inst.gea(["admin", "email", "search", "--paginate", "--json", "email,username"]);
    all.assert_ok("gea admin email search with no keyword");
    let owner = all
        .json()
        .as_array()
        .expect("an array of addresses")
        .iter()
        .find(|e| e["email"].as_str() == Some(user.email().as_str()))
        .map(|e| e["username"].as_str().unwrap_or_default().to_owned());
    assert_eq!(
        owner.as_deref(),
        Some(user.name.as_str()),
        "the keyword-less listing disagrees with the search about who owns {}",
        user.email()
    );
}

/// Renaming is a `POST` that answers `204` and carries the whole result in a side effect, which
/// is the shape with the least evidence available from the response. Both ends are therefore
/// read back, and the account's id must survive: a rename that re-created the account would
/// orphan every repository, issue and token pointing at the old id.
///
/// # What the old login does afterwards, which no mock would have guessed
///
/// It does **not** 404. Gitea records the rename and answers `307` at the old path with a
/// `Location` naming the new one, so links and scripts written before the rename keep working.
/// This test asserts that redirect rather than an absence, for two reasons: writing the
/// intuitive assertion is how this test failed the first time it was run against a real server,
/// and a client that stopped following redirects would otherwise silently start reporting
/// renamed accounts as missing.
///
/// The one thing the old path must never do is answer `200` with the account inline — that
/// would mean two live logins for one account.
#[test]
fn renaming_an_account_moves_it_rather_than_copying_it() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminRenameUser", "adminSearchUsers"]);

    let user = TempUser::create(inst, "adm-rename");
    let (code, before) = user.read();
    assert_eq!(code, 200, "{before}");
    let id_before = before["id"].as_i64().expect("an account id");

    let renamed = format!("{}-moved", user.name);
    inst.gea(["raw", "admin", "rename-user", &user.name, "--new-username", &renamed])
        .assert_ok("gea raw admin rename-user");

    let (code, body) = inst.api("GET", &format!("users/{renamed}"), None);
    assert_eq!(code, 200, "the account should answer at its new name: HTTP {code}: {body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a user");
    assert_eq!(
        after["id"].as_i64(),
        Some(id_before),
        "the rename changed the account id, which would orphan everything pointing at it"
    );

    let (code, body) = user.read();
    assert_eq!(
        code, 307,
        "the old login must redirect to the new one rather than answer directly: HTTP {code}: \
         {body}"
    );
    let headers = inst.api_headers(&format!("users/{}", user.name));
    assert!(
        headers.lines().any(|h| {
            let h = h.trim();
            h.to_ascii_lowercase().starts_with("location:")
                && h.ends_with(&format!("/users/{renamed}"))
        }),
        "the redirect from the old login does not point at {renamed}:\n{headers}"
    );

    let listed = inst.gea(["raw", "admin", "search-users", "--paginate"]);
    listed.assert_ok("gea raw admin search-users");
    let logins: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of accounts")
        .iter()
        .map(|u| u["login"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(logins.contains(&renamed), "the renamed account is not listed: {logins:?}");
    assert!(!logins.contains(&user.name), "the old login is still listed: {logins:?}");

    // `TempUser`'s drop targets the old name, where the server answers a redirect that `curl`
    // is not told to follow, so the delete would not land. This one cleans up explicitly.
    let (code, body) = inst.api("DELETE", &format!("admin/users/{renamed}?purge=true"), None);
    assert!((200..300).contains(&code), "could not clean up {renamed}: HTTP {code}: {body}");
}

// --------------------------------------------------------------------------- organizations

/// `admin org create` posts to `/admin/users/{owner}/orgs`, so the owner is in the **path** and
/// the organization's own name is in the **body** — two names in one request, with nothing in the
/// reply that would look wrong if they were swapped. A mock cannot tell; the server can, because
/// it records who the first administrator is.
///
/// So ownership is read back from `/orgs/{name}/members`, not merely inferred from a 201.
#[test]
fn an_organization_is_created_under_the_account_named_in_the_path() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin org create", "admin org list"],
        hits: ["adminCreateOrg", "adminGetAllOrgs"],
    );

    let owner = TempUser::create(inst, "adm-orgown");
    let org = inst.unique_repo_name("adm-org");

    inst.gea([
        "admin",
        "org",
        "create",
        &org,
        "--owner",
        &owner.name,
        "--description",
        "made by the admin group",
        "--visibility",
        "limited",
    ])
    .assert_ok("gea admin org create");

    let (code, body) = inst.api("GET", &format!("orgs/{org}"), None);
    assert_eq!(code, 200, "the organization should exist: {body}");
    let created: serde_json::Value = serde_json::from_str(&body).expect("an organization");
    assert_eq!(created["description"], "made by the admin group");
    assert_eq!(created["visibility"], "limited", "--visibility did not reach the server");

    // The two-names-in-one-request check: the account in the path is the one that ended up
    // owning it, rather than the caller.
    let (code, body) = inst.api("GET", &format!("orgs/{org}/members"), None);
    assert_eq!(code, 200, "{body}");
    let members: serde_json::Value = serde_json::from_str(&body).expect("an array of members");
    let logins: Vec<&str> =
        members.as_array().expect("an array").iter().filter_map(|m| m["login"].as_str()).collect();
    assert!(
        logins.contains(&owner.name.as_str()),
        "the organization is not owned by the account named in the path: {logins:?}"
    );

    // `--paginate`: instance-wide, and other test binaries create organizations too.
    let listed = inst.gea(["admin", "org", "list", "--paginate", "--json", "username"]);
    listed.assert_ok("gea admin org list");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("an array of organizations")
        .iter()
        .map(|o| o["username"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&org), "admin org list did not include {org}: {names:?}");

    let (code, body) = inst.api("DELETE", &format!("orgs/{org}"), None);
    assert!((200..300).contains(&code), "could not clean up the organization: HTTP {code}: {body}");
}

// ---------------------------------------------------------------------------------- cron

/// `admin cron run` answers `204` and prints nothing, so "it exited 0" is the entire response.
/// That is exactly the situation where a request sent to a *nearly* right URL — or with the task
/// name in a query string instead of the path — looks like success. The only real evidence is on
/// the server, and `/admin/cron` happens to publish it: every task carries an `exec_times`
/// counter and a `prev` timestamp.
///
/// So the counter is read before and after, and it must have moved.
///
/// # Why `update_checker`, and why it is safe to run on a shared instance
///
/// Eight other test binaries share this Gitea, so the task had to be one that cannot disturb
/// them. `update_checker` only asks whether a newer Gitea has been released; it touches no
/// repository, no account and no database row belonging to anyone's fixtures, and the harness
/// boots the container with `OFFLINE_MODE`, so even the outbound request has nowhere to go.
/// Every other entry in the list is disqualified: `git_gc_repos`, `repo_health_check` and
/// `check_repo_stats` rewrite repositories another test may be pushing to, `archive_cleanup`,
/// `delete_repo_archives` and `cleanup_packages` delete artifacts, and `delete_missing_repos`
/// and `reinit_missing_repos` are destructive by name.
///
/// This is the one test here that changes instance-wide state and cannot put it back — a run
/// counter only goes up. It is recorded rather than restored, which is safe precisely because
/// the counter is the only thing that moves.
#[test]
fn running_the_update_checker_advances_its_execution_count() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin cron list", "admin cron run"],
        hits: ["adminCronList", "adminCronRun"],
    );

    /// Side-effect-free on a shared instance; see this test's documentation.
    const TASK: &str = "update_checker";

    let runs = |inst: &Instance| -> i64 {
        let listed = inst.gea(["admin", "cron", "list", "--json", "name,exec_times"]);
        listed.assert_ok("gea admin cron list");
        listed
            .json()
            .as_array()
            .expect("an array of tasks")
            .iter()
            .find(|t| t["name"].as_str() == Some(TASK))
            .unwrap_or_else(|| panic!("this instance has no {TASK} task"))["exec_times"]
            .as_i64()
            .expect("an execution count")
    };

    let before = runs(inst);
    inst.gea(["admin", "cron", "run", TASK, "--yes"]).assert_ok("gea admin cron run");
    let after = runs(inst);

    assert!(
        after > before,
        "{TASK} reported {before} runs before and {after} after, so the 204 did not correspond \
         to the task actually running"
    );
}

/// Two gates stand in front of `POST /admin/cron/{task}`, and both must hold without the server
/// being asked to enforce them.
///
/// An unknown task name is caught by listing the tasks first, so the user is told what this
/// instance *does* have instead of receiving a bare 404 — this is the "never swallow the real
/// reason" rule applied to a name that was simply mistyped. It is also why this test cannot
/// claim `adminCronRun`: the run is never sent.
///
/// Running a task is destructive, so without `--yes` and without a terminal to confirm at, the
/// command must refuse. A regression there would let a scripted invocation silently run
/// maintenance on a production instance.
#[test]
fn cron_run_refuses_an_unknown_task_and_refuses_to_run_unconfirmed() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["admin cron run"], hits: ["adminCronList"]);

    let unknown = inst.gea(["admin", "cron", "run", "no-such-task-at-all", "--yes"]);
    unknown.assert_code(2, "gea admin cron run with an unknown task");
    unknown.assert_says("no scheduled task");
    // The point of listing first: the message names what this instance actually has.
    unknown.assert_says("update_checker");

    // No `--yes`, and the test harness is not a terminal, so there is nowhere to confirm.
    let unconfirmed = inst.gea(["admin", "cron", "run", "update_checker"]);
    unconfirmed.assert_code(2, "gea admin cron run without --yes");
    unconfirmed.assert_says("--yes");
}

// -------------------------------------------------------------------------------- runners

/// A whole runner lifecycle without a runner process: the registration token is minted through
/// the REST API and redeemed the way `act_runner register` would, so nothing has to connect.
///
/// The runner is looked up by id rather than the listing merely being non-empty — other test
/// binaries share this instance and may register runners of their own, so counting is avoided
/// throughout — and the `--status` filter, which `gea` applies itself because the endpoint has no
/// such parameter, is checked against a runner whose status is known: registered, never seen,
/// therefore `offline`.
#[test]
fn a_registered_runner_is_listed_by_the_instance_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin runner list", "admin runner delete"],
        hits: [
            "adminCreateRunnerRegistrationToken", "getAdminRunner", "getAdminRunners",
            "updateAdminRunner", "deleteAdminRunner",
        ],
    );

    let name = inst.unique_repo_name("adm-runner");
    let id = inst.register_runner("admin", &name, &["ubuntu-latest"]);

    let fetched = inst.gea(["raw", "admin", "get-admin-runner", &id.to_string()]);
    fetched.assert_ok("gea raw admin get-admin-runner");
    let fetched = fetched.json();
    assert_eq!(fetched["name"], name.as_str(), "the id names a different runner");

    let listed = inst.gea(["admin", "runner", "list", "--json", "id,name,status"]);
    listed.assert_ok("gea admin runner list");
    let listed = listed.json();
    let mine = listed
        .as_array()
        .expect("an array of runners")
        .iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .unwrap_or_else(|| panic!("runner {id} is missing from admin runner list: {listed}"));
    assert_eq!(mine["name"], name.as_str());
    assert_eq!(mine["status"], "offline", "a runner that never connected: {mine}");

    let offline = inst.gea(["admin", "runner", "list", "--status", "offline"]);
    offline.assert_ok("gea admin runner list --status offline");
    assert!(offline.stdout.contains(&name), "{}", offline.stdout);
    let online = inst.gea(["admin", "runner", "list", "--status", "online"]);
    online.assert_ok("gea admin runner list --status online");
    assert!(!online.stdout.contains(&name), "{}", online.stdout);

    let disabled =
        inst.gea(["raw", "admin", "update-admin-runner", &id.to_string(), "--disabled=true"]);
    disabled.assert_ok("gea raw admin update-admin-runner --disabled=true");
    assert_eq!(disabled.json()["disabled"], true, "the edit did not stick: {}", disabled.stdout);
    let only_disabled = inst.gea(["admin", "runner", "list", "--disabled", "--json", "id"]);
    only_disabled.assert_ok("gea admin runner list --disabled");
    assert!(
        only_disabled.json().as_array().is_some_and(|a| a.iter().any(|r| r["id"] == id)),
        "the disabled runner is missing from --disabled: {}",
        only_disabled.stdout
    );

    inst.gea(["admin", "runner", "delete", &id.to_string(), "--yes"])
        .assert_ok("gea admin runner delete");

    let gone = inst.gea(["raw", "admin", "get-admin-runner", &id.to_string()]);
    assert!(!gone.ok(), "the runner should be gone after delete:\n{}", gone.stdout);
    gone.assert_says("HTTP 404");
}

// ------------------------------------------------------------------------------- unadopted

/// A repository directory on disk that Gitea has no database row for cannot be manufactured
/// through the API — it takes a shell on the server — so what this pins is the half that is
/// reachable: the listing works, and both mutating routes refuse a path that is not there
/// **without creating anything**.
///
/// That refusal is worth asserting rather than skipping. `POST /admin/unadopted/{o}/{r}` on a
/// missing directory answers Gitea's own JSON 404, not the router's — which is the evidence
/// that the two path segments went where the specification says they go. A route we had spelled
/// wrong would 404 too, so the second half of the check is that no repository called `{o}/{r}`
/// came into existence: an adopt that silently created a fresh repository instead of adopting an
/// existing directory would be the genuinely dangerous failure here.
///
/// A `TestRepo` is created first because `GET /admin/unadopted` walks the repository root on
/// disk, and on an instance that has never held a repository that directory does not exist and
/// the listing answers `500 lstat ... no such file or directory`. Creating one repository is
/// what makes the empty listing an empty listing rather than an error.
#[test]
fn adopting_a_directory_that_is_not_on_disk_is_refused_and_creates_nothing() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["admin adopt list", "admin adopt adopt", "admin adopt delete"],
        hits: ["adminUnadoptedList", "adminAdoptRepository", "adminDeleteUnadoptedRepository"],
    );

    // See the doc comment: this exists so the repository root exists.
    let anchor = TestRepo::create(inst, "adm-adopt-anchor");

    let listed = inst.gea(["admin", "adopt", "list", "--paginate"]);
    listed.assert_ok("gea admin adopt list");

    let missing = inst.unique_repo_name("adm-not-on-disk");
    let slug = format!("{}/{missing}", inst.user);

    let adopt = inst.gea(["admin", "adopt", "adopt", &slug, "--yes"]);
    assert!(!adopt.ok(), "adopting a directory that is not there should fail:\n{}", adopt.stdout);
    adopt.assert_says("HTTP 404");

    let (code, body) = inst.api("GET", &format!("repos/{slug}"), None);
    assert_eq!(
        code, 404,
        "a failed adopt must not have created a repository instead: HTTP {code}: {body}"
    );

    let delete = inst.gea(["admin", "adopt", "delete", &slug, "--yes"]);
    assert!(!delete.ok(), "deleting a directory that is not there should fail:\n{}", delete.stdout);
    delete.assert_says("HTTP 404");

    // The anchor is a real, adopted repository; nothing above may have disturbed it.
    let (code, body) = inst.api("GET", &format!("repos/{}", anchor.slug()), None);
    assert_eq!(code, 200, "the anchor repository was disturbed: HTTP {code}: {body}");
}

// ---------------------------------------------------------------------------- system hooks

/// The asymmetry no mock would ever reproduce: `POST /admin/hooks` creates a webhook that
/// `GET /admin/hooks` does not return.
///
/// Gitea's admin listing is filtered to *system* webhooks, while the admin create makes a
/// default one, so a webhook that exists, answers `GET /admin/hooks/{id}`, and accepts a `PATCH`
/// is nevertheless invisible to the command an operator would use to find it. A `FakeTransport`
/// test would have the listing return what the create returned, and the whole trap would stay
/// hidden until somebody could not find a hook they had just made.
///
/// So the lifecycle is driven by id, and the listing is asserted to *not* contain it. The
/// assertion is "this id is absent" rather than "the list is empty" because other test binaries
/// share the instance and may have hooks of their own.
#[test]
fn a_webhook_survives_a_round_trip_but_never_reaches_the_admin_listing() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "adminCreateHook",
        "adminGetHook",
        "adminEditHook",
        "adminListHooks",
        "adminDeleteHook",
    ]);

    // Inactive and pointed at an unroutable host: nothing should ever deliver, and `.invalid`
    // is reserved by RFC 2606 precisely so it cannot resolve.
    let created = inst.gea([
        "raw",
        "admin",
        "create-hook",
        "--type",
        "gitea",
        "--active=false",
        "--events",
        "push",
        "--config",
        r#"{"url":"http://gea-itest-admin.invalid/hook","content_type":"json"}"#,
    ]);
    created.assert_ok("gea raw admin create-hook");
    let id = created.json()["id"].as_i64().expect("the new hook's id");

    let fetched = inst.gea(["raw", "admin", "get-hook", &id.to_string()]);
    fetched.assert_ok("gea raw admin get-hook");
    let fetched = fetched.json();
    assert_eq!(fetched["active"], false, "--active=false did not reach the server");
    assert_eq!(
        fetched["config"]["url"], "http://gea-itest-admin.invalid/hook",
        "the config object was not stored as sent"
    );

    let edited =
        inst.gea(["raw", "admin", "edit-hook", &id.to_string(), "--branch-filter", "main"]);
    edited.assert_ok("gea raw admin edit-hook");
    assert_eq!(
        edited.json()["branch_filter"],
        "main",
        "the PATCH reported success without applying --branch-filter"
    );
    // Read back through a second GET: a reply echoing our own request would look identical.
    let confirmed = inst.gea(["raw", "admin", "get-hook", &id.to_string()]);
    confirmed.assert_ok("gea raw admin get-hook after the edit");
    assert_eq!(confirmed.json()["branch_filter"], "main", "the edit did not persist");

    let listed = inst.gea(["raw", "admin", "list-hooks", "--paginate"]);
    listed.assert_ok("gea raw admin list-hooks");
    let ids: Vec<i64> = listed
        .json()
        .as_array()
        .expect("an array of hooks")
        .iter()
        .filter_map(|h| h["id"].as_i64())
        .collect();
    assert!(
        !ids.contains(&id),
        "GET /admin/hooks now returns hooks created by POST /admin/hooks. That is a better \
         world, but it is a change in Gitea's behaviour and the note in this test's \
         documentation — that an admin-created hook is invisible to the admin listing — is now \
         wrong and should be removed. Listed: {ids:?}, created: {id}"
    );

    inst.gea(["raw", "admin", "delete-hook", &id.to_string()])
        .assert_ok("gea raw admin delete-hook");

    let gone = inst.gea(["raw", "admin", "get-hook", &id.to_string()]);
    assert!(!gone.ok(), "the hook should be gone after delete:\n{}", gone.stdout);
    gone.assert_says("HTTP 404");
}

// ------------------------------------------------------------------------------ action jobs

// --------------------------------------------------------------------------------- scopes

/// Gitea fixes a token's scopes when it is minted and never reports them back, so `gea` has to
/// infer from a bare `403` which scope was missing. `admin/scope.rs` does that by naming
/// `read:admin` for a read and `write:admin` for a write — and if the server's own division of
/// the admin routes ever differed from ours, the advice would send an operator to mint a token
/// that still does not work. Only a real server can settle where the line is.
///
/// Three credentials, one line each:
///
/// * an admin's token carrying only `read:admin` — reads must work and writes must not;
/// * the same token on a write — the error must name `write:admin`, not `read:admin`;
/// * an ordinary account's token with no admin scope at all — refused on a read.
///
/// The third is the one that proves site-administrator status and token scope are independent
/// checks: that account is not an administrator *and* its token lacks the scope, and `gea` must
/// still produce an actionable message rather than a bare "forbidden".
///
/// Exit code 4 throughout, because a script distinguishing "your credential is wrong" from
/// "the thing you asked for is not there" (5) keys off exactly that.
#[test]
fn a_read_only_admin_token_may_list_but_may_not_create() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["admin user list", "admin cron list", "admin user create"]);

    // Gitea has no admin route that mints a token for an account, and `/users/{name}/tokens`
    // accepts only a password, so the narrow token is minted the way a person would mint it.
    let Some(password) = inst.web_password() else {
        println!("SKIPPED: attached to an instance whose admin password this harness never had");
        return;
    };
    let token_name = inst.unique_repo_name("ro-admin");
    let out = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "30", "-u", &format!("{}:{password}", inst.user)])
        .args(["-H", "Content-Type: application/json", "-X", "POST"])
        .args(["-d", &format!(r#"{{"name":"{token_name}","scopes":["read:admin"]}}"#)])
        .arg(format!("{}/users/{}/tokens", inst.api_base(), inst.user))
        .output()
        .expect("curl should run");
    let minted: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the token route answers JSON");
    let read_only = minted["sha1"].as_str().expect("a secret").to_owned();

    inst.gea_as(&read_only, ["admin", "user", "list", "--json", "login"])
        .assert_ok("admin user list with a read:admin token");
    inst.gea_as(&read_only, ["admin", "cron", "list", "--json", "name"])
        .assert_ok("admin cron list with a read:admin token");

    let blocked = inst.unique_repo_name("adm-scope");
    let refused = inst.gea_as(
        &read_only,
        [
            "admin",
            "user",
            "create",
            &blocked,
            "--email",
            &format!("{blocked}@example.invalid"),
            "--password",
            "gea-itest-admin-pass-1",
            "--no-must-change-password",
        ],
    );
    refused.assert_code(4, "admin user create with a read-only admin token");
    refused.assert_says("write:admin");

    // The refusal has to have been real: nothing may have been created.
    let (code, body) = inst.api("GET", &format!("users/{blocked}"), None);
    assert_eq!(code, 404, "a refused create still made an account: HTTP {code}: {body}");

    // No admin scope at all, and not an administrator either. The message must still name the
    // scope to ask for, since that is the only thing the reader can act on.
    let outsider = inst
        .scoped_user("admoutsider", &["read:repository"])
        .expect("a second account with a narrow token");
    let refused = inst.gea_as(&outsider.token, ["admin", "user", "list"]);
    refused.assert_code(4, "admin user list with no admin scope");
    refused.assert_says("read:admin");

    let (code, body) =
        inst.api("DELETE", &format!("admin/users/{}?purge=true", outsider.name), None);
    assert!(
        (200..300).contains(&code),
        "could not clean up {}: HTTP {code}: {body}",
        outsider.name
    );
}

// ------------------------------------------------------------------------------------ badges

/// User badges, which Gitea can attach and detach through the API but not create: there is no
/// route that makes a badge, so a fresh instance has none to attach.
///
/// That shapes what can be asserted. The listing of a new account is an empty array, not `null`
/// and not a 404; attaching a slug that does not exist is refused — with a **500** and the
/// server's own "badge does not exist", which `gea` must relay rather than swallow; and
/// detaching one is a quiet 204, because the server deletes by slug and deleting nothing is not
/// an error. A mock would have answered all three however the fixture said.
#[test]
fn badges_can_be_listed_and_detached_but_only_existing_ones_attached() {
    let inst = instance_or_skip!();
    cover!(raw: ["adminListUserBadges", "adminAddUserBadges", "adminDeleteUserBadges"]);
    let user = TempUser::create(inst, "badged");

    let listed = inst.gea(["raw", "admin", "list-user-badges", &user.name]);
    listed.assert_ok("gea raw admin list-user-badges");
    assert_eq!(listed.json(), serde_json::json!([]), "a new account has no badges");

    let refused =
        inst.gea(["raw", "admin", "add-user-badges", &user.name, "--badge-slugs", "no-such-badge"]);
    assert!(!refused.ok(), "attaching a badge that does not exist succeeded:\n{}", refused.stdout);
    refused.assert_says("badge does not exist");

    inst.gea(["raw", "admin", "delete-user-badges", &user.name, "--badge-slugs", "no-such-badge"])
        .assert_ok("gea raw admin delete-user-badges");
}

/// The instance-wide Actions listings answer with a count beside the list, never `null`.
///
/// Other binaries share this instance and dispatch workflows, so no count is asserted — only the
/// shape, which is the part a generated client decodes and a mock would have invented.
#[test]
fn the_instance_wide_actions_listings_answer_with_a_count_and_a_list() {
    let inst = instance_or_skip!();
    cover!(raw: ["listAdminWorkflowJobs", "listAdminWorkflowRuns"]);

    for (cmd, key) in
        [("list-admin-workflow-jobs", "jobs"), ("list-admin-workflow-runs", "workflow_runs")]
    {
        let listed = inst.gea(["raw", "admin", cmd]);
        listed.assert_ok(&format!("gea raw admin {cmd}"));
        let listed = listed.json();
        assert!(listed["total_count"].is_u64(), "no count in {listed}");
        assert!(listed[key].is_array(), "no {key} array in {listed}");
    }
}

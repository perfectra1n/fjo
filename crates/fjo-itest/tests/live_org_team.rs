//! Organizations and teams, end to end against a real Forgejo.
//!
//! This is the corner of the API where a mock is least useful, because almost nothing about it
//! is addressed the way a user thinks about it. Team routes take a **numeric id**
//! (`PUT /teams/{id}/members/{u}`), organization membership *is* team membership, and the two
//! routes that publicise a membership refuse the instance admin outright — Forgejo answers
//! `403 Cannot publicize another member`, so only the member's own token can drive them. A
//! `FakeTransport` test cannot discover any of that: it returns whatever the test author
//! believed, which is exactly the belief under examination.
//!
//! Every test here therefore reads its mutation back **out of band** through `inst.api`, and
//! every test owns a freshly created organization so the suite can run on parallel threads
//! against the one shared container.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use fjo_itest::{Instance, cover, instance_or_skip};

/// A 1×1 transparent PNG, base64-encoded — the smallest thing `POST /orgs/{org}/avatar` accepts.
const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// Scopes a second account needs to act on an organization it belongs to.
///
/// `write:organization` is the one that matters: publicising your own membership is a *write*
/// to the organization, so a token minted with `read:user` alone — the obvious choice for "a
/// second warm body" — gets a 403 that looks exactly like the 403 an admin gets for trying to
/// publicise somebody else, and the test would then pass for the wrong reason.
const MEMBER_SCOPES: &[&str] = &["write:organization", "read:organization", "read:user"];

/// An organization that deletes itself, and everything Forgejo insists on deleting first.
///
/// Modelled on the harness's `TestRepo`: `Drop` rather than an explicit call, so a panicking
/// assertion still tears down instead of leaving debris the next run trips over.
struct TestOrg<'a> {
    inst: &'a Instance,
    name: String,
}

impl<'a> TestOrg<'a> {
    /// Claim a unique name without creating anything.
    ///
    /// For the tests that drive `fjo org create` itself: the cleanup has to be armed *before*
    /// the command under test runs, or a failure between creation and the end of the test
    /// leaks an organization.
    fn reserve(inst: &'a Instance, prefix: &str) -> Self {
        Self { inst, name: inst.unique_repo_name(prefix) }
    }

    fn create(inst: &'a Instance, prefix: &str) -> Self {
        Self::create_with(inst, prefix, "")
    }

    /// As [`TestOrg::create`], with extra JSON fields spliced into the creation body.
    fn create_with(inst: &'a Instance, prefix: &str, extra: &str) -> Self {
        let org = Self::reserve(inst, prefix);
        let body = format!(r#"{{"username":"{}"{extra}}}"#, org.name);
        let (code, reply) = inst.api("POST", "orgs", Some(&body));
        assert!(
            (200..300).contains(&code),
            "could not create the organization {}: HTTP {code}: {reply}",
            org.name
        );
        org
    }

    fn name(&self) -> &str {
        &self.name
    }

    /// An out-of-band call under `orgs/<name>/`.
    fn api(&self, method: &str, path: &str, body: Option<&str>) -> (i32, String) {
        self.inst.api(method, &format!("orgs/{}/{}", self.name, path.trim_start_matches('/')), body)
    }

    /// The organization as the server currently has it.
    fn get(&self) -> serde_json::Value {
        let (code, body) = self.inst.api("GET", &format!("orgs/{}", self.name), None);
        assert_eq!(code, 200, "reading back {} gave HTTP {code}: {body}", self.name);
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("{} was not JSON ({e}): {body}", self.name))
    }
}

impl Drop for TestOrg<'_> {
    fn drop(&mut self) {
        // Forgejo answers `DELETE /orgs/{org}` with `500 user still has ownership of
        // repositories` while any repository remains, so they go first. Without this, one test
        // that creates an organization repository leaks the whole organization — and the leak
        // is silent, because `Drop` cannot fail.
        let (_, body) = self.inst.api("GET", &format!("orgs/{}/repos?limit=100", self.name), None);
        if let Ok(serde_json::Value::Array(repos)) =
            serde_json::from_str::<serde_json::Value>(&body)
        {
            for repo in repos {
                if let Some(full) = repo["full_name"].as_str() {
                    let _ = self.inst.api("DELETE", &format!("repos/{full}"), None);
                }
            }
        }
        let _ = self.inst.api("DELETE", &format!("orgs/{}", self.name), None);
    }
}

/// Create a team through `fjo raw org create-team` and return the id the server assigned.
///
/// The id is the whole reason a helper exists: every other team route is addressed by it, and
/// it is only ever knowable from the server's reply.
fn create_team(inst: &Instance, org: &str, name: &str, permission: &str, units: &[&str]) -> i64 {
    let mut args =
        vec!["raw", "org", "create-team", org, "--name", name, "--permission", permission];
    for unit in units {
        args.push("--units");
        args.push(unit);
    }
    let run = inst.fjo(args);
    run.assert_ok(&format!("fjo raw org create-team {name}"));
    let team = run.json();
    team["id"].as_i64().unwrap_or_else(|| panic!("no team id in {}", run.stdout))
}

/// A team's `units_map` without the units it is kept out of (`none`): what it can actually reach.
fn reachable_units(team: &serde_json::Value) -> BTreeMap<String, String> {
    team["units_map"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(unit, level)| Some((unit.clone(), level.as_str()?.to_owned())))
        .filter(|(_, level)| level != "none")
        .collect()
}

/// The values of `key` across a JSON array, as a set — the shape almost every listing assertion
/// here wants, and one that does not care what order the server chose.
fn field_set(value: &serde_json::Value, key: &str) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected a JSON array, got {value}"))
        .iter()
        .filter_map(|v| v[key].as_str().map(str::to_owned))
        .collect()
}

// ------------------------------------------------------------------------ organization CRUD

/// Bug this prevents: `org create` reporting success while the optional fields — the ones a
/// self-hosted admin actually fills in — never reach the server, and `org delete` reporting a
/// deletion that did not happen.
///
/// A mock cannot catch either: it decides for itself what `POST /orgs` returns, so an omitted
/// `location` comes back looking however the fixture says it comes back. Only the server can
/// say what it stored.
#[test]
fn creating_an_organization_stores_every_field_and_deleting_it_removes_it() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["org create", "org view", "org delete"], hits: ["orgCreate", "orgGet", "orgDelete"]);

    let org = TestOrg::reserve(inst, "orgcrud");
    inst.fjo([
        "org",
        "create",
        org.name(),
        "--full-name",
        "Anvil Works",
        "-d",
        "makes anvils",
        "--visibility",
        "limited",
        "--website",
        "https://anvil.example.invalid",
        "--location",
        "Toontown",
        "--email",
        "hi@anvil.example.invalid",
    ])
    .assert_ok("fjo org create");

    let stored = org.get();
    assert_eq!(stored["username"], org.name(), "{stored}");
    assert_eq!(stored["full_name"], "Anvil Works", "the display name was dropped: {stored}");
    assert_eq!(stored["description"], "makes anvils", "the description was dropped: {stored}");
    assert_eq!(stored["visibility"], "limited", "the visibility was dropped: {stored}");
    assert_eq!(
        stored["website"], "https://anvil.example.invalid",
        "the website was dropped: {stored}"
    );
    assert_eq!(stored["location"], "Toontown", "the location was dropped: {stored}");
    assert_eq!(
        stored["email"], "hi@anvil.example.invalid",
        "the contact address was dropped: {stored}"
    );

    inst.fjo(["org", "view", org.name()])
        .assert_ok("fjo org view")
        .assert_says("Anvil Works")
        .assert_says("Toontown");

    inst.fjo(["org", "delete", org.name(), "--yes"]).assert_ok("fjo org delete --yes");
    let (code, body) = inst.api("GET", &format!("orgs/{}", org.name()), None);
    assert_eq!(code, 404, "the organization was reported deleted but is still there: {body}");
}

/// The expensive bug, and the reason `org edit` is a read-modify-write:
/// `EditOrgOption`'s fields are plain `String`s, so an omitted flag serialises as `""` and
/// **clears** the value. `org edit -d x` blanking the website, full name and email is a silent
/// data loss that only the server can testify to.
#[test]
fn editing_an_organization_changes_only_the_field_that_was_named() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["org edit"], hits: ["orgEdit", "orgGet"]);

    let org = TestOrg::create_with(
        inst,
        "orgedit",
        r#","full_name":"Before","description":"old","website":"https://before.example.invalid","location":"Elsewhere","visibility":"limited""#,
    );

    inst.fjo(["org", "edit", org.name(), "-d", "after"]).assert_ok("fjo org edit -d");

    let stored = org.get();
    assert_eq!(stored["description"], "after", "the named field was not changed: {stored}");
    assert_eq!(stored["full_name"], "Before", "org edit blanked the display name: {stored}");
    assert_eq!(
        stored["website"], "https://before.example.invalid",
        "org edit blanked the website: {stored}"
    );
    assert_eq!(stored["location"], "Elsewhere", "org edit blanked the location: {stored}");
    assert_eq!(stored["visibility"], "limited", "org edit reset the visibility: {stored}");
}

/// Three different questions, three different routes — `/user/orgs`, `/orgs` and
/// `/users/{u}/orgs`. Bug this prevents: `--all` quietly answering from `/user/orgs`, which on
/// an instance you administer looks like the instance has almost no organizations.
///
/// The raw operations are driven alongside the porcelain rather than merely credited, so the
/// claim that each porcelain form reaches its own endpoint is checked rather than asserted.
#[test]
fn a_new_organization_appears_in_every_listing_route() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["org list"], hits: ["orgGetAll", "orgListCurrentUserOrgs", "orgListUserOrgs"]);

    let org = TestOrg::create(inst, "orglist");
    let user = inst.user.clone();

    // `--limit` is load-bearing: the default stops at 30 items, and sibling tests on other
    // threads are creating organizations of their own the whole time this one runs.
    for args in [
        vec!["org", "list", "--limit", "500", "--json", "username"],
        vec!["org", "list", "--all", "--limit", "500", "--json", "username"],
        vec!["org", "list", "--user", user.as_str(), "--limit", "500", "--json", "username"],
    ] {
        let label = args.join(" ");
        let run = inst.fjo(args);
        run.assert_ok(&format!("fjo {label}"));
        assert!(
            field_set(&run.json(), "username").contains(org.name()),
            "fjo {label} did not list {}: {}",
            org.name(),
            run.stdout
        );
    }

    for args in [
        vec!["raw", "org", "get-all", "--paginate", "--limit", "500"],
        vec!["raw", "org", "list-current-user-orgs", "--paginate", "--limit", "500"],
        vec!["raw", "org", "list-user-orgs", user.as_str(), "--paginate", "--limit", "500"],
    ] {
        let label = args.join(" ");
        let run = inst.fjo(args);
        run.assert_ok(&format!("fjo {label}"));
        assert!(
            field_set(&run.json(), "username").contains(org.name()),
            "fjo {label} did not list {}: {}",
            org.name(),
            run.stdout
        );
    }
}

/// `POST /orgs/{org}/rename` answers `204` with an empty body, so the only evidence the rename
/// happened is what the two names resolve to afterwards. A mock that returns 204 proves the
/// request was formed; it cannot prove the server moved anything.
///
/// It also records a server behaviour worth knowing before someone writes a client that assumes
/// otherwise: the old name does **not** become a 404. Forgejo keeps a redirect record, so
/// `GET /orgs/<old>` answers `307` pointing at the new name — which means a client that treats
/// "still resolves" as "the rename failed" would be wrong, and one that follows redirects
/// silently would keep working under the stale name for ever.
#[test]
fn renaming_an_organization_moves_it_and_leaves_nothing_at_the_old_name() {
    let inst = instance_or_skip!();
    cover!(raw: ["renameOrg", "orgGet"]);

    let mut org = TestOrg::create(inst, "orgrename");
    let old = org.name.clone();
    let new = inst.unique_repo_name("orgrenamed");

    inst.fjo(["raw", "org", "rename-org", old.as_str(), "--new-name", new.as_str()])
        .assert_ok("fjo raw org rename-org");
    // Re-point the cleanup before asserting anything: an assertion that fires now must not also
    // leak the organization under its new name.
    org.name = new.clone();

    let run = inst.fjo(["raw", "org", "get", new.as_str()]);
    run.assert_ok("fjo raw org get, after the rename");
    assert_eq!(run.json()["username"], new.as_str(), "{}", run.stdout);

    let (code, body) = inst.api("GET", &format!("orgs/{old}"), None);
    assert_eq!(code, 307, "the old name should redirect after a rename, not serve: {body}");
    assert!(
        body.contains(&new),
        "the redirect left at the old name does not point at the new one: {body}"
    );
}

// ------------------------------------------------------------------------------- membership

/// Forgejo has no "add a member to an organization" route at all: membership *is* team
/// membership. This drives that shape and then asks every route that reports on a member
/// whether it agrees — including `DELETE /orgs/{org}/members/{u}`, which is deliberately not
/// symmetric with the add (it removes the user from the organization and from every team in
/// it at once).
#[test]
fn org_membership_is_reported_by_every_route_that_answers_for_it() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "orgCreateTeam",
        "orgAddTeamMember",
        "orgIsMember",
        "orgListMembers",
        "orgGetUserPermissions",
        "orgDeleteMember",
        "orgDeleteTeam",
    ]);

    let org = TestOrg::create(inst, "orgmembers");
    let member = inst.scoped_user("orgmemb", MEMBER_SCOPES).expect("a second account");
    let id = create_team(inst, org.name(), "Dev", "write", &["repo.code", "repo.issues"]);
    let id = id.to_string();

    inst.fjo(["raw", "org", "is-member", org.name(), member.name.as_str()])
        .assert_code(5, "is-member before the user was added");

    inst.fjo(["raw", "org", "add-team-member", id.as_str(), member.name.as_str()])
        .assert_ok("fjo raw org add-team-member");

    inst.fjo(["raw", "org", "is-member", org.name(), member.name.as_str()])
        .assert_ok("fjo raw org is-member, after joining a team");

    let run = inst.fjo(["raw", "org", "list-members", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-members");
    assert!(
        field_set(&run.json(), "login").contains(&member.name),
        "team membership did not make {} an organization member: {}",
        member.name,
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "get-user-permissions", member.name.as_str(), org.name()]);
    run.assert_ok("fjo raw org get-user-permissions");
    let perms = run.json();
    assert_eq!(perms["can_write"], true, "a `write` team gave no write permission: {perms}");
    assert_eq!(perms["is_owner"], false, "joining `Dev` made the user an owner: {perms}");

    // The asymmetry under test: one call removes them from the organization *and* the team.
    inst.fjo(["raw", "org", "delete-member", org.name(), member.name.as_str()])
        .assert_ok("fjo raw org delete-member");
    inst.fjo(["raw", "org", "is-member", org.name(), member.name.as_str()])
        .assert_code(5, "is-member after removal");
    let (code, body) = inst.api("GET", &format!("teams/{id}/members/{}", member.name), None);
    assert_eq!(code, 404, "delete-member left the user in the team: {body}");

    inst.fjo(["raw", "org", "delete-team", id.as_str()]).assert_ok("fjo raw org delete-team");
}

/// The rule no mock would have guessed: **an admin cannot publicise somebody else's
/// membership.** Forgejo answers `403 Cannot publicize another member`, so these two routes are
/// only reachable with the member's own token — which is why this test mints one.
///
/// `orgListPublicMembers` is driven but deliberately not used as the oracle: Forgejo shows a
/// *member* the full roster from that route regardless of visibility, so it cannot distinguish
/// public from concealed. `orgIsPublicMember` can, and does.
#[test]
fn a_member_can_publicise_and_conceal_their_own_membership() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "orgCreateTeam",
        "orgAddTeamMember",
        "orgPublicizeMember",
        "orgIsPublicMember",
        "orgListPublicMembers",
        "orgConcealMember",
    ]);

    let org = TestOrg::create(inst, "orgpublic");
    let member = inst.scoped_user("orgpub", MEMBER_SCOPES).expect("a second account");
    let id = create_team(inst, org.name(), "Dev", "write", &["repo.code"]).to_string();
    inst.fjo(["raw", "org", "add-team-member", id.as_str(), member.name.as_str()])
        .assert_ok("fjo raw org add-team-member");

    inst.fjo(["raw", "org", "is-public-member", org.name(), member.name.as_str()])
        .assert_code(5, "is-public-member before publicising");

    // The admin is refused, and that refusal is the documented behaviour rather than a bug.
    // Asserted on the exit code and a stable substring, never on the server's whole sentence.
    inst.fjo(["raw", "org", "publicize-member", org.name(), member.name.as_str()])
        .assert_code(1, "publicize-member as somebody other than the member")
        .assert_says("403");

    inst.fjo_as(
        &member.token,
        ["raw", "org", "publicize-member", org.name(), member.name.as_str()],
    )
    .assert_ok("fjo raw org publicize-member, as the member");
    inst.fjo(["raw", "org", "is-public-member", org.name(), member.name.as_str()])
        .assert_ok("fjo raw org is-public-member, after publicising");

    let run =
        inst.fjo(["raw", "org", "list-public-members", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-public-members");
    assert!(
        field_set(&run.json(), "login").contains(&member.name),
        "the public roster does not name {}: {}",
        member.name,
        run.stdout
    );

    inst.fjo_as(&member.token, ["raw", "org", "conceal-member", org.name(), member.name.as_str()])
        .assert_ok("fjo raw org conceal-member, as the member");
    inst.fjo(["raw", "org", "is-public-member", org.name(), member.name.as_str()])
        .assert_code(5, "is-public-member after concealing");
}

/// `org member add` takes `--team` because there is no other way in, and the name has to be
/// resolved to an id first. Bug this prevents: the resolution picking the wrong team — every
/// organization ships with `Owners`, so a lookup that silently falls back to the first team
/// makes the new user an owner.
#[test]
fn org_member_add_puts_the_user_in_the_named_team_and_remove_takes_them_out() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["org member add", "org member list", "org member remove"], hits: [
        "orgListTeams",
        "orgAddTeamMember",
        "orgListMembers",
        "orgListPublicMembers",
        "orgDeleteMember",
    ]);

    let org = TestOrg::create(inst, "orgmemcmd");
    let member = inst.scoped_user("orgmemc", MEMBER_SCOPES).expect("a second account");
    let dev = create_team(inst, org.name(), "Dev", "write", &["repo.code"]).to_string();

    inst.fjo(["org", "member", "add", org.name(), member.name.as_str(), "--team", "Dev"])
        .assert_ok("fjo org member add --team Dev");

    // The team they landed in, read back out of band — the assertion the `Owners` fallback bug
    // would fail.
    let (code, body) = inst.api("GET", &format!("teams/{dev}/members/{}", member.name), None);
    assert_eq!(code, 200, "the user is not in Dev: HTTP {code}: {body}");
    let owners: serde_json::Value = {
        let (_, teams) = org.api("GET", "teams", None);
        serde_json::from_str(&teams).expect("the team list")
    };
    let owners_id = owners
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "Owners"))
        .and_then(|t| t["id"].as_i64())
        .expect("every organization has an Owners team");
    let (code, body) = inst.api("GET", &format!("teams/{owners_id}/members/{}", member.name), None);
    assert_eq!(code, 404, "org member add made the user an owner: HTTP {code}: {body}");

    for extra in [None, Some("--public")] {
        let mut args =
            vec!["org", "member", "list", org.name(), "--limit", "100", "--json", "login"];
        args.extend(extra);
        let label = args.join(" ");
        let run = inst.fjo(args);
        run.assert_ok(&format!("fjo {label}"));
        assert!(
            field_set(&run.json(), "login").contains(&member.name),
            "fjo {label} did not list {}: {}",
            member.name,
            run.stdout
        );
    }

    inst.fjo(["org", "member", "remove", org.name(), member.name.as_str(), "--yes"])
        .assert_ok("fjo org member remove --yes");
    let (code, body) =
        inst.api("GET", &format!("orgs/{}/members/{}", org.name(), member.name), None);
    assert_eq!(code, 404, "the member was reported removed but is still there: {body}");
}

// ------------------------------------------------------------------------------------ teams

/// The team routes are addressed by a numeric id that only the server can hand out, and
/// `PATCH /teams/{id}` takes a body whose `name` is required and whose other fields are not
/// optional — so a partial edit is a read-modify-write or it is data loss. This drives the
/// whole raw lifecycle and checks the id-addressed reads agree with the org-scoped listing.
#[test]
fn a_teams_raw_lifecycle_is_visible_through_every_team_route() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "orgCreateTeam",
        "orgGetTeam",
        "orgListTeams",
        "teamSearch",
        "orgEditTeam",
        "orgListTeamActivityFeeds",
        "orgDeleteTeam",
    ]);

    let org = TestOrg::create(inst, "teamraw");
    let id =
        create_team(inst, org.name(), "Raw", "write", &["repo.code", "repo.issues"]).to_string();

    let run = inst.fjo(["raw", "org", "get-team", id.as_str()]);
    run.assert_ok("fjo raw org get-team");
    let team = run.json();
    assert_eq!(team["name"], "Raw", "{team}");
    assert_eq!(team["permission"], "write", "the permission was not stored: {team}");
    assert_eq!(
        team["units"].as_array().map(Vec::len),
        Some(2),
        "the unit list was not stored: {team}"
    );

    let run = inst.fjo(["raw", "org", "list-teams", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-teams");
    let listed = field_set(&run.json(), "name");
    assert!(listed.contains("Raw"), "the new team is missing from the listing: {}", run.stdout);
    assert!(listed.contains("Owners"), "the built-in Owners team is missing: {}", run.stdout);

    // `teamSearch` is the only server-side filter, and — unlike every other listing here — it
    // wraps its results in an `{data, ok}` envelope. Forgetting to unwrap that is a bug a mock
    // would only catch if the fixture already knew about the envelope.
    let run = inst.fjo(["raw", "team", "search", org.name(), "--q", "Raw"]);
    run.assert_ok("fjo raw team search");
    let found = run.json();
    assert_eq!(found["ok"], true, "{found}");
    assert_eq!(field_set(&found["data"], "name"), BTreeSet::from(["Raw".to_owned()]), "{found}");

    inst.fjo([
        "raw",
        "org",
        "edit-team",
        id.as_str(),
        "--name",
        "Raw",
        "--description",
        "edited",
        "--permission",
        "read",
        "--units",
        "repo.code",
    ])
    .assert_ok("fjo raw org edit-team");
    let (code, body) = inst.api("GET", &format!("teams/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let team: serde_json::Value = serde_json::from_str(&body).expect("a team");
    assert_eq!(team["description"], "edited", "the edit did not reach the server: {team}");
    assert_eq!(team["permission"], "read", "the permission was not changed: {team}");

    // Nothing has happened in this team, so an empty feed is the right answer; what is under
    // test is that the id-addressed route composes and decodes at all.
    let run = inst.fjo(["raw", "org", "list-team-activity-feeds", id.as_str()]);
    run.assert_ok("fjo raw org list-team-activity-feeds");
    assert!(run.json().is_array(), "the feed was not a JSON array: {}", run.stdout);

    inst.fjo(["raw", "org", "delete-team", id.as_str()]).assert_ok("fjo raw org delete-team");
    let (code, body) = inst.api("GET", &format!("teams/{id}"), None);
    assert_eq!(code, 404, "the team was reported deleted but is still there: {body}");
}

/// The porcelain half, and the bug it exists to prevent: `team edit -d x` renaming the team to
/// `""`, clearing its unit list and dropping its permission to the default, because
/// `EditTeamOption` cannot express "leave this alone". Removing every unit makes a team's
/// repositories invisible to its members — silent, and only visible on the server.
#[test]
fn editing_a_team_keeps_the_units_and_permission_it_was_not_asked_to_change() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["team create", "team view", "team edit", "team list", "team delete"], hits: [
        "orgCreateTeam",
        "orgListTeams",
        "teamSearch",
        "orgEditTeam",
        "orgDeleteTeam",
    ]);

    let org = TestOrg::create(inst, "teamcmd");
    inst.fjo([
        "team",
        "create",
        org.name(),
        "Dev",
        "-d",
        "the developers",
        "--permission",
        "write",
        "--unit",
        "repo.code",
        "--unit",
        "repo.issues",
    ])
    .assert_ok("fjo team create");

    let (_, body) = org.api("GET", "teams", None);
    let teams: serde_json::Value = serde_json::from_str(&body).expect("the team list");
    let created = teams
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "Dev"))
        .unwrap_or_else(|| panic!("fjo team create did not create Dev: {body}"))
        .clone();
    assert_eq!(created["permission"], "write", "the permission was dropped: {created}");
    let units: BTreeSet<&str> =
        created["units"].as_array().expect("units").iter().filter_map(|u| u.as_str()).collect();
    assert_eq!(
        units,
        BTreeSet::from(["repo.code", "repo.issues"]),
        "the unit list was dropped: {created}"
    );

    inst.fjo(["team", "view", org.name(), "Dev"])
        .assert_ok("fjo team view")
        .assert_says("the developers")
        .assert_says("repo.issues");

    inst.fjo(["team", "edit", org.name(), "Dev", "-d", "renamed the description only"])
        .assert_ok("fjo team edit -d");

    let id = created["id"].as_i64().expect("a team id");
    let (code, body) = inst.api("GET", &format!("teams/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a team");
    assert_eq!(after["description"], "renamed the description only", "{after}");
    assert_eq!(after["name"], "Dev", "team edit renamed the team to something else: {after}");
    assert_eq!(after["permission"], "write", "team edit reset the permission: {after}");
    let units: BTreeSet<&str> =
        after["units"].as_array().expect("units").iter().filter_map(|u| u.as_str()).collect();
    assert_eq!(
        units,
        BTreeSet::from(["repo.code", "repo.issues"]),
        "team edit cleared the unit list, hiding the team's repositories from its members: {after}"
    );

    // And the two flags that change access do change it. Forgejo derives a below-admin team's
    // level from `units_map` and ignores the `units` list beside one, so before fjo sent the map
    // carrying the change, both of these exited 0 and changed nothing.
    let read_team = |id: i64| -> serde_json::Value {
        let (code, body) = inst.api("GET", &format!("teams/{id}"), None);
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).expect("a team")
    };
    inst.fjo(["team", "edit", org.name(), "Dev", "--permission", "read"])
        .assert_ok("fjo team edit --permission read");
    let after = read_team(id);
    assert_eq!(after["permission"], "read", "--permission was ignored: {after}");
    assert_eq!(
        reachable_units(&after),
        BTreeMap::from([
            ("repo.code".to_owned(), "read".to_owned()),
            ("repo.issues".to_owned(), "read".to_owned()),
        ]),
        "--permission did not re-level the units the team reaches: {after}"
    );

    inst.fjo(["team", "edit", org.name(), "Dev", "--unit", "repo.wiki", "--permission", "write"])
        .assert_ok("fjo team edit --unit --permission");
    let after = read_team(id);
    assert_eq!(after["permission"], "write", "{after}");
    assert_eq!(
        reachable_units(&after),
        BTreeMap::from([("repo.wiki".to_owned(), "write".to_owned())]),
        "--unit did not replace the units the team reaches: {after}"
    );

    // Bare list, then the filtered form — which is a different route (`teamSearch`) behind one
    // command, so both are driven.
    let run = inst.fjo(["team", "list", org.name(), "--limit", "100", "--json", "name"]);
    run.assert_ok("fjo team list");
    assert!(field_set(&run.json(), "name").contains("Dev"), "{}", run.stdout);
    let run = inst.fjo(["team", "list", org.name(), "Dev", "--limit", "100", "--json", "name"]);
    run.assert_ok("fjo team list <query>");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from(["Dev".to_owned()]),
        "{}",
        run.stdout
    );

    inst.fjo(["team", "delete", org.name(), "Dev", "--yes"]).assert_ok("fjo team delete --yes");
    let (code, body) = inst.api("GET", &format!("teams/{id}"), None);
    assert_eq!(code, 404, "the team was reported deleted but is still there: {body}");
}

/// Adding somebody to a team is how they join the organization, so this checks both facts at
/// once. Bug this prevents: `team member remove` resolving the team name to the wrong id and
/// removing nobody, which exits 0 and looks exactly like success.
#[test]
fn team_membership_appears_on_the_member_routes_and_goes_away_on_removal() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["team member add", "team member list", "team member remove"], hits: [
        "orgAddTeamMember",
        "orgListTeamMembers",
        "orgRemoveTeamMember",
    ]);
    cover!(raw: ["orgListTeamMember"]);

    let org = TestOrg::create(inst, "teammem");
    let member = inst.scoped_user("teammem", MEMBER_SCOPES).expect("a second account");
    let id = create_team(inst, org.name(), "Dev", "write", &["repo.code"]).to_string();

    inst.fjo(["team", "member", "add", org.name(), "Dev", member.name.as_str()])
        .assert_ok("fjo team member add");

    let run = inst.fjo([
        "team",
        "member",
        "list",
        org.name(),
        "Dev",
        "--limit",
        "100",
        "--json",
        "login",
    ]);
    run.assert_ok("fjo team member list");
    assert_eq!(
        field_set(&run.json(), "login"),
        BTreeSet::from([member.name.clone()]),
        "{}",
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "list-team-member", id.as_str(), member.name.as_str()]);
    run.assert_ok("fjo raw org list-team-member");
    assert_eq!(run.json()["login"], member.name.as_str(), "{}", run.stdout);

    // Team membership is organization membership; checked out of band so this is the server's
    // opinion rather than the command's.
    let (code, body) =
        inst.api("GET", &format!("orgs/{}/members/{}", org.name(), member.name), None);
    assert_eq!(code, 204, "joining a team did not make the user an org member: {code} {body}");

    inst.fjo(["team", "member", "remove", org.name(), "Dev", member.name.as_str()])
        .assert_ok("fjo team member remove");
    let (code, body) = inst.api("GET", &format!("teams/{id}/members/{}", member.name), None);
    assert_eq!(code, 404, "the member was reported removed but is still in the team: {body}");
}

// ---------------------------------------------------------------------- team repositories

/// A team can only be given repositories owned by **its own organization**, and the route names
/// the owner separately (`PUT /teams/{id}/repos/{org}/{repo}`). That is why the repositories
/// here are created through `POST /orgs/{org}/repos` rather than the harness's `TestRepo`,
/// which creates under the current user — a repository owned by the admin cannot be added to
/// an organization's team at all.
///
/// Both creation routes are driven: the current one and the deprecated `POST /org/{org}/repos`,
/// which differs from it by a single character and is exactly the sort of path a spec bump can
/// silently break.
#[test]
fn a_team_reaches_only_the_repositories_it_has_been_given() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["team repo add", "team repo list", "team repo remove"], hits: [
        "orgAddTeamRepository",
        "orgListTeamRepos",
        "orgRemoveTeamRepository",
    ]);
    cover!(raw: [
        "createOrgRepo",
        "createOrgRepoDeprecated",
        "orgListRepos",
        "orgListTeamRepo",
        "orgAddTeamRepository",
        "orgRemoveTeamRepository",
        "orgListActivityFeeds",
    ]);

    let org = TestOrg::create(inst, "teamrepo");
    let id = create_team(inst, org.name(), "Dev", "write", &["repo.code"]).to_string();

    let granted = inst.unique_repo_name("granted");
    let withheld = inst.unique_repo_name("withheld");
    inst.fjo([
        "raw",
        "org",
        "create-org-repo",
        org.name(),
        "--name",
        granted.as_str(),
        "--private=true",
        "--auto-init=true",
        "--default-branch",
        "main",
    ])
    .assert_ok("fjo raw org create-org-repo");
    inst.fjo([
        "raw",
        "org",
        "create-org-repo-deprecated",
        org.name(),
        "--name",
        withheld.as_str(),
        "--private=true",
    ])
    .assert_ok("fjo raw org create-org-repo-deprecated");

    let run = inst.fjo(["raw", "org", "list-repos", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-repos");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from([granted.clone(), withheld.clone()]),
        "both creation routes should have produced a repository: {}",
        run.stdout
    );

    inst.fjo(["team", "repo", "add", org.name(), "Dev", granted.as_str()])
        .assert_ok("fjo team repo add");

    let run =
        inst.fjo(["team", "repo", "list", org.name(), "Dev", "--limit", "100", "--json", "name"]);
    run.assert_ok("fjo team repo list");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from([granted.clone()]),
        "the team should reach exactly the one repository it was given: {}",
        run.stdout
    );

    inst.fjo(["raw", "org", "list-team-repo", id.as_str(), org.name(), granted.as_str()])
        .assert_ok("fjo raw org list-team-repo, for a granted repository");
    inst.fjo(["raw", "org", "list-team-repo", id.as_str(), org.name(), withheld.as_str()])
        .assert_code(5, "list-team-repo for a repository the team was never given");

    // The raw pair, on the repository the porcelain did not touch, so add and remove are both
    // proved on the route the porcelain merely wraps.
    inst.fjo(["raw", "org", "add-team-repository", id.as_str(), org.name(), withheld.as_str()])
        .assert_ok("fjo raw org add-team-repository");
    inst.fjo(["raw", "org", "list-team-repo", id.as_str(), org.name(), withheld.as_str()])
        .assert_ok("fjo raw org list-team-repo, after the raw grant");
    inst.fjo(["raw", "org", "remove-team-repository", id.as_str(), org.name(), withheld.as_str()])
        .assert_ok("fjo raw org remove-team-repository");
    inst.fjo(["raw", "org", "list-team-repo", id.as_str(), org.name(), withheld.as_str()])
        .assert_code(5, "list-team-repo after the raw revocation");

    inst.fjo(["team", "repo", "remove", org.name(), "Dev", granted.as_str()])
        .assert_ok("fjo team repo remove");
    let (code, body) = inst.api("GET", &format!("teams/{id}/repos/{}/{granted}", org.name()), None);
    assert_eq!(code, 404, "the team still reaches the repository it lost: {body}");

    // Creating those two repositories is an organization activity, so the feed gets something in
    // it — which is the only way to tell a feed route that works from one that always answers
    // `[]`. Since Forgejo 17 the feed is written by a background queue (`services/feed`
    // `notifyAll` pushes a `QueueableAction`), so the entry lands *after* the creation answers;
    // read until it appears rather than once.
    let deadline = Instant::now() + FEED_DEADLINE;
    loop {
        let run = inst.fjo(["raw", "org", "list-activity-feeds", org.name()]);
        run.assert_ok("fjo raw org list-activity-feeds");
        if field_set(&run.json(), "op_type").contains("create_repo") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "after {FEED_DEADLINE:?}, the organization's feed still does not record the two \
             repository creations: {}",
            run.stdout
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// How long the activity feed's background queue gets to record an action. It is milliseconds on
/// an idle instance; the rest is headroom for a loaded CI runner.
const FEED_DEADLINE: Duration = Duration::from_secs(30);

// ----------------------------------------------------------------------- labels and hooks

/// Organization-wide labels are a different route from repository labels and use a different
/// id space. Bug this prevents: `edit-label` sending the color with its leading `#` intact, or
/// dropping the description because it was not named — both invisible without reading back.
#[test]
fn an_org_label_survives_a_round_trip_through_every_label_route() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgCreateLabel", "orgListLabels", "orgGetLabel", "orgEditLabel", "orgDeleteLabel"]);

    let org = TestOrg::create(inst, "orglabel");
    let run = inst.fjo([
        "raw",
        "org",
        "create-label",
        org.name(),
        "--name",
        "triage",
        "--color",
        "#00aabb",
        "--description",
        "needs a look",
    ]);
    run.assert_ok("fjo raw org create-label");
    let label = run.json();
    // Forgejo stores the colour without the `#` it accepts on the way in.
    assert_eq!(label["color"], "00aabb", "{label}");
    let id = label["id"].as_i64().expect("a label id").to_string();

    let run = inst.fjo(["raw", "org", "list-labels", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-labels");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from(["triage".to_owned()]),
        "{}",
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "get-label", org.name(), id.as_str()]);
    run.assert_ok("fjo raw org get-label");
    assert_eq!(run.json()["description"], "needs a look", "{}", run.stdout);

    inst.fjo([
        "raw",
        "org",
        "edit-label",
        org.name(),
        id.as_str(),
        "--name",
        "triaged",
        "--color",
        "#112233",
    ])
    .assert_ok("fjo raw org edit-label");
    let (code, body) = org.api("GET", &format!("labels/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a label");
    assert_eq!(after["name"], "triaged", "{after}");
    assert_eq!(after["color"], "112233", "{after}");

    inst.fjo(["raw", "org", "delete-label", org.name(), id.as_str()])
        .assert_ok("fjo raw org delete-label");
    let (code, body) = org.api("GET", &format!("labels/{id}"), None);
    assert_eq!(code, 404, "the label was reported deleted but is still there: {body}");
}

/// A webhook's `config` is a nested object passed as one JSON flag, which is the part most
/// likely to arrive as a string containing JSON rather than as an object. Reading the hook back
/// and inspecting `config.url` is the only way to tell those two apart.
#[test]
fn an_org_webhook_survives_a_round_trip_through_every_hook_route() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgCreateHook", "orgListHooks", "orgGetHook", "orgEditHook", "orgDeleteHook"]);

    let org = TestOrg::create(inst, "orghook");
    let run = inst.fjo([
        "raw",
        "org",
        "create-hook",
        org.name(),
        "--type",
        "forgejo",
        "--config",
        r#"{"url":"http://hooks.example.invalid/one","content_type":"json"}"#,
        "--events",
        "push",
        "--active=true",
    ]);
    run.assert_ok("fjo raw org create-hook");
    let hook = run.json();
    assert_eq!(
        hook["config"]["url"], "http://hooks.example.invalid/one",
        "the nested config did not arrive as an object: {hook}"
    );
    assert_eq!(hook["active"], true, "{hook}");
    let id = hook["id"].as_i64().expect("a hook id").to_string();

    let run = inst.fjo(["raw", "org", "list-hooks", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-hooks");
    assert_eq!(
        run.json().as_array().map(Vec::len),
        Some(1),
        "a fresh organization should have exactly the one hook: {}",
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "get-hook", org.name(), id.as_str()]);
    run.assert_ok("fjo raw org get-hook");
    assert_eq!(run.json()["type"], "forgejo", "{}", run.stdout);

    inst.fjo(["raw", "org", "edit-hook", org.name(), id.as_str(), "--active=false"])
        .assert_ok("fjo raw org edit-hook");
    let (code, body) = org.api("GET", &format!("hooks/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let after: serde_json::Value = serde_json::from_str(&body).expect("a hook");
    assert_eq!(after["active"], false, "the hook was not deactivated: {after}");

    inst.fjo(["raw", "org", "delete-hook", org.name(), id.as_str()])
        .assert_ok("fjo raw org delete-hook");
    let (code, body) = org.api("GET", &format!("hooks/{id}"), None);
    assert_eq!(code, 404, "the hook was reported deleted but is still there: {body}");
}

// ------------------------------------------------------------- Actions secrets, variables

/// A secret's **value is never returned** by any route, so the only checkable facts are that
/// the name appears, that the listing does not leak the value, and that deleting it removes the
/// name. Asserting on the value would be asserting on something the API deliberately withholds.
#[test]
fn an_org_secret_is_listed_by_name_and_never_by_value() {
    let inst = instance_or_skip!();
    cover!(raw: ["updateOrgSecret", "orgListActionsSecrets", "deleteOrgSecret"]);

    let org = TestOrg::create(inst, "orgsecret");
    let value = "a-value-that-must-never-come-back";

    inst.fjo(["raw", "org", "update-org-secret", org.name(), "DEPLOY_KEY", "--data", value])
        .assert_ok("fjo raw org update-org-secret");

    let run = inst.fjo([
        "raw",
        "org",
        "list-actions-secrets",
        org.name(),
        "--paginate",
        "--limit",
        "100",
    ]);
    run.assert_ok("fjo raw org list-actions-secrets");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from(["DEPLOY_KEY".to_owned()]),
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains(value),
        "the secret listing echoed the secret back: {}",
        run.stdout
    );

    // The same route is create *and* update; a second write to an existing name must not be a
    // conflict.
    inst.fjo(["raw", "org", "update-org-secret", org.name(), "DEPLOY_KEY", "--data", "rotated"])
        .assert_ok("fjo raw org update-org-secret, rotating an existing secret");

    inst.fjo(["raw", "org", "delete-org-secret", org.name(), "DEPLOY_KEY"])
        .assert_ok("fjo raw org delete-org-secret");
    let (code, body) = org.api("GET", "actions/secrets", None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a secret list");
    assert_eq!(left.as_array().map(Vec::len), Some(0), "the secret was not deleted: {body}");
}

/// Unlike a secret, a variable's value *is* readable, so the whole round trip is checkable —
/// including the separate create (`POST`) and update (`PUT`) routes, which differ only in verb
/// and are therefore easy to wire to the wrong one.
#[test]
fn an_org_variable_round_trips_its_value_and_disappears_on_delete() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "createOrgVariable",
        "getOrgVariablesList",
        "getOrgVariable",
        "updateOrgVariable",
        "deleteOrgVariable",
    ]);

    let org = TestOrg::create(inst, "orgvar");

    inst.fjo(["raw", "org", "create-org-variable", org.name(), "REGION", "--value", "eu-west"])
        .assert_ok("fjo raw org create-org-variable");

    let run = inst.fjo(["raw", "org", "get-org-variable", org.name(), "REGION"]);
    run.assert_ok("fjo raw org get-org-variable");
    let var = run.json();
    assert_eq!(var["name"], "REGION", "{var}");
    assert_eq!(var["data"], "eu-west", "the value did not survive the round trip: {var}");

    inst.fjo([
        "raw",
        "org",
        "update-org-variable",
        org.name(),
        "REGION",
        "--name",
        "REGION",
        "--value",
        "us-east",
    ])
    .assert_ok("fjo raw org update-org-variable");

    let run = inst.fjo([
        "raw",
        "org",
        "get-org-variables-list",
        org.name(),
        "--paginate",
        "--limit",
        "100",
    ]);
    run.assert_ok("fjo raw org get-org-variables-list");
    let list = run.json();
    assert_eq!(field_set(&list, "name"), BTreeSet::from(["REGION".to_owned()]), "{}", run.stdout);
    assert_eq!(list[0]["data"], "us-east", "the update did not reach the server: {}", run.stdout);

    inst.fjo(["raw", "org", "delete-org-variable", org.name(), "REGION"])
        .assert_ok("fjo raw org delete-org-variable");
    let (code, body) = org.api("GET", "actions/variables/REGION", None);
    assert_eq!(code, 404, "the variable was reported deleted but is still there: {body}");
}

/// Registering a runner over the API is the one Actions operation that needs no runner binary,
/// so the whole id-addressed lifecycle is reachable. Bug this prevents: `get-org-runner` and
/// `delete-org-runner` rendering `{runner_id}` into the wrong slot, which against a real server
/// is a 404 and against a mock is whatever the fixture says.
///
/// `orgSearchRunJobs` is driven here too. With no runner attached to any workflow Forgejo
/// answers `200` with a bare `null` body, which is a decoding shape worth proving we survive —
/// a client that insists on an array would fail on it.
#[test]
fn a_registered_org_runner_is_listed_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "orgGetRunnerRegistrationToken",
        "registerOrgRunner",
        "getOrgRunners",
        "getOrgRunner",
        "deleteOrgRunner",
        "orgSearchRunJobs",
    ]);

    let org = TestOrg::create(inst, "orgrunner");

    let run = inst.fjo(["raw", "org", "get-runner-registration-token", org.name()]);
    run.assert_ok("fjo raw org get-runner-registration-token");
    assert!(
        run.json()["token"].as_str().is_some_and(|t| !t.is_empty()),
        "no registration token in {}",
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "register-org-runner", org.name(), "--name", "itest-runner"]);
    run.assert_ok("fjo raw org register-org-runner");
    let id = run.json()["id"].as_i64().expect("a runner id").to_string();

    let run =
        inst.fjo(["raw", "org", "get-org-runners", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org get-org-runners");
    assert_eq!(
        field_set(&run.json(), "name"),
        BTreeSet::from(["itest-runner".to_owned()]),
        "{}",
        run.stdout
    );

    let run = inst.fjo(["raw", "org", "get-org-runner", org.name(), id.as_str()]);
    run.assert_ok("fjo raw org get-org-runner");
    assert_eq!(run.json()["name"], "itest-runner", "{}", run.stdout);

    let run = inst.fjo(["raw", "org", "search-run-jobs", org.name()]);
    run.assert_ok("fjo raw org search-run-jobs");
    let jobs = run.json();
    assert!(
        jobs.is_null() || jobs.is_array(),
        "the job search answered with neither null nor an array: {}",
        run.stdout
    );

    inst.fjo(["raw", "org", "delete-org-runner", org.name(), id.as_str()])
        .assert_ok("fjo raw org delete-org-runner");
    let (code, body) = org.api("GET", &format!("actions/runners/{id}"), None);
    assert_eq!(code, 404, "the runner was reported deleted but is still there: {body}");
}

/// The avatar routes answer `204` with no body, so the only evidence either did anything is
/// that `avatar_url` changed — and changed *back*. Forgejo derives the default from a hash of
/// the account, so the restored URL is the original one, which makes this checkable rather than
/// merely "different again".
#[test]
fn uploading_an_org_avatar_changes_its_url_and_deleting_it_restores_the_default() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgUpdateAvatar", "orgDeleteAvatar", "orgGet"]);

    let org = TestOrg::create(inst, "orgavatar");
    let before = org.get()["avatar_url"].as_str().expect("a default avatar url").to_owned();

    inst.fjo(["raw", "org", "update-avatar", org.name(), "--image", TINY_PNG])
        .assert_ok("fjo raw org update-avatar");
    let uploaded = org.get()["avatar_url"].as_str().expect("an avatar url").to_owned();
    assert_ne!(uploaded, before, "the upload did not change the organization's avatar");

    inst.fjo(["raw", "org", "delete-avatar", org.name()]).assert_ok("fjo raw org delete-avatar");
    let restored = org.get()["avatar_url"].as_str().expect("an avatar url").to_owned();
    assert_eq!(restored, before, "deleting the avatar did not restore the default");
}

/// Blocking answers `204`, and the listing that proves it worked reports only a `block_id` —
/// the blocked account's user id, with no name anywhere in the reply. A test that looked for
/// the username would find nothing and could only conclude the block failed, so the id is
/// fetched out of band first.
#[test]
fn blocking_a_user_lists_them_until_they_are_unblocked() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgBlockUser", "orgListBlockedUsers", "orgUnblockUser"]);

    let org = TestOrg::create(inst, "orgblock");
    let blocked = inst.scoped_user("orgblk", &["read:user"]).expect("a second account");
    let (code, body) = inst.api("GET", &format!("users/{}", blocked.name), None);
    assert_eq!(code, 200, "{body}");
    let user: serde_json::Value = serde_json::from_str(&body).expect("a user");
    let user_id = user["id"].as_i64().expect("a user id");

    let run =
        inst.fjo(["raw", "org", "list-blocked-users", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-blocked-users, before blocking anybody");
    assert_eq!(
        run.json().as_array().map(Vec::len),
        Some(0),
        "a fresh organization has blocked nobody: {}",
        run.stdout
    );

    inst.fjo(["raw", "org", "block-user", org.name(), blocked.name.as_str()])
        .assert_ok("fjo raw org block-user");

    let run =
        inst.fjo(["raw", "org", "list-blocked-users", org.name(), "--paginate", "--limit", "100"]);
    run.assert_ok("fjo raw org list-blocked-users");
    let ids: BTreeSet<i64> = run
        .json()
        .as_array()
        .expect("a blocked list")
        .iter()
        .filter_map(|b| b["block_id"].as_i64())
        .collect();
    assert!(ids.contains(&user_id), "{} is not in the blocked list: {}", blocked.name, run.stdout);

    inst.fjo(["raw", "org", "unblock-user", org.name(), blocked.name.as_str()])
        .assert_ok("fjo raw org unblock-user");
    let (code, body) = org.api("GET", "list_blocked", None);
    assert_eq!(code, 200, "{body}");
    let left: serde_json::Value = serde_json::from_str(&body).expect("a blocked list");
    assert_eq!(left.as_array().map(Vec::len), Some(0), "the user is still blocked: {body}");
}

// ------------------------------------------------------------------------------------ quota

/// The `size:` subjects the quota tree is addressed by, one per branch.
///
/// Every one of them carries a `:`, which is the whole reason the list is not a single entry:
/// `:` is a *reserved* character, so whether `--subject size:assets:packages:all` arrives intact
/// depends on the query encoder, and `fjo raw org check-quota --dry-run` shows it going out as
/// `subject=size%3Aassets%3Apackages%3Aall`. Whether a server accepts that spelling is not
/// something a mock can be asked — it answers with whatever the fixture says regardless of what
/// was in the query string.
const QUOTA_SUBJECTS: &[&str] = &[
    "size:all",
    "size:repos:public",
    "size:repos:private",
    "size:git:lfs",
    "size:assets:artifacts",
    "size:assets:attachments:releases",
    "size:assets:packages:all",
];

/// Read one `i64` out of `GET /orgs/{org}/quota`'s nested `used.size` tree, failing with the
/// whole document rather than with `None`.
fn used_size(quota: &serde_json::Value, path: &[&str]) -> i64 {
    let mut at = &quota["used"]["size"];
    for key in path {
        at = &at[key];
    }
    at.as_i64().unwrap_or_else(|| panic!("used.size.{} is not a number in {quota}", path.join(".")))
}

/// Bug this prevents: the quota figures being structurally present but inert — every field
/// decoding to zero because it landed in the wrong slot, or because the nested `used.size` tree
/// was flattened on the way through the generated model. All-zero is also what a *correct*
/// reply looks like for a fresh organization, so reading zeros proves nothing on its own.
///
/// So this measures twice. An empty organization reports zero everywhere; then a private
/// repository with a commit in it is created, and `repos.private` must move while
/// `repos.public` stays put. A figure that moves in the right slot is the only evidence that
/// the tree is wired to real state rather than to a default.
///
/// Note `git.LFS`: the key is upper-case in the specification and the server agrees, which is
/// exactly the sort of detail a hand-written fixture smooths over into `lfs`.
#[test]
fn an_organizations_quota_reports_zero_until_something_uses_it() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgGetQuota"]);

    let org = TestOrg::create(inst, "orgquota");

    let run = inst.fjo(["raw", "org", "get-quota", org.name()]);
    run.assert_ok("fjo raw org get-quota, on a fresh organization");
    let before = run.json();
    for path in [
        ["repos", "public"].as_slice(),
        ["repos", "private"].as_slice(),
        ["git", "LFS"].as_slice(),
        ["assets", "artifacts"].as_slice(),
        ["assets", "attachments", "issues"].as_slice(),
        ["assets", "attachments", "releases"].as_slice(),
        ["assets", "packages", "all"].as_slice(),
    ] {
        assert_eq!(
            used_size(&before, path),
            0,
            "a fresh organization has used no {}: {before}",
            path.join(".")
        );
    }
    // `groups` is a list of the quota groups the organization is in. Empty here, but it must be
    // a list: a `null` would mean the generated model cannot tell "no groups" from "absent".
    assert!(
        before["groups"].is_array(),
        "the quota group list is not an array, so an ungrouped organization is indistinguishable \
         from a missing field: {before}"
    );

    let repo = inst.unique_repo_name("quotarepo");
    inst.fjo([
        "raw",
        "org",
        "create-org-repo",
        org.name(),
        "--name",
        repo.as_str(),
        "--private=true",
        "--auto-init=true",
        "--default-branch",
        "main",
    ])
    .assert_ok("fjo raw org create-org-repo, to put something in the quota");

    let run = inst.fjo(["raw", "org", "get-quota", org.name()]);
    run.assert_ok("fjo raw org get-quota, after creating a repository");
    let after = run.json();
    assert!(
        used_size(&after, &["repos", "private"]) > 0,
        "a private repository with a commit in it did not move repos.private: {after}"
    );
    assert_eq!(
        used_size(&after, &["repos", "public"]),
        0,
        "a *private* repository was counted against the public figure: {after}"
    );
}

/// The distinction this exists for: an empty collection must arrive as `[]`, not as `null`.
///
/// `live_issues.rs` found that Forgejo answers an empty *reaction* list with a bare `null`, so
/// the same question has to be asked of every collection rather than assumed from one answer.
/// These three do return `[]` — which is worth pinning, because it is the difference between a
/// generated model that can be a plain `Vec<T>` and one that needs `Option<Vec<T>>` or a
/// null-tolerant deserializer. `is_array()` is the assertion that tells them apart; a length
/// check alone would pass for neither and panic unhelpfully for `null`.
#[test]
fn an_unused_organizations_quota_listings_are_empty_arrays_rather_than_null() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgListQuotaArtifacts", "orgListQuotaAttachments", "orgListQuotaPackages"]);

    let org = TestOrg::create(inst, "orgquotalist");

    for command in ["list-quota-artifacts", "list-quota-attachments", "list-quota-packages"] {
        let run = inst.fjo(["raw", "org", command, org.name()]);
        run.assert_ok(&format!("fjo raw org {command}"));
        let listed = run.json();
        assert!(
            listed.is_array(),
            "fjo raw org {command} answered with {listed} rather than a JSON array — an empty \
             collection arriving as null is the shape that breaks a plain Vec<T> model"
        );
        assert_eq!(
            listed.as_array().map(Vec::len),
            Some(0),
            "a fresh organization has nothing counting against its quota: {}",
            run.stdout
        );
    }
}

/// `GET /orgs/{org}/quota/check` is the only operation in this group with a required *query*
/// parameter, and its reply is a bare JSON boolean rather than an object — two shapes that a
/// client can get wrong independently.
///
/// The negative case is the load-bearing half. A `--subject` that never reached the query string
/// would also be a 422, so "an unknown subject is rejected" proves nothing by itself: the server
/// answers a *missing* subject with `[subject: ]` and a bogus one by echoing it back. Asserting
/// the echo carries the value this test sent is what distinguishes a flag that travelled from a
/// flag that was dropped — and it asserts on a string this test chose, not on Forgejo's wording.
#[test]
fn checking_a_quota_subject_sends_the_subject_and_rejects_an_unknown_one() {
    let inst = instance_or_skip!();
    cover!(raw: ["orgCheckQuota"]);

    let org = TestOrg::create(inst, "orgquotacheck");

    for subject in QUOTA_SUBJECTS {
        let run = inst.fjo(["raw", "org", "check-quota", org.name(), "--subject", subject]);
        run.assert_ok(&format!("fjo raw org check-quota --subject {subject}"));
        assert!(
            run.json().is_boolean(),
            "check-quota for {subject} answered with {} rather than a bare JSON boolean",
            run.stdout.trim()
        );
    }

    // Deliberately shaped like a real subject — same `prefix:suffix` form — so the rejection is
    // about the value and not about the punctuation surviving the trip.
    let bogus = "size:no-such-subject";
    inst.fjo(["raw", "org", "check-quota", org.name(), "--subject", bogus])
        .assert_code(1, "check-quota with a subject the server does not know")
        .assert_says("422")
        .assert_says(bogus);
}

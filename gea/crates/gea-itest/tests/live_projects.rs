//! Layer 0 and `gea project`, against a real Gitea.
//!
//! # Why none of this can be covered by a `FakeTransport`
//!
//! Everything layer 0 rests on is a fact about Gitea that no specification states, because
//! the routes behind the board UI are not described anywhere (Projects had no REST API at all
//! before Gitea 28; the one 28 added is driven at the end of this file). A
//! mock answers with whatever the test author believed, so it agrees with itself by
//! construction — and the whole risk of this layer is that the author's belief is wrong. Four
//! beliefs in particular are load-bearing, and each was wrong at least once while this was being
//! written:
//!
//! * the session cookie is `session` (Gitea's `i_like_gitea`, which most documentation on the
//!   internet names, matches nothing on Gitea);
//! * the web forms take lowercase field names, not the capitalised Go struct fields;
//! * `POST` to a web route with neither `Origin` nor `Sec-Fetch-Site` passes Gitea's
//!   cross-origin protection, so there is no CSRF token to scrape; and
//! * a `303` to `/user/login` — never a `401` — is the only signal that a session has lapsed.
//!
//! These run against the pinned image, which is what turns a Gitea release that changes any of
//! them into a red test rather than a user's bug report. That pin is layer 0's entire safety
//! story: see `docs/layers.md`.
//!
//! # What these caught, and why the fixture alone was not enough
//!
//! These were written before they could run, and the first thing they found was a bug no unit
//! test could have: `gea` stored the **pre-authentication** session id.
//!
//! Gitea's sign-in calls `RegenerateSession`, a session-fixation defence that issues a fresh
//! session id *after* writing the user id, so the response sets the cookie twice — the first
//! anonymous, the second signed in. Reading the first yielded a session that was perfectly
//! valid and perfectly anonymous, so every public route worked and every private one answered
//! **404** rather than a 401 or a redirect. Nothing in the lapsed-session path fired, because
//! the server never said "signed out".
//!
//! That is the shape of bug this file exists for: it needs a real server, a real sign-in, and a
//! *private* repository, and it is invisible to a `FakeTransport` answering whatever the test
//! author believed. It also explains why the captured board fixture did not reveal it — that
//! page was fetched with a session obtained by `curl`, which keeps the last cookie as a browser
//! does.
//!
//! # The environment is built from scratch, deliberately
//!
//! Every command below runs with `GEA_TOKEN` and `GITEA_TOKEN` *removed*. An environment token
//! would satisfy the API half of these commands without the stored web session being consulted
//! at all, and the suite would pass with the feature entirely broken. The same reasoning is
//! written out in `live_oauth.rs`, which found it first.

use std::path::PathBuf;
use std::process::Command;

use gea_itest::{Instance, TestRepo, cover, gea_bin, instance_or_skip};

/// A configuration directory of its own, so a stored session cannot leak between tests.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("gea-itest-projects-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch configuration directory");
        Self { dir }
    }

    fn hosts_toml(&self) -> String {
        std::fs::read_to_string(self.dir.join("hosts.toml")).unwrap_or_default()
    }

    fn write_hosts_toml(&self, contents: &str) {
        std::fs::write(self.dir.join("hosts.toml"), contents).expect("writable scratch");
    }

    /// Break one field of the stored session document, whichever way TOML escaped it.
    ///
    /// Asserts that something actually changed. Without that, a quoting mismatch makes the
    /// corruption a no-op and the test then exercises a healthy credential while claiming to
    /// exercise a broken one -- which is exactly what happened the first time.
    fn corrupt(&self, field: &str) {
        let before = self.hosts_toml();
        let mut after = before.clone();
        for pattern in [format!(r#""{field}":""#), format!(r#""{field}":""#)] {
            let broken = format!("{pattern}dead-");
            after = after.replace(&pattern, &broken);
        }
        assert_ne!(
            before, after,
            "expected a {field} in the stored document to corrupt, found none in:\n{before}"
        );
        self.write_hosts_toml(&after);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Session {
    scratch: Scratch,
}

impl Session {
    /// Sign in with a password, the way a user would, and confirm the credential landed.
    fn login(inst: &Instance, tag: &str, pass: &str) -> Self {
        let scratch = Scratch::new(tag);
        let me = Self { scratch };
        // `--login` is how a non-interactive sign-in names the account. The suite runs with no
        // terminal, which is exactly the shape CI has.
        let (code, out, err) = me.run_stdin(
            &["auth", "login", "--host", &inst.base_url, "--login", &inst.user, "--with-password"],
            Some(pass),
        );
        assert_eq!(code, Some(0), "sign-in failed\nstdout: {out}\nstderr: {err}");
        assert!(
            me.scratch.hosts_toml().contains("web_session"),
            "the web credential should be in hosts.toml:\n{}",
            me.scratch.hosts_toml()
        );
        me
    }

    /// A second machine: its own configuration directory, nothing signed in.
    fn fresh(tag: &str) -> Self {
        Self { scratch: Scratch::new(tag) }
    }

    fn run(&self, args: &[&str]) -> (Option<i32>, String, String) {
        self.run_stdin(args, None)
    }

    fn run_stdin(&self, args: &[&str], stdin: Option<&str>) -> (Option<i32>, String, String) {
        use std::io::Write;
        use std::process::Stdio;

        let mut cmd = Command::new(gea_bin());
        cmd.args(args)
            .env("GEA_CONFIG_DIR", &self.scratch.dir)
            // The file store, so the test can inspect and corrupt the stored credential. A
            // keyring is not available in CI and would make this test skip silently.
            .env("GEA_CREDENTIAL_STORE", "file")
            .env("GEA_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            // See the module comment: an environment token would hide a broken session.
            .env_remove("GEA_TOKEN")
            .env_remove("GITEA_TOKEN")
            .env_remove("GEA_WEB_SESSION")
            .env_remove("GEA_HOST")
            .env_remove("GITEA_HOST")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("the gea binary should be executable");
        if let Some(text) = stdin {
            let pipe = child.stdin.as_mut().expect("a piped stdin");
            pipe.write_all(text.as_bytes()).expect("writable stdin");
        }
        let out = child.wait_with_output().expect("the command should finish");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// Enable the Projects unit on a repository. It is off by default on a fresh repo, and every
/// project route answers 404 until it is on — which looks exactly like a wrong URL.
fn enable_projects(repo: &TestRepo<'_>) {
    let (code, body) = repo.api("PATCH", "", Some(r#"{"has_projects":true}"#));
    assert!((200..300).contains(&code), "enabling the projects unit failed ({code}): {body}");
}

fn password_or_skip(inst: &Instance) -> Option<&'static str> {
    match inst.web_password() {
        Some(p) => Some(p),
        None => {
            eprintln!(
                "SKIPPED: attached to a pre-existing instance, so there is no web password to \
                 sign in with"
            );
            None
        }
    }
}

/// The layer's credential: a web session reaches the board page an API token cannot.
///
/// This used to assert the *negative* first — that `/api/v1` served no board — so a Gitea that
/// added the endpoint would fail here rather than leave layer 0 silently kept. Gitea 28 did add
/// it, and the premise now reads the other way round: the API serves projects, so `gea project`
/// is retirable onto layer 2 (`gea raw repo list-projects` and friends, driven in
/// `projects_are_reachable_through_the_rest_api_at_every_owner`). Until that re-targeting lands,
/// the session half below is still what `gea project` stands on.
#[test]
fn the_web_session_reaches_the_board_and_the_api_now_does_too() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "web-session");
    enable_projects(&repo);

    let (api_code, body) = repo.api("GET", "/projects", None);
    assert_eq!(api_code, 200, "Gitea 28 serves projects over the REST API: {body}");

    let s = Session::login(inst, "reaches", pass);
    let (code, out, err) = s.run(&["web", "GET", &format!("{}/projects", repo.slug()), "-i"]);
    assert_eq!(code, Some(0), "stdout: {out}\nstderr: {err}");
    assert!(
        out.contains("200") && !out.contains("/user/login"),
        "a signed-in request should render the board page, not bounce to the login form:\n{out}"
    );
}

/// A board, built and read back entirely through `gea project`.
///
/// This is the test that pins the route shapes and the form field names together: every one of
/// them was wrong in the first draft of the design, and each would have produced a plausible
/// looking 200 or a silent no-op rather than an error.
#[test]
fn a_board_round_trips_through_the_porcelain() {
    let inst = instance_or_skip!();
    cover!(porcelain: [
        "project create",
        "project list",
        "project view",
        "project column add",
        "project card add",
        "project card move",
        "project delete"
    ]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "board");
    enable_projects(&repo);
    let (code, body) = repo.api(
        "POST",
        "/issues",
        Some(r#"{"title":"Fix <script> & \"quotes\"","body":"an entity-bearing title"}"#),
    );
    assert!((200..300).contains(&code), "seeding an issue failed ({code}): {body}");

    let s = Session::login(inst, "board", pass);
    let r = repo.flag();
    let repo_flag: [&str; 2] = [r[0].as_str(), r[1].as_str()];

    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&repo_flag);
        let (code, out, err) = s.run(&full);
        assert_eq!(code, Some(0), "`gea {}` failed\nstdout: {out}\nstderr: {err}", args.join(" "));
        out
    };

    run(&["project", "create", "Roadmap", "--from-template", "basic-kanban"]);
    let listed = run(&["project", "list"]);
    assert!(listed.contains("Roadmap"), "the new board should be listed:\n{listed}");

    run(&["project", "column", "add", "Roadmap", "Review", "--hex", "#1f883d"]);
    run(&["project", "card", "add", "1", "--project", "Roadmap"]);
    run(&["project", "card", "move", "1", "--to", "Review"]);

    // Read the board back through the parser, and assert on the structure rather than on a
    // substring: a parser that found nothing would still "contain" very little and pass a
    // laxer check.
    let json = run(&["project", "view", "Roadmap", "--json"]);
    let board: serde_json::Value = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("view --json is not JSON: {e}\n{json}"));

    let columns = board["columns"].as_array().expect("a columns array");
    assert!(columns.len() >= 5, "basic-kanban plus one added column:\n{json}");

    let review = columns
        .iter()
        .find(|c| c["title"] == "Review")
        .unwrap_or_else(|| panic!("the added column should be present:\n{json}"));
    assert_eq!(review["color"], "#1f883d", "the custom colour should survive the round trip");

    let cards = review["cards"].as_array().expect("a cards array");
    assert_eq!(cards.len(), 1, "the card should have moved into Review:\n{json}");
    // The title is entity-escaped in the markup; a parser that skipped decoding would return
    // `Fix &lt;script&gt; &amp; &#34;quotes&#34;` and look almost right.
    assert_eq!(
        cards[0]["title"], "Fix <script> & \"quotes\"",
        "card titles must be entity-decoded"
    );

    run(&["project", "delete", "Roadmap", "--yes"]);
}

/// An issue may sit on several boards, and putting it on a second must not take it off the first.
///
/// Gitea's `POST /{owner}/{repo}/issues/projects` *replaces* an issue's set of boards: posting
/// board B for an issue on board A removes it from A, with a timeline comment saying so. A mock
/// would have answered `{"ok":true}` to either shape, so only a server shows that `card add`
/// sends the union — and only a second board, read back, shows the first one survived.
#[test]
fn adding_a_card_to_a_second_board_leaves_it_on_the_first() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["project create", "project card add", "project view"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "twoboards");
    enable_projects(&repo);
    let (code, body) = repo.api("POST", "/issues", Some(r#"{"title":"on two boards"}"#));
    assert!((200..300).contains(&code), "seeding an issue failed ({code}): {body}");

    let s = Session::login(inst, "twoboards", pass);
    let r = repo.flag();
    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&[r[0].as_str(), r[1].as_str()]);
        let (code, out, err) = s.run(&full);
        assert_eq!(code, Some(0), "`gea {}` failed\nstdout: {out}\nstderr: {err}", args.join(" "));
        out
    };

    for board in ["First", "Second"] {
        run(&["project", "create", board, "--from-template", "basic-kanban"]);
        run(&["project", "card", "add", "1", "--project", board]);
    }
    for board in ["First", "Second"] {
        let json = run(&["project", "view", board, "--json"]);
        let view: serde_json::Value = serde_json::from_str(&json).expect("view --json is JSON");
        let cards: usize = view["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .map(|c| c["cards"].as_array().map_or(0, Vec::len))
            .sum();
        assert_eq!(cards, 1, "#1 is missing from {board} after being added to both:\n{json}");
    }
}

/// The reliability property: a dead session is renewed from the remember token without the user
/// noticing, and without a password.
///
/// The session is corrupted in the stored credential rather than invalidated server-side. Both
/// produce the same thing — a cookie Gitea does not recognise, answered with `303` to
/// `/user/login` — and this way needs no surgery on the container's database.
#[test]
fn a_dead_session_is_renewed_silently_from_the_remember_token() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "remint");
    enable_projects(&repo);
    let s = Session::login(inst, "remint", pass);

    // Establish a working session first, so the failure below is the *renewal* path and not a
    // first-use path that happens to mint one.
    let (code, _, err) = s.run(&["web", "GET", &format!("{}/projects", repo.slug())]);
    assert_eq!(code, Some(0), "the first request should work: {err}");

    s.scratch.corrupt("session");

    // The command must simply work. No prompt, no error, no mention of signing in.
    let (code, out, err) = s.run(&["web", "GET", &format!("{}/projects", repo.slug())]);
    assert_eq!(code, Some(0), "a lapsed session should renew itself\nstdout: {out}\nstderr: {err}");
    assert!(
        !err.to_lowercase().contains("log in") && !err.to_lowercase().contains("password"),
        "renewal should be silent, but stderr said: {err}"
    );

    // Persisted, not merely used: a renewed session that is not written down makes the next
    // invocation mint another one, which is the failure `oauth_refresh`'s rule exists to prevent.
    let after = s.scratch.hosts_toml();
    assert!(
        !after.contains("dead-"),
        "the renewed session should have replaced the dead one on disk:\n{after}"
    );
}

/// A remember token that is itself dead cannot self-heal, and must say so in terms that name the
/// one command that fixes it.
#[test]
fn a_dead_remember_token_asks_for_a_password_rather_than_looping() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "expired");
    enable_projects(&repo);
    let s = Session::login(inst, "expired", pass);

    s.scratch.corrupt("session");
    s.scratch.corrupt("remember");

    let (code, out, err) = s.run(&["web", "GET", &format!("{}/projects", repo.slug())]);
    assert_ne!(code, Some(0), "a dead remember token must fail, not hang or succeed");
    let said = format!("{out}{err}");
    assert!(
        said.contains("--with-password"),
        "the error should name the command that fixes it:\n{said}"
    );
}

/// `gea web` as the escape hatch: the raw JSON move the board's own JavaScript sends.
///
/// Pins the thing most likely to rot quietly — that a `POST` carrying neither `Origin` nor
/// `Sec-Fetch-Site` is accepted. If Gitea ever reintroduces a CSRF token, this is the test
/// that says so.
#[test]
fn a_raw_json_move_is_accepted_without_any_csrf_token() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["project create", "project card add", "project view"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "rawmove");
    enable_projects(&repo);
    let (code, body) = repo.api("POST", "/issues", Some(r#"{"title":"raw move target"}"#));
    assert!((200..300).contains(&code), "seeding an issue failed ({code}): {body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("an issue");
    let issue_id = issue["id"].as_i64().expect("an internal issue id");

    let s = Session::login(inst, "rawmove", pass);
    let r = repo.flag();

    // Build a board through the porcelain, then drive the move through layer 0 directly.
    let (code, _, err) = s.run(&[
        "project",
        "create",
        "Raw",
        "--from-template",
        "basic-kanban",
        r[0].as_str(),
        r[1].as_str(),
    ]);
    assert_eq!(code, Some(0), "creating the board failed: {err}");

    // The issue has to be ON the board before it can be moved between columns: Gitea's
    // MoveIssues answers 500, not 4xx, when asked to move an issue the project does not hold.
    let (code, _, err) =
        s.run(&["project", "card", "add", "1", "--project", "Raw", r[0].as_str(), r[1].as_str()]);
    assert_eq!(code, Some(0), "putting the issue on the board failed: {err}");

    let (_, ids, _) = s.run(&["project", "view", "Raw", "--json", r[0].as_str(), r[1].as_str()]);
    let board: serde_json::Value = serde_json::from_str(&ids).unwrap_or_default();
    let project_id = board["id"].as_i64().unwrap_or(1);
    let column_id =
        board["columns"][0]["id"].as_i64().expect("basic-kanban creates at least one column");

    let payload = format!(r#"{{"issues":[{{"issueID":{issue_id},"sorting":0}}]}}"#);
    let path = format!("{}/projects/{project_id}/{column_id}/move", repo.slug());
    let (code, out, err) = s.run_stdin(&["web", "POST", &path, "--input", "-"], Some(&payload));
    assert_eq!(
        code,
        Some(0),
        "a JSON move with no CSRF token should be accepted\nstdout: {out}\nstderr: {err}"
    );
}

/// The board's lifecycle verbs, and every column verb, driven against a real server.
///
/// One test rather than six because each needs the same board built first, and a board is four
/// requests to stand up. The `coverage-check` ratchet counts leaves driven, not tests written.
#[test]
fn a_boards_state_and_its_columns_can_be_changed_and_removed() {
    let inst = instance_or_skip!();
    cover!(porcelain: [
        "project close",
        "project reopen",
        "project column edit",
        "project column move",
        "project column delete",
        "project list"
    ]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "lifecycle");
    let s = Session::login(inst, "lifecycle", pass);
    let r = repo.flag();
    let repo_flag: [&str; 2] = [r[0].as_str(), r[1].as_str()];

    let run = |args: &[&str]| {
        let mut full = args.to_vec();
        full.extend_from_slice(&repo_flag);
        let (code, out, err) = s.run(&full);
        assert_eq!(code, Some(0), "`gea {}` failed\nstdout: {out}\nstderr: {err}", args.join(" "));
        out
    };

    run(&["project", "create", "Ops", "--from-template", "basic-kanban"]);

    // Columns: rename, recolour, reorder, remove.
    run(&["project", "column", "edit", "Ops", "Backlog", "--title", "Icebox", "--hex", "#c320f6"]);
    let json = run(&["project", "view", "Ops", "--json"]);
    let board: serde_json::Value = serde_json::from_str(&json).expect("a board");
    let icebox = board["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .find(|c| c["title"] == "Icebox")
        .unwrap_or_else(|| panic!("the rename should have taken:\n{json}"));
    assert_eq!(icebox["color"], "#c320f6", "the colour should have taken too:\n{json}");

    run(&["project", "column", "move", "Ops", "Icebox", "--last"]);
    let json = run(&["project", "view", "Ops", "--json"]);
    let board: serde_json::Value = serde_json::from_str(&json).expect("a board");
    let titles: Vec<&str> = board["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .filter_map(|c| c["title"].as_str())
        .collect();
    assert_eq!(titles.last(), Some(&"Icebox"), "--last should put it at the end: {titles:?}");

    // Icebox is the renamed Backlog, which is the board's DEFAULT column. Gitea will not
    // delete that one, and answers a bare 500 rather than saying so, so gea refuses first.
    let (code, _, err) = s.run(&[
        "project",
        "column",
        "delete",
        "Ops",
        "Icebox",
        "--yes",
        repo_flag[0],
        repo_flag[1],
    ]);
    assert_ne!(code, Some(0), "deleting the default column must be refused");
    assert!(
        err.contains("default column"),
        "and the refusal should explain why rather than relay a bare 500: {err}"
    );

    // A non-default column deletes normally.
    run(&["project", "column", "delete", "Ops", "Done", "--yes"]);
    let json = run(&["project", "view", "Ops", "--json"]);
    assert!(!json.contains("\"Done\""), "the column should be gone:\n{json}");

    // Closing must not make a board unreachable -- the rule `gea milestone` set.
    run(&["project", "close", "Ops"]);
    let closed = run(&["project", "list", "-s", "closed"]);
    assert!(closed.contains("Ops"), "a closed board should still be listed:\n{closed}");
    let open = run(&["project", "list"]);
    assert!(!open.contains("Ops"), "and not among the open ones:\n{open}");

    run(&["project", "reopen", "Ops"]);
    let open = run(&["project", "list"]);
    assert!(open.contains("Ops"), "reopening should bring it back:\n{open}");
}

/// A session survives a round trip through `export` and `import`, which is the CI story.
///
/// The interesting assertion is the last one: the imported session must actually *work*, not
/// merely be stored. An export that produced something `import` accepts but Gitea does not is
/// the failure this is for.
#[test]
fn a_session_can_be_exported_and_imported_on_another_machine() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth export", "auth import", "auth status"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "transfer");
    let from = Session::login(inst, "transfer-from", pass);

    // stdout is piped here, so the terminal guard does not fire and --force is not needed.
    let (code, doc, err) = from.run(&["auth", "export", "--web"]);
    assert_eq!(code, Some(0), "export failed: {err}");
    assert!(doc.contains("web-session"), "export should emit a session document: {doc}");
    assert!(
        err.to_lowercase().contains("full-account"),
        "the warning belongs on stderr, where it cannot corrupt the pipe: {err}"
    );

    // A second machine: its own config directory, and no sign-in of its own.
    let to = Session::fresh("transfer-to");
    let (code, _, err) = to.run_stdin(
        &["auth", "login", "--host", &inst.base_url, "--login", &inst.user, "--with-token"],
        Some(&inst.token),
    );
    assert_eq!(code, Some(0), "seeding the host entry failed: {err}");

    let (code, out, err) = to.run_stdin(&["auth", "import", "--web"], Some(doc.trim()));
    assert_eq!(code, Some(0), "import failed\nstdout: {out}\nstderr: {err}");

    // It is reported...
    let (_, status, _) = to.run(&["auth", "status"]);
    assert!(status.contains("Web session:"), "auth status should report it:\n{status}");
    assert!(!status.contains("Web session: none"), "and not as absent:\n{status}");

    // ...and, the part that matters, it authenticates a private repo.
    let (code, out, err) = to.run(&["web", "GET", &format!("{}/projects", repo.slug()), "-i"]);
    assert_eq!(code, Some(0), "the imported session should work\nstdout: {out}\nstderr: {err}");
    assert!(out.contains("200"), "expected the board page:\n{out}");
}

// ------------------------------------------------------------------------- the REST projects API

/// Where a project lives, and how the 16 commands that manage it are addressed there.
///
/// Gitea 28 serves the same board API under three owners, and the generated commands differ
/// only in group, command prefix and leading path arguments: `gea raw repo create-project O R`,
/// `gea raw org create-project ORG`, `gea raw user current-create-project`. One driver therefore
/// covers all three, which is also the strongest check that they really are the same API.
struct Owner {
    group: &'static str,
    prefix: &'static str,
    /// The positional arguments that precede the project id.
    leading: Vec<String>,
    /// The projects collection, relative to `/api/v1`, for out-of-band reads.
    base: String,
}

impl Owner {
    fn raw(&self, inst: &Instance, command: &str, rest: &[&str]) -> gea_itest::Run {
        let command = format!("{}{command}", self.prefix);
        let mut args: Vec<&str> = vec!["raw", self.group, &command];
        args.extend(self.leading.iter().map(String::as_str));
        args.extend_from_slice(rest);
        inst.gea(args)
    }

    fn get(&self, inst: &Instance, path: &str) -> (i32, serde_json::Value) {
        let (code, body) = inst.api("GET", &format!("{}/{path}", self.base), None);
        (code, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }
}

/// A project that deletes itself, so a failed assertion does not leave a board on the shared
/// admin account — the one owner here that outlives the test.
struct ProjectGuard<'a> {
    inst: &'a Instance,
    path: String,
}

impl Drop for ProjectGuard<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api("DELETE", &self.path, None);
    }
}

/// An organization with one repository, both deleted on drop, repository first because Gitea
/// refuses to delete an organization that still owns one.
struct ScratchOrg<'a> {
    inst: &'a Instance,
    name: String,
    repo: String,
}

impl<'a> ScratchOrg<'a> {
    fn create(inst: &'a Instance, prefix: &str) -> Self {
        let name = inst.unique_repo_name(prefix);
        let (code, body) = inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{name}"}}"#)));
        assert!((200..300).contains(&code), "creating the org {name} failed ({code}): {body}");
        let org = Self { inst, name, repo: "board".to_owned() };
        let (code, body) = inst.api(
            "POST",
            &format!("orgs/{}/repos", org.name),
            Some(r#"{"name":"board","auto_init":true}"#),
        );
        assert!((200..300).contains(&code), "creating the org repository failed ({code}): {body}");
        org
    }
}

impl Drop for ScratchOrg<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api("DELETE", &format!("repos/{}/{}", self.name, self.repo), None);
        let _ = self.inst.api("DELETE", &format!("orgs/{}", self.name), None);
    }
}

/// Open an issue out of band and return its **global** id.
///
/// The project routes take the database id, not the `#N` a user sees — the spec says so on
/// every `issue_id`, and passing the index would silently target whichever issue in the whole
/// instance happens to have that id.
fn open_issue(inst: &Instance, repo_slug: &str) -> i64 {
    let (code, body) =
        inst.api("POST", &format!("repos/{repo_slug}/issues"), Some(r#"{"title":"card"}"#));
    assert!((200..300).contains(&code), "opening an issue failed ({code}): {body}");
    let issue: serde_json::Value = serde_json::from_str(&body).expect("an issue");
    issue["id"].as_i64().unwrap_or_else(|| panic!("no issue id in {issue}"))
}

fn ids(v: &serde_json::Value) -> Vec<i64> {
    v.as_array()
        .unwrap_or_else(|| panic!("expected an array, got {v}"))
        .iter()
        .filter_map(|x| x["id"].as_i64())
        .collect()
}

/// Every one of the 16 project commands at one owner, each mutation read back out of band.
fn drive_project_lifecycle(inst: &Instance, owner: &Owner, issue: i64) {
    let issue = issue.to_string();

    let project = owner
        .raw(inst, "create-project", &["--title", "Roadmap", "--description", "the plan"])
        .assert_ok("create-project")
        .json();
    let pid = project["id"].as_i64().unwrap_or_else(|| panic!("no project id in {project}"));
    let _guard = ProjectGuard { inst, path: format!("{}/{pid}", owner.base) };
    let p = pid.to_string();

    let listed = owner.raw(inst, "list-projects", &[]).assert_ok("list-projects").json();
    assert!(ids(&listed).contains(&pid), "the new project is not listed: {listed}");

    let got = owner.raw(inst, "get-project", &[&p]).assert_ok("get-project").json();
    assert_eq!(got["title"], "Roadmap", "{got}");

    // Close and reopen: `state` is the one enum in the body, and a closed board refuses cards.
    owner
        .raw(inst, "edit-project", &[&p, "--title", "Roadmap 2", "--state", "closed"])
        .assert_ok("edit-project");
    let (_, back) = owner.get(inst, &p);
    assert_eq!(back["title"], "Roadmap 2", "the rename did not stick: {back}");
    assert_eq!(back["state"], "closed", "the close did not stick: {back}");
    owner.raw(inst, "edit-project", &[&p, "--state", "open"]).assert_ok("edit-project (reopen)");

    let todo = owner
        .raw(inst, "create-project-column", &[&p, "--title", "Todo", "--color", "#00ff00"])
        .assert_ok("create-project-column")
        .json();
    let done = owner
        .raw(inst, "create-project-column", &[&p, "--title", "Done"])
        .assert_ok("create-project-column")
        .json();
    let (a, b) = (todo["id"].as_i64().unwrap(), done["id"].as_i64().unwrap());
    let (sa, sb) = (a.to_string(), b.to_string());

    let columns =
        owner.raw(inst, "list-project-columns", &[&p]).assert_ok("list-project-columns").json();
    assert!(ids(&columns).contains(&a) && ids(&columns).contains(&b), "{columns}");

    let got =
        owner.raw(inst, "get-project-column", &[&p, &sa]).assert_ok("get-project-column").json();
    assert_eq!(got["title"], "Todo", "{got}");

    owner
        .raw(inst, "edit-project-column", &[&p, &sa, "--title", "Doing"])
        .assert_ok("edit-project-column");
    let (_, back) = owner.get(inst, &format!("{p}/columns/{sa}"));
    assert_eq!(back["title"], "Doing", "the column rename did not stick: {back}");

    // `column_ids` is a body `[]int64`: sent as strings, the server rejects the whole move.
    let (_, before) = owner.get(inst, &format!("{p}/columns"));
    let mut order = ids(&before);
    order.reverse();
    let mut args: Vec<String> = vec![p.clone()];
    for id in &order {
        args.extend(["--column-ids".to_owned(), id.to_string()]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    owner.raw(inst, "move-project-columns", &args).assert_ok("move-project-columns");
    let (_, after) = owner.get(inst, &format!("{p}/columns"));
    assert_eq!(ids(&after), order, "the columns were not reordered: {after}");

    owner
        .raw(inst, "set-default-project-column", &[&p, &sb])
        .assert_ok("set-default-project-column");
    let (_, back) = owner.get(inst, &format!("{p}/columns/{sb}"));
    assert_eq!(back["default"], true, "Done is not the default column: {back}");

    owner
        .raw(inst, "add-issue-to-project-column", &[&p, &sa, &issue])
        .assert_ok("add-issue-to-project-column");
    let cards = owner
        .raw(inst, "list-project-column-issues", &[&p, &sa])
        .assert_ok("list-project-column-issues")
        .json();
    let want: i64 = issue.parse().unwrap();
    assert_eq!(ids(&cards), vec![want], "the card is not in Doing: {cards}");

    owner
        .raw(inst, "move-project-issue", &[&p, &issue, "--column-id", &sb])
        .assert_ok("move-project-issue");
    let (_, in_a) = owner.get(inst, &format!("{p}/columns/{sa}/issues"));
    let (_, in_b) = owner.get(inst, &format!("{p}/columns/{sb}/issues"));
    assert!(ids(&in_a).is_empty(), "the card is still in Doing: {in_a}");
    assert_eq!(ids(&in_b), vec![want], "the card did not reach Done: {in_b}");

    owner
        .raw(inst, "remove-issue-from-project-column", &[&p, &sb, &issue])
        .assert_ok("remove-issue-from-project-column");
    let (_, in_b) = owner.get(inst, &format!("{p}/columns/{sb}/issues"));
    assert!(ids(&in_b).is_empty(), "the card survived its removal: {in_b}");

    owner.raw(inst, "delete-project-column", &[&p, &sa]).assert_ok("delete-project-column");
    let (code, _) = owner.get(inst, &format!("{p}/columns/{sa}"));
    assert_eq!(code, 404, "the column survived its deletion");

    owner.raw(inst, "delete-project", &[&p]).assert_ok("delete-project");
    let (code, _) = owner.get(inst, &p);
    assert_eq!(code, 404, "the project survived its deletion");
}

/// Gitea 28's projects API, driven through layer 2 at every owner it is served under.
#[test]
fn projects_are_reachable_through_the_rest_api_at_every_owner() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoListProjects", "repoCreateProject", "repoGetProject", "repoEditProject",
        "repoDeleteProject", "repoListProjectColumns", "repoCreateProjectColumn",
        "repoGetProjectColumn", "repoEditProjectColumn", "repoDeleteProjectColumn",
        "repoMoveProjectColumns", "repoSetDefaultProjectColumn", "repoListProjectColumnIssues",
        "repoAddIssueToProjectColumn", "repoRemoveIssueFromProjectColumn", "repoMoveProjectIssue",
    ]);
    cover!(raw: [
        "orgListProjects", "orgCreateProject", "orgGetProject", "orgEditProject",
        "orgDeleteProject", "orgListProjectColumns", "orgCreateProjectColumn",
        "orgGetProjectColumn", "orgEditProjectColumn", "orgDeleteProjectColumn",
        "orgMoveProjectColumns", "orgSetDefaultProjectColumn", "orgListProjectColumnIssues",
        "orgAddIssueToProjectColumn", "orgRemoveIssueFromProjectColumn", "orgMoveProjectIssue",
    ]);
    cover!(raw: [
        "userCurrentListProjects", "userCurrentCreateProject", "userCurrentGetProject",
        "userCurrentEditProject", "userCurrentDeleteProject", "userCurrentListProjectColumns",
        "userCurrentCreateProjectColumn", "userCurrentGetProjectColumn",
        "userCurrentEditProjectColumn", "userCurrentDeleteProjectColumn",
        "userCurrentMoveProjectColumns", "userCurrentSetDefaultProjectColumn",
        "userCurrentListProjectColumnIssues", "userCurrentAddIssueToProjectColumn",
        "userCurrentRemoveIssueFromProjectColumn", "userCurrentMoveProjectIssue",
        "userListProjects",
    ]);

    let repo = TestRepo::create_initialized(inst, "rest-board");
    enable_projects(&repo);
    let in_repo = Owner {
        group: "repo",
        prefix: "",
        leading: vec![repo.owner.clone(), repo.name.clone()],
        base: format!("repos/{}/projects", repo.slug()),
    };
    drive_project_lifecycle(inst, &in_repo, open_issue(inst, &repo.slug()));

    let org = ScratchOrg::create(inst, "rest-board-org");
    let in_org = Owner {
        group: "org",
        prefix: "",
        leading: vec![org.name.clone()],
        base: format!("orgs/{}/projects", org.name),
    };
    drive_project_lifecycle(inst, &in_org, open_issue(inst, &format!("{}/board", org.name)));

    let mine = Owner {
        group: "user",
        prefix: "current-",
        leading: vec![],
        base: "user/projects".to_owned(),
    };
    drive_project_lifecycle(inst, &mine, open_issue(inst, &repo.slug()));

    // The public listing of somebody's boards, the one read with no `current-` twin.
    let created = mine
        .raw(inst, "create-project", &["--title", "Public board"])
        .assert_ok("create-project")
        .json();
    let pid = created["id"].as_i64().unwrap();
    let _guard = ProjectGuard { inst, path: format!("user/projects/{pid}") };
    let listed = inst
        .gea(["raw", "user", "list-projects", inst.user.as_str()])
        .assert_ok("gea raw user list-projects")
        .json();
    assert!(ids(&listed).contains(&pid), "{} does not list its own board: {listed}", inst.user);
}

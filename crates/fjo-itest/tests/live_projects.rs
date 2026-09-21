//! Layer 0 and `fjo project`, against a real Forgejo.
//!
//! # Why none of this can be covered by a `FakeTransport`
//!
//! Everything layer 0 rests on is a fact about Forgejo that no specification states, because
//! Projects has no REST API and the routes behind the board UI are not described anywhere. A
//! mock answers with whatever the test author believed, so it agrees with itself by
//! construction — and the whole risk of this layer is that the author's belief is wrong. Four
//! beliefs in particular are load-bearing, and each was wrong at least once while this was being
//! written:
//!
//! * the session cookie is `session` (Gitea's `i_like_gitea`, which most documentation on the
//!   internet names, matches nothing on Forgejo);
//! * the web forms take lowercase field names, not the capitalised Go struct fields;
//! * `POST` to a web route with neither `Origin` nor `Sec-Fetch-Site` passes Forgejo's
//!   cross-origin protection, so there is no CSRF token to scrape; and
//! * a `303` to `/user/login` — never a `401` — is the only signal that a session has lapsed.
//!
//! These run against the pinned image, which is what turns a Forgejo release that changes any of
//! them into a red test rather than a user's bug report. That pin is layer 0's entire safety
//! story: see `docs/layers.md`.
//!
//! # What these caught, and why the fixture alone was not enough
//!
//! These were written before they could run, and the first thing they found was a bug no unit
//! test could have: `fjo` stored the **pre-authentication** session id.
//!
//! Forgejo's sign-in calls `RegenerateSession`, a session-fixation defence that issues a fresh
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
//! Every command below runs with `FJO_TOKEN` and `FORGEJO_TOKEN` *removed*. An environment token
//! would satisfy the API half of these commands without the stored web session being consulted
//! at all, and the suite would pass with the feature entirely broken. The same reasoning is
//! written out in `live_oauth.rs`, which found it first.

use std::path::PathBuf;
use std::process::Command;

use fjo_itest::{Instance, TestRepo, cover, fjo_bin, instance_or_skip};

/// A configuration directory of its own, so a stored session cannot leak between tests.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("fjo-itest-projects-{}-{tag}", std::process::id()));
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

    fn run(&self, args: &[&str]) -> (Option<i32>, String, String) {
        self.run_stdin(args, None)
    }

    fn run_stdin(&self, args: &[&str], stdin: Option<&str>) -> (Option<i32>, String, String) {
        use std::io::Write;
        use std::process::Stdio;

        let mut cmd = Command::new(fjo_bin());
        cmd.args(args)
            .env("FJO_CONFIG_DIR", &self.scratch.dir)
            // The file store, so the test can inspect and corrupt the stored credential. A
            // keyring is not available in CI and would make this test skip silently.
            .env("FJO_CREDENTIAL_STORE", "file")
            .env("FJO_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            // See the module comment: an environment token would hide a broken session.
            .env_remove("FJO_TOKEN")
            .env_remove("FORGEJO_TOKEN")
            .env_remove("FJO_WEB_SESSION")
            .env_remove("FJO_HOST")
            .env_remove("FORGEJO_HOST")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("the fjo binary should be executable");
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

/// The whole point of the layer: a credential that reaches a route the API does not have.
///
/// Deliberately asserts the *negative* first. If `/api/v1` could serve a board, none of this
/// code would need to exist, and a future Forgejo that adds the endpoint should make this test
/// fail loudly so the layer can be retired rather than silently kept.
#[test]
fn the_api_has_no_projects_endpoint_but_the_web_session_reaches_one() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "web"]);
    let Some(pass) = password_or_skip(inst) else { return };

    let repo = TestRepo::create_initialized(inst, "web-session");
    enable_projects(&repo);

    // The premise. If this starts returning 200, upstream shipped the API and layer 0's
    // project half is retirable -- see the retirement note in docs/layers.md.
    let (api_code, _) = repo.api("GET", "/projects", None);
    assert!(
        !(200..300).contains(&api_code),
        "the REST API unexpectedly served a projects endpoint ({api_code}); if upstream has \
         added it, layer 0's project half is retirable -- see docs/layers.md"
    );

    let s = Session::login(inst, "reaches", pass);
    let (code, out, err) = s.run(&["web", "GET", &format!("{}/projects", repo.slug()), "-i"]);
    assert_eq!(code, Some(0), "stdout: {out}\nstderr: {err}");
    assert!(
        out.contains("200") && !out.contains("/user/login"),
        "a signed-in request should render the board page, not bounce to the login form:\n{out}"
    );
}

/// A board, built and read back entirely through `fjo project`.
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
        assert_eq!(code, Some(0), "`fjo {}` failed\nstdout: {out}\nstderr: {err}", args.join(" "));
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

/// The reliability property: a dead session is renewed from the remember token without the user
/// noticing, and without a password.
///
/// The session is corrupted in the stored credential rather than invalidated server-side. Both
/// produce the same thing — a cookie Forgejo does not recognise, answered with `303` to
/// `/user/login` — and this way needs no surgery on the container's database.
#[test]
fn a_dead_session_is_renewed_silently_from_the_remember_token() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "web"]);
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
    cover!(porcelain: ["auth login", "web"]);
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

/// `fjo web` as the escape hatch: the raw JSON move the board's own JavaScript sends.
///
/// Pins the thing most likely to rot quietly — that a `POST` carrying neither `Origin` nor
/// `Sec-Fetch-Site` is accepted. If Forgejo ever reintroduces a CSRF token, this is the test
/// that says so.
#[test]
fn a_raw_json_move_is_accepted_without_any_csrf_token() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["web"]);
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

    // The issue has to be ON the board before it can be moved between columns: Forgejo's
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

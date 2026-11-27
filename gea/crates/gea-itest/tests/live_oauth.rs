//! Gitea's OAuth2 provider, measured rather than assumed.
//!
//! Everything `gea auth login --web` does rests on a handful of facts about Gitea that are not
//! in the Swagger document, because the OAuth2 endpoints live under the web root rather than
//! `/api/v1`. A `FakeTransport` cannot check any of them: it answers with whatever the test
//! author believed, so it agrees with itself by construction. These run against a real instance.
//!
//! # The consent click is covered, and how
//!
//! `/login/oauth/authorize` renders a page that needs a signed-in session and a click, so the
//! obvious conclusion is that it cannot be driven from a test. It can. Two things make it easy,
//! and both were established by trying it rather than by reading templates:
//!
//! * Forgejo 16.0.4 (measured for fjo, which gea was ported from)'s sign-in form carries **no CSRF token**, so a session is one form POST.
//! * The grant form carries no CSRF token either, and every field it does carry — `client_id`,
//!   `state`, `scope`, `nonce`, `redirect_uri` — is a value that was in the authorize URL. So the
//!   consent can be posted without parsing a single byte of HTML.
//!
//! That removes the reason to avoid this: there is no markup dependency to rot. What remains is
//! the field names, which are OAuth's own plus `granted`, against a pinned image.
//!
//! So [`a_browser_login_stores_a_session_that_authenticates_the_api`] drives **gea's real code
//! path** — its PKCE, its loopback listener, its state check, its token exchange, its storage —
//! by pointing `$BROWSER` at a script that completes the consent and then fetches the redirect,
//! which is exactly what a browser would do.

use std::path::PathBuf;
use std::process::Command;

use gea_itest::{Instance, cover, gea_bin, instance_or_skip};

/// The two endpoint paths, which are the ones `Endpoints::fixed` falls back to.
///
/// Note the token endpoint is `/login/oauth/access_token`, not the conventional `/token`. A
/// reasonable guess gets a 404 that says nothing about why, which is exactly the kind of thing
/// worth pinning against a real server so a Gitea release cannot move it quietly.
#[test]
fn the_instance_publishes_an_openid_configuration_naming_the_gitea_paths() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let out = Command::new("curl")
        .args(["-sS", "--max-time", "30"])
        .arg(format!("{}/.well-known/openid-configuration", inst.base_url))
        .output()
        .expect("curl runs");
    let doc = String::from_utf8_lossy(&out.stdout);

    assert!(
        doc.contains("/login/oauth/authorize"),
        "the discovery document should name the authorize endpoint: {doc}"
    );
    assert!(
        doc.contains("/login/oauth/access_token"),
        "the token endpoint is access_token, not /token: {doc}"
    );
    // No device_authorization_endpoint: Gitea has no device grant, which is why --no-browser
    // pastes a URL back instead of showing a device code.
    assert!(
        !doc.contains("device_authorization_endpoint"),
        "Gitea grew a device grant; --no-browser could now be a real device flow: {doc}"
    );
}

/// A scratch configuration directory, so a login here cannot touch the developer's own.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("gea-itest-oauth-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch configuration directory");
        Self { dir }
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write the stand-in browser and return its path.
///
/// `gea auth login --web` hands a URL to whatever `$BROWSER` names and then waits on its loopback
/// socket. This does what a browser and a user would: sign in, open the consent page, click
/// Authorize, and follow the redirect back to 127.0.0.1. It parses no HTML — see the module
/// comment for why it does not have to.
fn write_fake_browser(scratch: &Scratch, base: &str, user: &str, pass: &str) -> PathBuf {
    let path = scratch.dir.join("fake-browser.sh");
    let script = format!(
        r#"#!/bin/sh
set -e
URL="$1"
JAR=$(mktemp)
trap 'rm -f "$JAR"' EXIT

curl -sS -c "$JAR" -b "$JAR" -o /dev/null -X POST   -d "user_name={user}&password={pass}" "{base}/user/login"

# Visiting the consent page is what makes Gitea validate redirect_uri and the PKCE challenge.
curl -sS -c "$JAR" -b "$JAR" -o /dev/null "$URL"

# Every field the grant form carries was in the URL we were handed, so no HTML is parsed. The
# values stay percent-encoded: curl sends them literally and the server decodes form values once.
qs=${{URL#*\?}}
field() {{ printf '%s' "$qs" | tr '&' '
' | sed -n "s/^$1=//p"; }}

LOC=$(curl -sS -c "$JAR" -b "$JAR" -o /dev/null -D - -X POST   -d "client_id=$(field client_id)&state=$(field state)&scope=&nonce=&redirect_uri=$(field redirect_uri)&granted=true"   "{base}/login/oauth/grant" | sed -n 's/^[Ll]ocation: *//p' | tr -d '\r')

# Hand the redirect to gea's listener, which is the whole point.
curl -sS -o /dev/null "$LOC" || true
"#
    );
    std::fs::write(&path, script).expect("write the stand-in browser");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("the script exists").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("make the script executable");
    }
    path
}

/// Register a public OAuth2 application and return its client id.
fn register_public_app(inst: &Instance, name: &str) -> String {
    let (code, body) = inst.api(
        "POST",
        "user/applications/oauth2",
        Some(&format!(
            r#"{{"name":"{name}","redirect_uris":["http://127.0.0.1"],"confidential_client":false}}"#
        )),
    );
    assert_eq!(code, 201, "registering a public OAuth application: {body}");
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["client_id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("the created application carries a client_id: {body}"))
}

/// The whole feature, end to end, through gea's own code.
///
/// This is the only test that proves `--web` works at all. Everything it exercises is gea's:
/// the PKCE verifier and challenge, the authorize URL, the loopback listener and its request
/// parser, the `state` check, the token exchange, and writing the session to the credential
/// store. The stand-in browser only does what a browser does.
///
/// It then spends the stored session on a real API call, which is the part a mock can never
/// check: that Gitea accepts gea's OAuth access token as `Authorization: Bearer` on `/api/v1`.
#[test]
fn a_browser_login_stores_a_session_that_authenticates_the_api() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "auth status", "auth token"], hits: ["userGetCurrent"]);

    let Some(pass) = inst.web_password() else {
        eprintln!("SKIPPED: attached to a pre-existing instance, so there is no web password");
        return;
    };

    let scratch = Scratch::new("login");
    let browser = write_fake_browser(&scratch, &inst.base_url, &inst.user, pass);
    let client_id = register_public_app(inst, &inst.unique_repo_name("gea-web-login"));

    let run = |args: &[&str]| -> (Option<i32>, String, String) {
        let out = Command::new(gea_bin())
            .args(args)
            // The environment is built from scratch rather than through `Instance::gea_env`,
            // which supplies GEA_TOKEN. An environment token would satisfy every command below
            // without the stored session being consulted at all, so the test would pass with the
            // feature broken.
            .env("GEA_CONFIG_DIR", &scratch.dir)
            .env("GEA_CREDENTIAL_STORE", "file")
            .env("GEA_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            // gea only opens a browser for a terminal; piped, it prints the URL and waits.
            .env("GEA_FORCE_TTY", "80")
            .env("BROWSER", &browser)
            .env_remove("GEA_TOKEN")
            .env_remove("GITEA_TOKEN")
            .env_remove("GEA_HOST")
            .env_remove("GITEA_HOST")
            .output()
            .expect("the gea binary should be executable");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (code, stdout, stderr) = run(&[
        "auth",
        "login",
        "--host",
        &inst.base_url,
        "--web",
        "--client-id",
        &client_id,
        // The happy path finishes in about two seconds. A generous multiple of that still
        // fails fast when the redirect never arrives, which is how this test reports a broken
        // redirect URI: not as an assertion, but as a wait that runs out.
        "--timeout",
        "20",
    ]);
    assert_eq!(code, Some(0), "auth login --web failed\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains(&format!("as {}", inst.user)), "{stdout}");
    assert!(stdout.contains("OAuth session"), "{stdout}");

    // Filed as an OAuth session, and the document is in the store rather than anywhere else.
    let hosts = scratch.read("hosts.toml");
    assert!(hosts.contains("kind = \"oauth2\""), "{hosts}");
    assert!(hosts.contains("\"v\":1"), "the stored document should be the credential: {hosts}");

    // The claim a mock cannot check: Gitea accepts this token as Bearer on /api/v1.
    let (code, stdout, stderr) = run(&["auth", "status"]);
    assert_eq!(code, Some(0), "auth status\nstdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Logged in to"), "{stdout}");
    assert!(stdout.contains("OAuth session"), "{stdout}");

    // And a real API command, which goes through Runtime rather than the auth group.
    let (code, stdout, stderr) = run(&["repo", "list", "--limit", "1"]);
    assert_eq!(code, Some(0), "repo list on an OAuth session\nstdout: {stdout}\nstderr: {stderr}");

    // The refresh token is the session; it must never reach a stream.
    let refresh = serde_json::from_str::<serde_json::Value>(
        hosts
            .lines()
            .find_map(|l| l.trim().strip_prefix("token = '")?.strip_suffix('\''))
            .expect("the credential document is on one line in hosts.toml"),
    )
    .expect("the credential document is JSON");
    let refresh = refresh["refresh_token"].as_str().expect("a refresh token was stored");
    assert!(!refresh.is_empty());

    let (code, stdout, stderr) = run(&["auth", "token"]);
    assert_eq!(code, Some(0), "auth token\nstderr: {stderr}");
    assert!(!stdout.contains(refresh), "auth token printed the refresh token");
    assert!(!stderr.contains(refresh), "the refresh token reached stderr");
    assert!(stdout.trim().starts_with("ey"), "an access token is a JWT: {stdout}");
}

/// **The single most fragile fact in the whole design**, pinned against a real server.
///
/// Gitea compares redirect URIs by exact string after uppercasing and trimming one trailing
/// slash. For a public client on `http` at a loopback IP it first strips the *port* and compares
/// again — but not the path. The built-in applications register `http://127.0.0.1`, so an
/// ephemeral port is forgiven and a path is not.
///
/// This matters because appending `/callback` is what every OAuth guide does, it looks tidier,
/// and the resulting failure is a `redirect_uri_mismatch` that says nothing about paths.
///
/// Checking it needs a signed-in session: `reqSignIn` runs before the handler, so an
/// unauthenticated request is answered `303` to `/user/login` whatever it asks for, and a bad
/// redirect URI is indistinguishable from a good one. With a session, the difference is a `200`
/// consent page against a `400`.
#[test]
fn a_loopback_redirect_uri_is_accepted_with_a_port_and_refused_with_a_path() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let Some(pass) = inst.web_password() else {
        eprintln!("SKIPPED: attached to a pre-existing instance, so there is no web password");
        return;
    };

    let scratch = Scratch::new("redirect");
    let jar = scratch.dir.join("cookies.txt");
    let client_id = register_public_app(inst, &inst.unique_repo_name("gea-redirect"));

    let ok = Command::new("curl")
        .args(["-sS", "-c"])
        .arg(&jar)
        .args(["-o", "/dev/null", "-X", "POST", "-d"])
        .arg(format!("user_name={}&password={pass}", inst.user))
        .arg(format!("{}/user/login", inst.base_url))
        .status()
        .expect("curl runs");
    assert!(ok.success(), "signing in to the web UI");

    let authorize = |redirect: &str| -> i32 {
        let url = format!(
            "{}/login/oauth/authorize?client_id={client_id}&response_type=code\
             &code_challenge_method=S256\
             &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &state=itest&redirect_uri={}",
            inst.base_url,
            redirect.replace(':', "%3A").replace('/', "%2F")
        );
        let out = Command::new("curl")
            .args(["-sS", "-b"])
            .arg(&jar)
            .args(["-o", "/dev/null", "-w", "%{http_code}"])
            .arg(url)
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    };

    assert_eq!(
        authorize("http://127.0.0.1:45231"),
        200,
        "a loopback redirect URI with a port must be accepted"
    );
    assert_eq!(
        authorize("http://127.0.0.1:45231/callback"),
        400,
        "a PATH on a loopback redirect URI must be refused. If this is ever 200, Gitea has \
         changed ContainsRedirectURI and cmd/auth/callback.rs can stop being careful about it."
    );
}

/// A refused token request must carry the server's own words, because those are what tell a user
/// which half is wrong. `invalid_request` alone does not.
#[test]
fn the_token_endpoint_refuses_a_bogus_code_with_a_described_error() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login"], hits: ["userGetCurrent"]);

    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/x-www-form-urlencoded",
            "-d",
            "grant_type=authorization_code&client_id=00000000-0000-0000-0000-000000000000\
             &code=not-a-real-code&redirect_uri=http://127.0.0.1:45231&code_verifier=x",
        ])
        .arg(format!("{}/login/oauth/access_token", inst.base_url))
        .output()
        .expect("curl runs");
    let body = String::from_utf8_lossy(&out.stdout);

    assert!(body.contains("\"error\""), "an OAuth failure is RFC 6749 shaped: {body}");
    assert!(
        body.contains("error_description"),
        "the description is the half written for a human, and gea quotes it: {body}"
    );
}

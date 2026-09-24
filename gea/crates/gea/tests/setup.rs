//! The setup surface, driven as a process: `auth`, `config`, `alias`, `completion`, `status`.
//!
//! Everything here runs the real executable, because the properties under test are properties of a
//! *process*: the mode of a file it wrote, which stream a secret did or did not reach, the exit
//! code, and — for the alias loop — that it terminates at all. None of those is observable from a
//! unit test.
//!
//! Nothing here touches the network. Where a host is configured it is deliberately one nothing
//! answers on, or one with no credential at all, so the interesting assertion is reached before a
//! socket would be.

use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

/// A token that is easy to grep for and obviously not real.
const PLANTED_TOKEN: &str = "f1cc0nly-planted-token-9d2b7a4e";

/// Every environment variable that could otherwise leak the developer's own setup — or their real
/// keyring — into a test.
///
/// `GEA_CREDENTIAL_STORE=file` is not laziness: without it these tests would probe the developer's
/// login keyring, which is both slow and rude.
fn cmd(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("gea").expect("the gea binary is built for its own tests");
    c.env("GEA_CONFIG_DIR", dir)
        .env("GEA_CREDENTIAL_STORE", "file")
        .env("GEA_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env_remove("GEA_HOST")
        .env_remove("GITEA_HOST")
        .env_remove("GEA_TOKEN")
        .env_remove("GITEA_TOKEN")
        .env_remove("GEA_USER")
        .env_remove("GITEA_USER")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("GEA_FORCE_TTY")
        .env_remove("CLICOLOR_FORCE");
    c.timeout(Duration::from_secs(30));
    c
}

/// The same, but pretending stdout is an 80-column terminal, so the human renderers run.
fn tty(dir: &Path) -> Command {
    let mut c = cmd(dir);
    c.env("GEA_FORCE_TTY", "80");
    c
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// A `hosts.toml` naming one host and one login, with the token in the file store.
///
/// Port 1 is used deliberately: nothing listens there, so any test that got as far as a request
/// would fail loudly rather than silently talking to something.
fn with_file_token(dir: &Path) {
    std::fs::write(
        dir.join("hosts.toml"),
        format!(
            "active = \"localhost:1\"\n\
             \n\
             [[hosts]]\n\
             name = \"localhost:1\"\n\
             url = \"http://localhost:1\"\n\
             active_login = \"perf3ct\"\n\
             credential_store = \"file\"\n\
             \n\
             [[hosts.logins]]\n\
             user = \"perf3ct\"\n\
             token = \"{PLANTED_TOKEN}\"\n\
             scopes = [\"read:repository\", \"write:issue\"]\n"
        ),
    )
    .expect("write hosts.toml");
}

/// A refresh token that is easy to grep for, and must never appear on any stream.
const PLANTED_REFRESH: &str = "f1cc0nly-planted-refresh-4b8e2c7d";

/// A `hosts.toml` holding an OAuth session in the file store.
///
/// The expiry is far in the future on purpose: a session about to lapse would be refreshed, and
/// these tests must not reach the network. Port 1 keeps that honest — nothing listens there, so
/// a test that did try would fail loudly.
fn with_oauth_session(dir: &Path) {
    // A TOML literal string, so the JSON's own quotes need no escaping.
    let blob = format!(
        concat!(
            r#"{{"v":1,"kind":"oauth2","access_token":"{}","refresh_token":"{}","#,
            r#""expires_at":"2099-01-01T00:00:00Z","#,
            r#""client_id":"a4792ccc-144e-407e-86c9-5e7d8d9c3269","#,
            r#""token_endpoint":"http://localhost:1/login/oauth/access_token"}}"#,
        ),
        PLANTED_TOKEN, PLANTED_REFRESH,
    );
    std::fs::write(
        dir.join("hosts.toml"),
        format!(
            "active = \"localhost:1\"\n\
             \n\
             [[hosts]]\n\
             name = \"localhost:1\"\n\
             url = \"http://localhost:1\"\n\
             active_login = \"perf3ct\"\n\
             credential_store = \"file\"\n\
             \n\
             [[hosts.logins]]\n\
             user = \"perf3ct\"\n\
             kind = \"oauth2\"\n\
             token = '{blob}'\n"
        ),
    )
    .expect("write hosts.toml");
}

/// A host with a recorded login but no credential anywhere.
fn without_any_token(dir: &Path) {
    std::fs::write(
        dir.join("hosts.toml"),
        "active = \"git.example.org\"\n\
         \n\
         [[hosts]]\n\
         name = \"git.example.org\"\n\
         url = \"https://git.example.org\"\n\
         active_login = \"perf3ct\"\n\
         credential_store = \"env\"\n\
         \n\
         [[hosts.logins]]\n\
         user = \"perf3ct\"\n\
         scopes = [\"read:repository\", \"write:issue\"]\n",
    )
    .expect("write hosts.toml");
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("hosts.toml exists").permissions().mode() & 0o777
}

// ------------------------------------------------------------------------------------- auth

/// The single most important test in this file, and the reason `--show-token` does not exist.
///
/// Bug it prevents: a token reaching `auth status` output. That output is what people paste into
/// bug reports, screenshots and chat, so one convenience field there turns every such paste into a
/// leaked credential. Both streams are checked, because a "helpful" diagnostic on stderr leaks just
/// as thoroughly as a column on stdout.
#[test]
fn auth_status_never_prints_the_token_on_either_stream() {
    let dir = tmp();
    with_file_token(dir.path());
    let out = tty(dir.path()).args(["auth", "status"]).output().expect("gea runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains(PLANTED_TOKEN), "token on stdout:\n{stdout}");
    assert!(!stderr.contains(PLANTED_TOKEN), "token on stderr:\n{stderr}");
    // ...while still being a useful report: it found the login, said where the token lives, and
    // pointed at the one command that will print it.
    assert!(stdout.contains("perf3ct"), "{stdout}");
    assert!(stdout.contains("hosts.toml"), "{stdout}");
    assert!(stdout.contains("Token: hidden"), "{stdout}");
}

/// `--json` must not become a back door around the previous test.
#[test]
fn auth_status_json_has_no_token_field_at_all() {
    let dir = tmp();
    with_file_token(dir.path());
    // Bare `--json` lists what can be selected. If a token were selectable it would be here.
    let out = cmd(dir.path()).args(["auth", "status", "--json"]).output().expect("gea runs");
    let listed = String::from_utf8_lossy(&out.stdout);
    for forbidden in ["token\n", "password", "secret"] {
        assert!(!listed.contains(forbidden), "{forbidden:?} is selectable:\n{listed}");
    }
    assert!(listed.contains("token_source"), "where it lives is still selectable:\n{listed}");

    let out = cmd(dir.path())
        .args(["auth", "status", "--json", "host,login,token_source,authenticated"])
        .output()
        .expect("gea runs");
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(!json.contains(PLANTED_TOKEN), "{json}");
}

/// Bug this prevents: a token in a file every local account can read.
///
/// Also covers the repair path: `Hosts::save` writes through a fresh 0600 temporary file plus
/// `rename`, so any command that touches `hosts.toml` fixes a file that had been left loose — which
/// is what makes the permission warning actionable in one step.
#[test]
#[cfg(unix)]
fn hosts_toml_is_written_mode_0600_and_a_loose_file_is_repaired() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tmp();
    with_file_token(dir.path());
    let path = dir.path().join("hosts.toml");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let out = cmd(dir.path())
        .args(["auth", "switch", "--host", "localhost:1", "--login", "perf3ct"])
        .output()
        .expect("gea runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(mode_of(&path), 0o600, "hosts.toml must not be readable by other accounts");
    // The loose mode it started with is reported rather than silently fixed, because a token that
    // *was* world-readable may already have been read.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("can be read by other users"), "{stderr}");
}

/// Bug this prevents: `auth status --json` exiting non-zero when a host fails.
///
/// `gh` exits 0 there and scripts depend on it: the user asked for a machine-readable report and
/// received a correct one, so the *report* succeeded even though the host did not. Without `--json`
/// the failure is reported through the taxonomy, so a shell `if` still works.
#[test]
fn auth_status_reports_a_failure_but_exits_zero_under_json() {
    let dir = tmp();
    without_any_token(dir.path());

    let out = cmd(dir.path())
        .args(["auth", "status", "--json", "host,login,authenticated,error"])
        .output()
        .expect("gea runs");
    assert!(out.status.success(), "--json must exit 0; got {:?}", out.status.code());
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON even on failure");
    assert_eq!(json[0]["authenticated"], serde_json::json!(false), "{json}");
    assert!(json[0]["error"].is_string(), "the reason has to be in the payload: {json}");

    // The same state, without a machine flag, is a failure the shell can see.
    cmd(dir.path()).args(["auth", "status"]).assert().failure();
}

/// Two hosts that both fail, in a chosen order.
///
/// `broken_first` puts the unreachable host at the top of `hosts.toml`. The two failures are
/// deliberately of *different* kinds so the exit code names which one was reported: the host with
/// no credential anywhere fails with `NotAuthenticated` (exit 4) without touching the network,
/// while `localhost:1` — where nothing listens — fails to connect (exit 6).
///
/// The reachable-looking host also carries two logins, so the human report has a host whose
/// stanzas must stay adjacent.
fn two_failing_hosts(dir: &Path, broken_first: bool) {
    let no_token = "[[hosts]]\n\
         name = \"aaa.example.org\"\n\
         url = \"https://aaa.example.org\"\n\
         active_login = \"perf3ct\"\n\
         credential_store = \"file\"\n\
         \n\
         [[hosts.logins]]\n\
         user = \"perf3ct\"\n\
         scopes = [\"read:repository\"]\n\
         \n\
         [[hosts.logins]]\n\
         user = \"alice\"\n\
         scopes = [\"read:repository\"]\n";
    let unreachable = format!(
        "[[hosts]]\n\
         name = \"localhost:1\"\n\
         url = \"http://localhost:1\"\n\
         active_login = \"bob\"\n\
         credential_store = \"file\"\n\
         \n\
         [[hosts.logins]]\n\
         user = \"bob\"\n\
         token = \"{PLANTED_TOKEN}\"\n\
         scopes = [\"read:repository\"]\n"
    );
    let (first, second) = if broken_first {
        (unreachable.as_str(), no_token)
    } else {
        (no_token, unreachable.as_str())
    };
    std::fs::write(
        dir.join("hosts.toml"),
        format!("active = \"aaa.example.org\"\n\n{first}\n{second}"),
    )
    .expect("write hosts.toml");
}

/// Bug this prevents: the concurrent per-host checks deciding which failure the shell sees.
///
/// `auth status` checks every host at once now, because each iteration targets a *different*
/// server and one decommissioned entry used to burn its whole connect-timeout × retry budget
/// before the next host was tried. The order of the *results* is still `hosts.toml` order, and two
/// observable things depend on that: `write_human` opens a new stanza whenever the host changes,
/// so an interleaved result set prints a host header twice; and the process's error is the first
/// failing row's, so an interleaved result set hands a script whichever host happened to answer
/// first. That is precisely what `buffer_unordered` would do and `buffered` does not.
#[test]
fn auth_status_reports_the_first_host_in_the_file_when_several_fail() {
    for (broken_first, expected) in [(false, 4), (true, 6)] {
        let dir = tmp();
        two_failing_hosts(dir.path(), broken_first);
        let out = cmd(dir.path()).args(["auth", "status"]).output().expect("gea runs");
        let stdout = String::from_utf8_lossy(&out.stdout);

        // (a) Stanza order follows the file, and each host opens exactly one stanza.
        let a = stdout.find("aaa.example.org (https://aaa.example.org)").expect(&stdout);
        let b = stdout.find("localhost:1 (http://localhost:1)").expect(&stdout);
        assert_eq!(a < b, !broken_first, "stanzas must follow hosts.toml order:\n{stdout}");
        for header in ["aaa.example.org (https://", "localhost:1 (http://"] {
            assert_eq!(
                stdout.matches(header).count(),
                1,
                "a host's rows must stay together, or its stanza opens twice:\n{stdout}"
            );
        }
        // Both logins of the multi-login host are reported, in the order the file lists them.
        let perf3ct = stdout.find("account perf3ct").expect(&stdout);
        let alice = stdout.find("account alice").expect(&stdout);
        assert!(perf3ct < alice, "logins must follow the file too:\n{stdout}");

        // (b) The first failing row is the one the exit code comes from: 4 when the host with no
        // credential leads, 6 when the unreachable one does.
        assert_eq!(
            out.status.code(),
            Some(expected),
            "broken_first={broken_first}\n{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Bug this prevents: a non-interactive `auth login` hanging on a prompt nobody can answer, or
/// failing with a message that does not name the flag to use instead.
#[test]
fn a_login_with_no_token_and_no_terminal_names_with_token() {
    let dir = tmp();
    cmd(dir.path())
        .args(["auth", "login", "--host", "git.example.org"])
        .assert()
        .code(2)
        .stderr(contains("--with-token"));
}

/// The never-hang guarantee, and it is only observable as a process.
///
/// Bug this prevents: `--web` blocking forever on a loopback socket nothing will ever connect
/// to. On a build server, in a container, or over an SSH session with no browser, the redirect
/// cannot arrive, and a wait with no deadline is indistinguishable from a crash. The deadline is
/// what turns that into an error that names the way out.
///
/// The URL goes to stderr before any of that, because it is the one thing that unblocks a user
/// in every failure mode this command has.
#[test]
fn a_web_login_without_a_browser_prints_the_url_and_does_not_hang() {
    let dir = tmp();
    cmd(dir.path())
        .args(["auth", "login", "--host", "localhost:1", "--web", "--timeout", "1"])
        .assert()
        .code(4)
        .stderr(contains("/login/oauth/authorize").and(contains("code_challenge_method=S256")));
}

/// `--no-browser` hands the reply back by hand, which needs somewhere to type. Without a
/// terminal it must say so rather than wait for input that cannot come.
#[test]
fn a_web_login_with_no_browser_and_no_terminal_says_so() {
    let dir = tmp();
    cmd(dir.path())
        .args(["auth", "login", "--host", "localhost:1", "--web", "--no-browser"])
        .assert()
        .code(2)
        .stderr(contains("--no-browser"));
}

/// The two ways of logging in are alternatives, not a combination, and clap should say so
/// before anything touches the network.
#[test]
fn a_web_login_conflicts_with_a_token_login() {
    let dir = tmp();
    cmd(dir.path())
        .args(["auth", "login", "--host", "localhost:1", "--web", "--with-token"])
        .assert()
        .code(2)
        .stderr(contains("--web").and(contains("--with-token")));
}

/// `--host` is required when there is no terminal to ask on, and the message has to say so.
#[test]
fn a_login_with_no_host_and_no_terminal_names_host() {
    let dir = tmp();
    cmd(dir.path()).args(["auth", "login"]).assert().code(2).stderr(contains("--host"));
}

/// THE bug an OAuth session introduces to this command.
///
/// `auth token` used to print the stored value verbatim, which was right when every stored value
/// was a token. An OAuth session is stored as a document carrying *both* tokens, and the refresh
/// token is the session itself — an access token is one hour of it. Printing the document would
/// put the long-lived half on a terminal, in scrollback, and in whatever the user piped it into.
#[test]
fn auth_token_on_an_oauth_session_prints_the_access_token_and_not_the_refresh_token() {
    let dir = tmp();
    with_oauth_session(dir.path());
    let out = cmd(dir.path()).args(["auth", "token"]).assert().success().get_output().clone();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout.trim(), PLANTED_TOKEN, "the access token is what git and curl want");
    for stream in [&stdout, &stderr] {
        assert!(!stream.contains(PLANTED_REFRESH), "the refresh token escaped: {stream}");
        assert!(!stream.contains("\"v\":1"), "the stored document escaped: {stream}");
    }
}

/// `auth status` reads the credential store, not the advisory `kind` in hosts.toml, and must
/// report an OAuth session as one without letting any part of it reach the output.
#[test]
fn auth_status_reports_an_oauth_session_without_printing_it() {
    let dir = tmp();
    with_oauth_session(dir.path());
    let out = cmd(dir.path())
        .args(["auth", "status", "--json", "credential_kind,expires_at,login"])
        .output()
        .expect("gea runs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("oauth2"), "{stdout}");
    assert!(stdout.contains("2099-01-01"), "the expiry is useful and is not a secret: {stdout}");
    assert!(!stdout.contains(PLANTED_REFRESH), "{stdout}");
    assert!(!stdout.contains(PLANTED_TOKEN), "{stdout}");
}

/// Logging out of an OAuth session must remove the whole document. This is the payoff of storing
/// it as one value: there is no second key to forget and no way to leave a live refresh token
/// behind.
#[test]
fn auth_logout_removes_the_whole_oauth_session() {
    let dir = tmp();
    with_oauth_session(dir.path());
    cmd(dir.path()).args(["auth", "logout", "--yes"]).assert().success();

    let left = std::fs::read_to_string(dir.path().join("hosts.toml")).unwrap_or_default();
    assert!(!left.contains(PLANTED_REFRESH), "a live refresh token survived logout: {left}");
    assert!(!left.contains(PLANTED_TOKEN), "{left}");
}

/// Bug this prevents: `auth logout` proceeding without confirmation in a script.
#[test]
fn logout_demands_yes_when_there_is_no_terminal() {
    let dir = tmp();
    with_file_token(dir.path());
    cmd(dir.path())
        .args(["auth", "logout", "--host", "localhost:1"])
        .assert()
        .code(2)
        .stderr(contains("--yes"));
    // ...and the credential is still there afterwards.
    assert!(
        std::fs::read_to_string(dir.path().join("hosts.toml")).unwrap().contains(PLANTED_TOKEN)
    );
}

/// Logout removes the token *and* the login. Leaving either behind is a distinct bug: a token with
/// no login is unreachable dead weight, and a login with no token reports "you are not logged in"
/// on every later command without saying why.
#[test]
fn logout_removes_both_halves_of_the_credential() {
    let dir = tmp();
    with_file_token(dir.path());
    cmd(dir.path())
        .args(["auth", "logout", "--host", "localhost:1", "--yes"])
        .assert()
        .success()
        .stdout(contains("Logged perf3ct out"));

    let after = std::fs::read_to_string(dir.path().join("hosts.toml")).unwrap_or_default();
    assert!(!after.contains(PLANTED_TOKEN), "the token survived logout:\n{after}");
    assert!(!after.contains("perf3ct"), "the login survived logout:\n{after}");
}

/// `auth token` is the one deliberate exit for a secret, and it must be exactly the token when
/// piped — no trailing newline, no banner — or `$(gea auth token)` and `curl --config -` disagree.
#[test]
fn auth_token_prints_exactly_the_token_when_piped() {
    let dir = tmp();
    with_file_token(dir.path());
    let out = cmd(dir.path())
        .args(["auth", "token", "--host", "localhost:1"])
        .output()
        .expect("gea runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), PLANTED_TOKEN);
    // The scrollback warning is for terminals only; in a pipe it would corrupt nothing but it
    // would be noise in every script that uses this.
    assert!(!String::from_utf8_lossy(&out.stderr).contains("scrollback"));
}

/// `setup-git` must name *this* executable, not the bare word `gea`, and its `--dry-run` has to
/// print a line a reader can actually paste.
#[test]
fn setup_git_dry_run_shows_a_pasteable_git_config_command() {
    let dir = tmp();
    with_file_token(dir.path());
    let out = cmd(dir.path())
        .args(["auth", "setup-git", "--host", "localhost:1", "--dry-run"])
        .output()
        .expect("gea runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("credential.http://localhost:1.helper"), "{stdout}");
    assert!(stdout.contains("auth git-credential"), "{stdout}");
    // The empty argument of the `--replace-all` step is quoted, or the line looks truncated.
    assert!(stdout.contains("--replace-all credential.http://localhost:1.helper ''"), "{stdout}");
    // Nothing was written: `--dry-run` means dry.
    assert!(!stdout.contains("git will now authenticate"), "{stdout}");
}

// ----------------------------------------------------------------------------------- config

/// Bug this prevents: rejecting a mistyped key without saying what the real ones are.
#[test]
fn config_rejects_an_unknown_key_by_listing_the_valid_ones() {
    let dir = tmp();
    cmd(dir.path())
        .args(["config", "get", "credentials_store"])
        .assert()
        .code(2)
        .stderr(contains("credential_store"))
        .stderr(contains("editor"))
        .stderr(contains("prompt"));
}

/// A per-host override must win for that host and not leak to any other.
#[test]
fn config_set_get_honours_per_host_overrides() {
    let dir = tmp();
    cmd(dir.path()).args(["config", "set", "editor", "hx"]).assert().success();
    cmd(dir.path())
        .args(["config", "set", "pager", "cat", "--host", "git.example.org"])
        .assert()
        .success();

    let value = |args: &[&str]| -> String {
        let out = cmd(dir.path()).args(args).output().expect("gea runs");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    assert_eq!(value(&["config", "get", "editor"]), "hx");
    // `get` prints the stored value, so an unset key is empty rather than the default. That is what
    // lets a script tell "unset" from "explicitly set to the default".
    assert_eq!(value(&["config", "get", "pager"]), "");
    assert_eq!(value(&["config", "get", "pager", "--host", "git.example.org"]), "cat");
    assert_eq!(value(&["config", "get", "pager", "--host", "codeberg.org"]), "");

    // Tokens are never in config.toml, which is why it is safe to paste into a bug report.
    let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(!text.contains("token"), "{text}");
}

/// Bug this prevents: `config set prompt off` being accepted and silently ignored forever.
#[test]
fn config_set_rejects_a_value_outside_a_keys_vocabulary() {
    let dir = tmp();
    cmd(dir.path())
        .args(["config", "set", "prompt", "off"])
        .assert()
        .code(2)
        .stderr(contains("enabled"));
}

// ------------------------------------------------------------------------------------ alias

/// The end-to-end claim this whole feature makes: `gea alias set X '<command>'` then `gea X`.
///
/// `completion fish` is the expansion because it is the only layer-3 command that needs neither a
/// network nor a configured host, so the test proves expansion rather than anything about the
/// command it expanded to.
#[test]
fn an_alias_is_expanded_before_the_command_line_is_parsed() {
    let dir = tmp();
    cmd(dir.path()).args(["alias", "set", "shell", "completion fish"]).assert().success();
    cmd(dir.path()).arg("shell").assert().success().stdout(contains("complete -c gea"));
    // And a global flag before the alias must not stop it being found.
    cmd(dir.path()).args(["--color", "never", "shell"]).assert().success();
}

/// Bug this prevents: `gea alias set pr 'pr list'` succeeding, after which `gea pr create`
/// expands to `pr list create` and the only clue is a clap error about `create`.
#[test]
fn an_alias_that_shadows_a_real_command_is_refused_by_name() {
    let dir = tmp();
    cmd(dir.path())
        .args(["alias", "set", "pr", "pr list"])
        .assert()
        .code(2)
        .stderr(contains("already a real gea command"))
        .stderr(contains("gea pr --help"));
    cmd(dir.path()).args(["alias", "set", "raw", "api version"]).assert().code(2);
}

/// The worst failure this file guards against: an alias loop hanging the process before it prints
/// anything. Detected at `set` time, and again at expansion time because `config.toml` can be
/// hand-edited — which is exactly what this test does.
///
/// The 30-second `Command::timeout` is the assertion: without the cycle check this never returns.
#[test]
fn a_hand_edited_alias_loop_fails_instead_of_hanging() {
    let dir = tmp();
    std::fs::write(dir.path().join("config.toml"), "[aliases]\na = \"b --x\"\nb = \"a --y\"\n")
        .expect("write config.toml");
    cmd(dir.path())
        .arg("a")
        .assert()
        .code(2)
        .stderr(contains("loop"))
        .stderr(contains("a -> b -> a"));
    // Set-time refusal covers the ordinary case.
    cmd(dir.path()).args(["alias", "set", "c", "a"]).assert().code(2).stderr(contains("loop"));
}

/// `$1` is positional, and arguments no placeholder consumed are appended — otherwise
/// `gea co 42 --force` would silently drop `--force`.
#[test]
fn placeholders_consume_positionally_and_the_rest_is_appended() {
    let dir = tmp();
    cmd(dir.path()).args(["alias", "set", "shell", "completion $1"]).assert().success();
    cmd(dir.path()).args(["shell", "fish"]).assert().success().stdout(contains("complete -c gea"));
    // A missing placeholder names the alias and how many arguments it wants, rather than sending
    // a literal `$1` on to the command.
    cmd(dir.path()).arg("shell").assert().code(2).stderr(contains("$1"));
}

/// Import installs what it can and reports what it could not, rather than refusing the whole file
/// over one bad entry.
#[test]
fn alias_import_skips_what_it_cannot_use_and_keeps_the_rest() {
    let dir = tmp();
    let file = dir.path().join("aliases.toml");
    std::fs::write(
        &file,
        "[aliases]\n\
         mine = \"issue list --assignee @me\"\n\
         pr = \"pr list\"\n",
    )
    .expect("write");

    let out = cmd(dir.path()).args(["alias", "import"]).arg(&file).output().expect("gea runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("skipped pr"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Named fields, not bare `--json`: a bare one is field discovery and prints names, which is
    // gea's documented divergence from `gh` (docs/output.md, divergence 2).
    let listed = cmd(dir.path())
        .args(["alias", "list", "--json", "name,expansion"])
        .output()
        .expect("gea runs");
    let json: serde_json::Value = serde_json::from_slice(&listed.stdout).expect("valid JSON");
    assert_eq!(json.as_array().map(Vec::len), Some(1), "{json}");
    assert_eq!(json[0]["name"], serde_json::json!("mine"));
}

// ------------------------------------------------------------------------------- completion

/// Bug this prevents: silently omitting layer 2 from completions. It *is* omitted, deliberately,
/// and the help text has to say so — a completion that quietly covers half the tool teaches users
/// the other half does not exist.
#[test]
fn completion_covers_layers_one_and_three_and_admits_what_it_omits() {
    let dir = tmp();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let out = cmd(dir.path()).args(["completion", shell]).output().expect("gea runs");
        assert!(out.status.success(), "{shell}: {}", String::from_utf8_lossy(&out.stderr));
        let script = String::from_utf8_lossy(&out.stdout);
        for word in ["auth", "config", "alias", "status", "api", "raw"] {
            assert!(script.contains(word), "{shell} script omits {word}");
        }
    }
    cmd(dir.path())
        .args(["completion", "--help"])
        .assert()
        .success()
        .stdout(contains("Individual raw operations are not included"))
        .stdout(contains("gea raw search"));
}

/// Completions must be generatable with no configuration at all — it is the first thing a user
/// runs, often before `auth login`.
#[test]
fn completion_needs_no_host_no_token_and_no_network() {
    let dir = tmp();
    cmd(dir.path()).args(["completion", "fish"]).assert().success();
    assert!(!dir.path().join("hosts.toml").exists());
}

// ---------------------------------------------------------------------------------- goldens

/// Human output for the three commands that render without a network, as one snapshot.
///
/// A golden rather than assertions because layout is the thing being fixed: column alignment, the
/// order of the stanza's fields, and the fact that an unset key still gets a row.
#[test]
fn human_output_goldens() {
    let dir = tmp();
    without_any_token(dir.path());
    std::fs::write(
        dir.path().join("config.toml"),
        "editor = \"hx\"\n\n[aliases]\nprs = \"pr list --json number,title\"\nco = \"pr checkout $1\"\n\n[hosts.\"git.example.org\"]\npager = \"cat\"\n",
    )
    .expect("write config.toml");

    let mut report = String::new();
    for args in [
        &["auth", "status"][..],
        &["config", "list"][..],
        &["config", "list", "--host", "git.example.org"][..],
        &["alias", "list"][..],
    ] {
        let out = tty(dir.path()).args(args).output().expect("gea runs");
        report.push_str(&format!("== gea {}\n", args.join(" ")));
        report.push_str(&String::from_utf8_lossy(&out.stdout));
        report.push('\n');
    }
    insta::assert_snapshot!(report);
}

/// The same three commands piped, which is the stable machine-readable contract: TSV with no
/// header, no padding, and empty cells preserved.
#[test]
fn piped_output_goldens() {
    let dir = tmp();
    without_any_token(dir.path());
    std::fs::write(
        dir.path().join("config.toml"),
        "editor = \"hx\"\n\n[aliases]\nprs = \"pr list\"\n",
    )
    .expect("write config.toml");

    let mut report = String::new();
    for args in [
        &["config", "list"][..],
        &["alias", "list"][..],
        &["auth", "status", "--json", "host,login,authenticated,scopes"][..],
    ] {
        let out = cmd(dir.path()).args(args).output().expect("gea runs");
        report.push_str(&format!("== gea {}\n", args.join(" ")));
        report.push_str(&String::from_utf8_lossy(&out.stdout));
        report.push('\n');
    }
    insta::assert_snapshot!(report);
}

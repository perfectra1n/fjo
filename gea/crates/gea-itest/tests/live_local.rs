//! The setup surface, driven against a real server: `auth`, `config`, `alias`, `browse`,
//! `completion`, `status`, `search` and `run runners`.
//!
//! # Why these need a live instance at all
//!
//! Most of this group is the part of `gea` that touches the *local machine* — a `hosts.toml`, a
//! `config.toml`, a `~/.gitconfig` — and `crates/gea/tests/setup.rs` already drives all of it
//! hermetically against hosts nothing listens on. What that plane structurally cannot reach is
//! the moment the two halves meet:
//!
//! * `auth login` is the only command whose whole contract is *"ask the server who this token
//!   belongs to, and file it under that name"*. Against a fake there is nobody to ask, so the
//!   discovery is stubbed and the one bug it exists to prevent — a token recorded against the
//!   account the user typed — is unreachable.
//! * `auth setup-git` writes a credential helper, and the only proof it works is `git` itself
//!   cloning a private repository with no token in the URL. That is three processes (gea, git,
//!   Gitea) agreeing about one file.
//! * An alias is only interesting if invoking it runs a real command; `config`'s `browser` key is
//!   only interesting if a real command reads it.
//! * `status`, `search prs` and `run runners` are plain API commands with no hermetic coverage of
//!   the shapes the server actually returns.
//!
//! # Every test owns its configuration directory
//!
//! The harness points every child at one shared scratch `XDG_CONFIG_HOME`, which is right for
//! the rest of the suite — nothing there writes configuration. Everything here does, and these
//! tests run on parallel threads, so each one sets `GEA_CONFIG_DIR` to a directory of its own
//! (see [`Config`]). `GEA_CREDENTIAL_STORE=file` goes with it, for the reason
//! `crates/gea/tests/setup.rs` gives: without it a test would probe the developer's login
//! keyring, which is both slow and rude.
//!
//! # Tokens
//!
//! No assertion in this file may put a token in a failure message. `auth status` not printing
//! one is pinned hermetically; what is pinned here is that the *live* round trip does not leak
//! one either, and a test that dumps the token while proving the command did not would be
//! absurd. Hence [`no_token_anywhere`] and the length-only comparisons below.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use gea_itest::{Run, TestRepo, cover, instance_or_skip};

// ---------------------------------------------------------------------------------- fixtures

/// A configuration directory of this test's own, removed when the test ends.
///
/// `GEA_CONFIG_DIR` rather than `XDG_CONFIG_HOME`: it is first in
/// `gitea_core::config`'s resolution order, so it survives a test that also has to set
/// `XDG_CONFIG_HOME` for `git`'s benefit.
struct Config {
    dir: PathBuf,
    /// The same path as a `String`, because `gea_env` takes `&[(&str, &str)]` and a
    /// `to_string_lossy()` temporary would not outlive the call.
    path: String,
}

impl Config {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("gea-itest-local-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch configuration directory");
        let path = dir.to_string_lossy().into_owned();
        Self { dir, path }
    }

    /// The environment every `gea` invocation in this test gets, on top of the harness's.
    fn env(&self) -> Vec<(&str, &str)> {
        vec![
            ("GEA_CONFIG_DIR", &self.path),
            ("GEA_CREDENTIAL_STORE", "file"),
            ("GEA_PROMPT_DISABLED", "1"),
        ]
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn exists(&self, name: &str) -> bool {
        self.dir.join(name).exists()
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A scratch directory, for the tests that need a `HOME` or a working directory outside a git
/// repository.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("gea-itest-localdir-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a scratch directory");
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

/// Neither stream carried the secret — asserted without ever putting the secret in the message.
#[track_caller]
fn no_token_anywhere(run: &Run, token: &str, what: &str) {
    assert!(
        !run.stdout.contains(token),
        "{what} wrote the token to stdout ({} byte(s) of output)",
        run.stdout.len()
    );
    assert!(
        !run.stderr.contains(token),
        "{what} wrote the token to stderr ({} byte(s) of output)",
        run.stderr.len()
    );
}

/// `host:port`, which is how `gea` keys a host once the scheme has done its job.
fn authority(base_url: &str) -> &str {
    base_url.trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/')
}

/// Log in to this instance with the admin token, into `cfg`.
///
/// Every `auth` test needs a stored credential before it can test anything else, and doing it
/// through the real command rather than by writing `hosts.toml` is the point: the file's shape
/// is `gea`'s business, and a fixture that hand-rolled it would stop noticing when that changed.
fn login(inst: &gea_itest::Instance, cfg: &Config, token: &str) -> Run {
    let run = inst.gea_env(
        Path::new("."),
        &cfg.env(),
        ["auth", "login", "--host", &inst.base_url, "--token", token],
    );
    run.assert_ok("gea auth login");
    run
}

/// Run `git` with an environment this test chose, reporting both streams on failure.
///
/// The harness's own `gea_itest::git` pins `HOME` at the system temporary directory. That is
/// exactly the variable under test here — `auth setup-git` writes into `$HOME/.gitconfig` — so
/// these tests have to choose it themselves. `GIT_TERMINAL_PROMPT=0` is what turns a missing
/// credential into a prompt failure rather than a hang.
fn git_try(cwd: &Path, env: &[(&str, &str)], args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_CONFIG_NOSYSTEM", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("could not run git {args:?}: {e}"));
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// A branch with one file on it, made entirely over the API.
///
/// No git involved: these tests want a pull request as a *fixture*, and a clone plus a push is
/// three seconds and a temporary directory for something two API calls can do.
fn seed_branch(repo: &TestRepo<'_>, branch: &str, file: &str) {
    let (code, body) =
        repo.api("POST", "branches", Some(&format!(r#"{{"new_branch_name":"{branch}"}}"#)));
    assert!((200..300).contains(&code), "could not create {branch}: HTTP {code}: {body}");
    let content = gitea_core::http::base64::encode(format!("contents of {file}\n"));
    let (code, body) = repo.api(
        "POST",
        &format!("contents/{file}"),
        Some(&format!(r#"{{"content":"{content}","message":"add {file}","branch":"{branch}"}}"#)),
    );
    assert!((200..300).contains(&code), "could not add {file}: HTTP {code}: {body}");
}

/// Poll `check` until it holds, or give up after roughly thirty seconds.
///
/// Gitea writes issue index entries from a queue, so `/repos/issues/search` does not answer
/// with a brand-new issue the instant it is created. A fixed sleep here is either flaky or slow;
/// polling is neither, and it fails with the caller's own message rather than with a timeout.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..60 {
        if check() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    panic!("{what} never became true within 30s, so the assertions after it would prove nothing");
}

// -------------------------------------------------------------------------------------- auth

/// The property `auth login` exists for, and the one no mock can check: the token is filed under
/// the account the **server** names, not the one the user typed.
///
/// `--login not-the-token-owner` is deliberately wrong. Against a fake there is nobody to
/// contradict it, so a build that stored the requested name would pass every hermetic test and
/// then record a real token against an account that does not own it — after which every later
/// error message names the wrong identity and `auth token --login` hands out the wrong secret.
///
/// The round trip is the whole test: `login` writes a file, and `auth status` reads that file
/// back and proves the credential in it still authenticates against the same server.
#[test]
fn a_login_files_the_token_under_the_account_the_server_names() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "auth status"], hits: ["userGetCurrent"]);

    let cfg = Config::new("login");
    let run = inst.gea_env(
        Path::new("."),
        &cfg.env(),
        [
            "auth",
            "login",
            "--host",
            &inst.base_url,
            "--login",
            "not-the-token-owner",
            "--token",
            &inst.token,
            "--scopes",
            "read:user,write:repository",
        ],
    );
    run.assert_ok("gea auth login");
    no_token_anywhere(&run, &inst.token, "gea auth login");
    run.assert_says(&format!("as {}", inst.user));
    // Not fatal, but it must not be silent either: a user who asked for one account and got
    // another needs to be told which one they have.
    run.assert_says("was ignored");

    // The file, checked without quoting it — it holds the token.
    let hosts = cfg.read("hosts.toml");
    assert!(
        hosts.contains(&format!("user = \"{}\"", inst.user)),
        "hosts.toml does not record the login the server named ({} byte(s) written)",
        hosts.len()
    );
    assert!(
        !hosts.contains("not-the-token-owner"),
        "the name typed on the command line was filed instead of the one GET /user returned"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(cfg.dir.join("hosts.toml"))
            .expect("hosts.toml exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a file holding a token must not be readable by other accounts");
    }

    // The other half of the round trip: read that file back, and ask the server.
    let status = inst.gea_env(
        Path::new("."),
        &cfg.env(),
        ["auth", "status", "--json", "host,url,login,authenticated,is_admin,scopes"],
    );
    status.assert_ok("gea auth status after a login");
    no_token_anywhere(&status, &inst.token, "gea auth status");
    let rows = status.json();
    let rows = rows.as_array().expect("an array of hosts");
    assert_eq!(rows.len(), 1, "exactly the host just logged in to: {}", status.stdout);
    let row = &rows[0];
    assert_eq!(row["login"].as_str(), Some(inst.user.as_str()), "{row}");
    assert_eq!(
        row["authenticated"],
        serde_json::json!(true),
        "the stored credential did not authenticate against the server it was verified with: \
         {row}"
    );
    assert_eq!(
        row["is_admin"],
        serde_json::json!(true),
        "the bootstrap account is a site administrator, and status reports what GET /user says: \
         {row}"
    );
    assert_eq!(row["host"].as_str(), Some(authority(&inst.base_url)), "{row}");
    assert_eq!(
        row["scopes"],
        serde_json::json!(["read:user", "write:repository"]),
        "--scopes is recorded so a later 403 can say what the token has: {row}"
    );

    // The human rendering says where the token is, and never what it is.
    let human = inst.gea_env(Path::new("."), &cfg.env(), ["auth", "status"]);
    human.assert_ok("gea auth status (human)");
    no_token_anywhere(&human, &inst.token, "gea auth status (human)");
    human.assert_says("Token: hidden");
    human.assert_says("hosts.toml");
}

/// `auth token` is the single deliberate exit for a secret, so what it prints has to be usable as
/// one: exactly the bytes, no banner, no trailing newline — and it has to be a credential the
/// server still accepts.
///
/// The last part is what a hermetic test cannot do. `crates/gea/tests/setup.rs` proves the bytes
/// match a token it planted; only a live instance can prove the bytes that came back out of
/// `hosts.toml` authenticate as the account `auth status` claims they belong to.
#[test]
fn the_token_auth_token_prints_is_the_one_the_server_still_accepts() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "auth token"], hits: ["userGetCurrent"]);

    let cfg = Config::new("token");
    login(inst, &cfg, &inst.token);

    let run = inst.gea_env(Path::new("."), &cfg.env(), ["auth", "token"]);
    run.assert_ok("gea auth token");
    assert!(
        run.stdout == inst.token,
        "auth token printed {} byte(s); the token stored for this host is {} byte(s)",
        run.stdout.len(),
        inst.token.len()
    );
    // A stray newline in an `Authorization` header is a 401 nobody can explain, so the piped
    // form must be the token and nothing else.
    assert!(!run.stdout.ends_with('\n'), "piped output must not carry a trailing newline");
    assert!(
        !run.stderr.contains("scrollback"),
        "the scrollback warning is for terminals; in a pipe it is noise in every script"
    );

    // Out of band, through curl rather than through our own HTTP stack: the printed value is a
    // working credential for the account `auth status` files it under.
    let (code, body) = inst.api_as(run.stdout.trim(), "GET", "user", None);
    assert_eq!(code, 200, "what auth token printed was not accepted by the server: {body}");
    let me: serde_json::Value = serde_json::from_str(&body).expect("a user");
    assert_eq!(me["login"].as_str(), Some(inst.user.as_str()), "{body}");
}

/// `auth switch` has to change which credential every later command picks up, not merely which
/// name a report prints.
///
/// Two real accounts on one host is the only arrangement where that is observable, and minting
/// the second one needs a server. The proof is `auth token`: before the switch it hands back one
/// account's secret and after it the other's, which is the same lookup `gea pr list` would do.
#[test]
fn auth_switch_changes_which_credential_every_later_command_picks_up() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["auth login", "auth switch", "auth token", "auth status"],
        hits: ["userGetCurrent"],
    );

    let Ok(second) = inst.scoped_user("switcher", &["read:user"]) else {
        panic!("could not mint a second account to switch between");
    };
    let cfg = Config::new("switch");

    login(inst, &cfg, &inst.token);
    // The second login wins the active slot, because `auth login` sets it.
    let run = login(inst, &cfg, &second.token);
    run.assert_says(&format!("as {}", second.name));

    let token_now = |what: &str| -> String {
        let run = inst.gea_env(Path::new("."), &cfg.env(), ["auth", "token"]);
        run.assert_ok(what);
        run.stdout.trim().to_owned()
    };
    assert!(
        token_now("gea auth token before the switch") == second.token,
        "the most recent login should be the active one, but auth token handed back a \
         different {} byte(s)-long value",
        token_now("gea auth token before the switch").len()
    );

    let switch = inst.gea_env(
        Path::new("."),
        &cfg.env(),
        ["auth", "switch", "--host", authority(&inst.base_url), "--login", &inst.user],
    );
    switch.assert_ok("gea auth switch");
    no_token_anywhere(&switch, &inst.token, "gea auth switch");
    switch.assert_says(&format!("Active account is now {}", inst.user));

    assert!(
        token_now("gea auth token after the switch") == inst.token,
        "auth switch reported success but the credential every later command would use did not \
         change, so the switch was cosmetic"
    );

    // Both logins are still recorded, and exactly one of them is active.
    let status = inst.gea_env(
        Path::new("."),
        &cfg.env(),
        ["auth", "status", "--json", "login,active,active_host,authenticated"],
    );
    status.assert_ok("gea auth status with two logins");
    no_token_anywhere(&status, &inst.token, "gea auth status with two logins");
    let rows = status.json();
    let rows = rows.as_array().expect("an array of logins").clone();
    let names: BTreeSet<String> =
        rows.iter().filter_map(|r| r["login"].as_str().map(str::to_owned)).collect();
    assert!(names.contains(&inst.user), "{}", status.stdout);
    assert!(names.contains(&second.name), "{}", status.stdout);
    let active: Vec<&str> = rows
        .iter()
        .filter(|r| r["active"] == serde_json::json!(true))
        .filter_map(|r| r["login"].as_str())
        .collect();
    assert_eq!(active, [inst.user.as_str()], "exactly one login is active: {}", status.stdout);
    assert!(
        rows.iter().all(|r| r["authenticated"] == serde_json::json!(true)),
        "both tokens were verified at login and must still work: {}",
        status.stdout
    );
}

/// Logout has to remove the secret from disk, and it has to say when something else will keep
/// authenticating anyway.
///
/// The environment warning is the live half: the harness exports `GEA_TOKEN` into every child,
/// which is exactly the situation the message exists for — a user who "logged out" and then
/// finds every command still works. No process can unset a variable in its parent's shell, so
/// saying so is the only correct behaviour, and silence here is the bug.
#[test]
fn logout_removes_the_token_from_disk_and_says_the_environment_still_has_one() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["auth login", "auth logout", "auth status"], hits: ["userGetCurrent"]);

    let cfg = Config::new("logout");
    login(inst, &cfg, &inst.token);
    assert!(
        cfg.read("hosts.toml").contains(&inst.token),
        "the file store is supposed to hold the token, so this test would prove nothing"
    );

    let out = inst.gea_env(Path::new("."), &cfg.env(), ["auth", "logout", "--yes"]);
    out.assert_ok("gea auth logout --yes");
    no_token_anywhere(&out, &inst.token, "gea auth logout");
    out.assert_says(&format!("Logged {} out of", inst.user));
    out.assert_says("had no other logins and was removed");
    out.assert_says("still exported in your environment");

    let after = cfg.read("hosts.toml");
    assert!(
        !after.contains(&inst.token),
        "the token survived logout in a {} byte(s) file",
        after.len()
    );

    // And the host is gone, so nothing can still report an account there.
    let status = inst.gea_env(Path::new("."), &cfg.env(), ["auth", "status"]);
    assert!(
        !status.ok(),
        "auth status should not report a logged-in account after logout:\n{}",
        status.stdout
    );
    no_token_anywhere(&status, &inst.token, "gea auth status after logout");
}

/// The end-to-end claim `auth setup-git` makes: after it, `git` clones a **private** repository
/// over HTTP with no token anywhere in the URL.
///
/// Nothing hermetic can reach this. It is three processes agreeing about one file — `gea` writes
/// a credential helper into `$HOME/.gitconfig`, `git` runs `gea auth git-credential` when
/// Gitea answers 401, and the helper reads the token `auth login` verified. A unit test can
/// only check the string `gea` would write.
///
/// The failing clone before `setup-git` is the control. Without it a clone that succeeded for
/// any other reason — a cached credential, an unexpectedly public repository — would look like
/// proof, and `--dry-run` writing nothing is checked in the same breath.
#[test]
fn git_clones_a_private_repository_with_the_credential_helper_setup_git_wrote() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["auth login", "auth setup-git", "auth git-credential"],
        hits: ["userGetCurrent"],
    );

    let repo = TestRepo::create_initialized(inst, "setupgit");
    let cfg = Config::new("setupgit");
    let home = Scratch::new("setupgit-home");
    let home_path = home.path().to_string_lossy().into_owned();

    // `XDG_CONFIG_HOME` as well as `HOME`: git reads `$XDG_CONFIG_HOME/git/config` in preference
    // to `$HOME/.gitconfig`, and pointing both at an empty directory is what makes
    // `git config --global` land somewhere this test can see. gea is unaffected — `GEA_CONFIG_DIR`
    // is first in its own resolution order.
    let mut env = cfg.env();
    env.push(("HOME", &home_path));
    env.push(("XDG_CONFIG_HOME", &home_path));
    let git_env: Vec<(&str, &str)> = vec![
        ("HOME", &home_path),
        ("XDG_CONFIG_HOME", &home_path),
        ("GEA_CONFIG_DIR", &cfg.path),
        ("GEA_CREDENTIAL_STORE", "file"),
    ];

    let run = inst.gea_env(
        Path::new("."),
        &env,
        ["auth", "login", "--host", &inst.base_url, "--token", &inst.token],
    );
    run.assert_ok("gea auth login");

    let dry = inst.gea_env(Path::new("."), &env, ["auth", "setup-git", "--dry-run"]);
    dry.assert_ok("gea auth setup-git --dry-run");
    no_token_anywhere(&dry, &inst.token, "gea auth setup-git --dry-run");
    dry.assert_says(&format!("credential.{}.helper", inst.base_url.trim_end_matches('/')));
    dry.assert_says("auth git-credential");
    assert!(
        !home.path().join(".gitconfig").exists(),
        "--dry-run wrote a git configuration; dry means dry"
    );

    // The control: with no helper configured, a private repository is unreachable.
    let url = format!("{}/{}.git", inst.base_url, repo.slug());
    let before = home.path().join("before");
    let (ok, text) =
        git_try(home.path(), &git_env, &["clone", "--quiet", &url, &before.to_string_lossy()]);
    assert!(
        !ok,
        "cloning a private repository succeeded before setup-git ran, so the clone below would \
         prove nothing about the credential helper:\n{text}"
    );

    let real = inst.gea_env(Path::new("."), &env, ["auth", "setup-git"]);
    real.assert_ok("gea auth setup-git");
    no_token_anywhere(&real, &inst.token, "gea auth setup-git");
    real.assert_says("git will now authenticate");

    let gitconfig = std::fs::read_to_string(home.path().join(".gitconfig"))
        .expect("setup-git should have written a global git configuration");
    assert!(gitconfig.contains("auth git-credential"), "{gitconfig}");
    // The whole reason the helper exists rather than a token in a remote: git's configuration is
    // a file people paste and screenshot.
    assert!(
        !gitconfig.contains(&inst.token),
        "the token was written into git's configuration, which is what setup-git exists to avoid"
    );

    let dest = home.path().join("after");
    let (ok, text) =
        git_try(home.path(), &git_env, &["clone", "--quiet", &url, &dest.to_string_lossy()]);
    assert!(
        ok,
        "git could not clone {} with the helper setup-git configured. The helper is the only \
         credential in play — the URL carries none — so this is `gea auth git-credential` \
         failing to answer:\n{text}",
        repo.slug()
    );
    assert!(
        dest.join("README.md").exists(),
        "the clone reported success but the repository's content is missing"
    );
    // The remote it recorded is the one we handed it: no token was smuggled into .git/config.
    let (_, remote) = git_try(&dest, &git_env, &["remote", "get-url", "origin"]);
    assert!(!remote.contains(&inst.token), "the clone put the token into .git/config");
    assert!(remote.trim() == url, "the remote should be exactly what was cloned: {remote}");
}

// ------------------------------------------------------------------------------------ config

/// `config` is a file round trip, and its one observable consequence against a live instance is
/// that a stored value changes what a real command does.
///
/// `browser` is the key chosen for that, because it is the only one whose effect is reachable
/// without a terminal: `gea browse` without `-n` consults it, and pointing it at a program that
/// does not exist proves *both* that the value was read from `config.toml` and that no browser
/// was opened — while still printing the URL, which is what makes the failure recoverable.
#[test]
fn a_stored_configuration_value_survives_the_file_and_reaches_a_live_command() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["config set", "config get", "config list", "config unset", "browse"],
    );

    let cfg = Config::new("config");
    let env = cfg.env();
    let host = authority(&inst.base_url);
    let value = |args: &[&str]| -> String {
        let run = inst.gea_env(Path::new("."), &env, args);
        run.assert_ok(&format!("gea {}", args.join(" ")));
        run.stdout.trim().to_owned()
    };

    inst.gea_env(Path::new("."), &env, ["config", "set", "editor", "hx"])
        .assert_ok("gea config set editor");
    inst.gea_env(Path::new("."), &env, ["config", "set", "pager", "cat", "--host", host])
        .assert_ok("gea config set pager --host");

    assert_eq!(value(&["config", "get", "editor"]), "hx");
    // `get` prints the *stored* value, so an unset key is empty rather than its default — which
    // is what lets a script tell "unset" from "set to the default".
    assert_eq!(value(&["config", "get", "pager"]), "", "a per-host key must not leak to the top");
    assert_eq!(value(&["config", "get", "pager", "--host", host]), "cat");

    let listed = value(&["config", "list"]);
    assert!(
        listed.lines().any(|l| l.starts_with("editor\thx")),
        "config list must show the value it stored:\n{listed}"
    );
    let listed_host = value(&["config", "list", "--host", host]);
    assert!(
        listed_host.lines().any(|l| l.starts_with("pager\tcat")),
        "config list --host must show the override:\n{listed_host}"
    );

    // config.toml is safe to paste into a bug report; tokens live in hosts.toml.
    assert!(!cfg.read("config.toml").contains("token"), "config.toml must never hold a secret");

    inst.gea_env(Path::new("."), &env, ["config", "unset", "editor"])
        .assert_ok("gea config unset editor");
    assert_eq!(value(&["config", "get", "editor"]), "", "unset should remove the stored value");
    assert!(
        value(&["config", "list"]).lines().any(|l| l.starts_with("editor\t")),
        "an unset key still has a row in `config list`, carrying its default"
    );

    // A mistyped key must say what the real ones are rather than just refusing.
    let bad = inst.gea_env(Path::new("."), &env, ["config", "get", "credentials_store"]);
    bad.assert_code(2, "gea config get with an unknown key");
    bad.assert_says("credential_store");

    // The live consequence: `browse` reads `browser` out of the file just written.
    let repo = TestRepo::create(inst, "config-browser");
    let elsewhere = Scratch::new("config-browse");
    inst.gea_env(Path::new("."), &env, ["config", "set", "browser", "gea-itest-no-such-browser"])
        .assert_ok("gea config set browser");

    let opened = inst.gea_env(elsewhere.path(), &env, ["browse", "-R", &repo.slug()]);
    assert!(
        !opened.ok(),
        "the configured browser does not exist, so opening it must fail rather than silently \
         doing nothing:\n{}\n{}",
        opened.stdout,
        opened.stderr
    );
    opened.assert_says("could not open a browser");
    // The URL survives the failure, which is what makes `-n` a remedy rather than a riddle.
    opened.assert_says(&format!("{}/{}", inst.base_url, repo.slug()));
}

// ------------------------------------------------------------------------------------- alias

/// The claim the whole alias feature makes: `gea alias set X '<command>'` then `gea X` runs that
/// command — including its positional placeholders — against the server.
///
/// `crates/gea/tests/setup.rs` proves expansion happens, using `completion fish` because it is
/// the one command needing neither a host nor a network. That leaves the interesting half
/// untested: an alias whose expansion carries `-R`, `--json` and a `$1` has to survive
/// re-parsing as a real command line and produce the same answer the command would have.
#[test]
fn an_alias_expands_into_a_command_that_really_runs_against_the_instance() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["alias set", "alias list", "alias import", "alias delete", "issue list", "issue view"],
        hits: ["issueListIssues", "issueGetIssue"],
    );

    let repo = TestRepo::create(inst, "alias");
    for title in ["first issue", "second issue"] {
        let (code, body) = repo.api("POST", "issues", Some(&format!(r#"{{"title":"{title}"}}"#)));
        assert!((200..300).contains(&code), "seeding {title} failed: HTTP {code}: {body}");
    }

    let cfg = Config::new("alias");
    let env = cfg.env();
    let list_expansion = format!("issue list -R {} --json number,title", repo.slug());
    let view_expansion = format!("issue view $1 -R {} --json number,title", repo.slug());

    inst.gea_env(Path::new("."), &env, ["alias", "set", "mine", &list_expansion])
        .assert_ok("gea alias set mine");
    inst.gea_env(Path::new("."), &env, ["alias", "set", "one", &view_expansion])
        .assert_ok("gea alias set one");

    let listed = inst.gea_env(Path::new("."), &env, ["alias", "list", "--json", "name,expansion"]);
    listed.assert_ok("gea alias list");
    let names: BTreeSet<String> = listed
        .json()
        .as_array()
        .expect("an array of aliases")
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["mine".to_owned(), "one".to_owned()]),
        "alias list must show exactly the user's aliases (built-ins are hidden): {}",
        listed.stdout
    );

    // The point: the alias runs, and what comes back is the server's answer.
    let run = inst.gea_env(Path::new("."), &env, ["mine"]);
    run.assert_ok("gea mine (an alias for issue list)");
    let rows = run.json();
    let titles: BTreeSet<String> = rows
        .as_array()
        .expect("an array of issues")
        .iter()
        .filter_map(|i| i["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        titles,
        BTreeSet::from(["first issue".to_owned(), "second issue".to_owned()]),
        "the alias did not reach the repository its expansion names: {}",
        run.stdout
    );

    // `$1` is positional, and what it substitutes has to survive into the request.
    let run = inst.gea_env(Path::new("."), &env, ["one", "2"]);
    run.assert_ok("gea one 2 (an alias with a placeholder)");
    assert_eq!(run.json()["number"].as_u64(), Some(2), "{}", run.stdout);
    assert_eq!(run.json()["title"].as_str(), Some("second issue"), "{}", run.stdout);

    // Import installs what it can and reports what it cannot, rather than refusing the file.
    let file = cfg.dir.join("aliases.toml");
    std::fs::write(
        &file,
        format!(
            "[aliases]\nmine = \"issue list -R {} --json number\"\nextra = \"issue list -R {} --json number\"\npr = \"pr list\"\n",
            repo.slug(),
            repo.slug()
        ),
    )
    .expect("write the alias file");

    let imported = inst.gea_env(Path::new("."), &env, ["alias", "import", &file.to_string_lossy()]);
    imported.assert_ok("gea alias import");
    imported.assert_says("skipped pr");
    // Without --clobber an existing alias is left alone, which is the difference the flag buys.
    let expansion_of = |name: &str| -> String {
        let run = inst.gea_env(Path::new("."), &env, ["alias", "list", "--json", "name,expansion"]);
        run.assert_ok("gea alias list");
        run.json()
            .as_array()
            .expect("an array")
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["expansion"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    assert_eq!(expansion_of("mine"), list_expansion, "import must not clobber without --clobber");
    assert!(!expansion_of("extra").is_empty(), "the importable alias should have been installed");
    assert!(
        expansion_of("pr").is_empty(),
        "an alias shadowing a real command must never be installed"
    );

    let clobbered = inst.gea_env(
        Path::new("."),
        &env,
        ["alias", "import", &file.to_string_lossy(), "--clobber"],
    );
    clobbered.assert_ok("gea alias import --clobber");
    assert_ne!(expansion_of("mine"), list_expansion, "--clobber must replace what is there");

    inst.gea_env(Path::new("."), &env, ["alias", "delete", "one"]).assert_ok("gea alias delete");
    let gone = inst.gea_env(Path::new("."), &env, ["one", "2"]);
    assert!(!gone.ok(), "a deleted alias must not still expand:\n{}", gone.stdout);
}

// ------------------------------------------------------------------------------------ status

/// `gea status` answers "what is waiting for me, anywhere" out of four concurrent reads, and the
/// sections have to hold what the server actually assigned rather than whatever came back first.
///
/// Instance-wide by nature: it searches every repository the token can see, and other test
/// binaries share this Gitea. So nothing here counts rows — it looks for the two items this
/// test created, and then proves `--exclude` removes exactly them.
#[test]
fn status_reports_what_was_assigned_and_exclusion_removes_exactly_it() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["status"], hits: ["issueSearchIssues", "notifyGetList"]);

    let repo = TestRepo::create_initialized(inst, "status");
    let tag = format!("status-marker-{}", std::process::id());
    let issue_title = format!("{tag}-issue");
    let pr_title = format!("{tag}-pull");

    let (code, body) = repo.api(
        "POST",
        "issues",
        Some(&format!(r#"{{"title":"{issue_title}","assignees":["{}"]}}"#, inst.user)),
    );
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");

    seed_branch(&repo, "feature", "f.txt");
    let (code, body) = repo.api(
        "POST",
        "pulls",
        Some(&format!(
            r#"{{"title":"{pr_title}","head":"feature","base":"main","assignees":["{}"]}}"#,
            inst.user
        )),
    );
    assert!((200..300).contains(&code), "seeding the pull request failed: HTTP {code}: {body}");

    // The index is written from a queue, so the search does not see either item immediately.
    let titles = |args: &[&str]| -> BTreeSet<String> {
        let run = inst.gea(args);
        run.assert_ok(&format!("gea {}", args.join(" ")));
        let board = run.json();
        ["assigned_issues", "assigned_pull_requests"]
            .iter()
            .filter_map(|k| board[*k].as_array())
            .flatten()
            .filter_map(|i| i["title"].as_str().map(str::to_owned))
            .collect()
    };
    let selected = ["status", "--json", "assigned_issues,assigned_pull_requests", "--limit", "100"];
    eventually("the assigned issue and pull request reached the issue index", || {
        let got = titles(&selected);
        got.contains(&issue_title) && got.contains(&pr_title)
    });

    // `--org` narrows to one owner and must keep them; the sections come from one endpoint with
    // an `owner` parameter, so a filter applied to the wrong call would drop everything.
    let mut scoped = selected.to_vec();
    scoped.extend(["--org", &inst.user]);
    let got = titles(&scoped);
    assert!(got.contains(&issue_title), "--org {} dropped the assigned issue", inst.user);
    assert!(got.contains(&pr_title), "--org {} dropped the assigned pull request", inst.user);

    // `--exclude` is applied here rather than by the server, against `repository.full_name`, and
    // it must be exact: a prefix match would also hide a differently-named repository.
    let slug = repo.slug();
    let mut excluded = selected.to_vec();
    excluded.extend(["-e", &slug]);
    let got = titles(&excluded);
    assert!(
        !got.contains(&issue_title) && !got.contains(&pr_title),
        "-e {slug} left its own repository's items in the report: {got:?}"
    );

    // A malformed `--exclude` is a usage error, decided before anything is fetched.
    let bad = inst.gea(["status", "-e", "not-a-slug"]);
    bad.assert_code(2, "gea status -e with no owner");
    bad.assert_says("owner/name");

    // The fourth section is notifications, and the whole board renders on the human path too.
    let human = inst.gea(["status", "--limit", "100"]);
    human.assert_ok("gea status (human)");
    assert!(
        human.stdout.contains(&issue_title),
        "the human report omits the issue it was given:\n{}",
        human.stdout
    );
}

// ------------------------------------------------------------------------------------ browse

/// Every route `browse` composes, checked against the instance the URLs are supposed to name.
///
/// `-n` throughout, so nothing is ever opened. The interesting one is the last: a file path with
/// no `--branch` and no checkout is the single case that costs a request, because the default
/// branch has to come from the server. Run from a scratch directory rather than the crate's own,
/// or `gea` would find *this* repository's checked-out branch and the request would never happen.
#[test]
fn browse_composes_every_route_and_asks_the_server_only_when_it_must() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["browse"], hits: ["repoGet"]);

    let repo = TestRepo::create_initialized(inst, "browse");
    let outside = Scratch::new("browse");
    let root = format!("{}/{}", inst.base_url, repo.slug());

    let slug = repo.slug();
    let url = |args: &[&str]| -> String {
        let mut full: Vec<&str> = vec!["browse", "-n", "-R", &slug];
        full.extend_from_slice(args);
        let run = inst.gea_in(outside.path(), &full);
        run.assert_ok(&format!("gea {}", full.join(" ")));
        run.stdout.trim().to_owned()
    };

    assert_eq!(url(&[]), root, "the bare form opens the repository");
    // Issues and pull requests share one numbering sequence, and Gitea redirects the one route
    // to the other, so a number must not have to say which it is.
    assert_eq!(url(&["42"]), format!("{root}/issues/42"));
    assert_eq!(url(&["--issues"]), format!("{root}/issues"));
    assert_eq!(url(&["--pulls"]), format!("{root}/pulls"));
    assert_eq!(url(&["--wiki"]), format!("{root}/wiki"));
    assert_eq!(url(&["--releases"]), format!("{root}/releases"));
    assert_eq!(url(&["--settings"]), format!("{root}/settings"));
    assert_eq!(url(&["--actions"]), format!("{root}/actions"));
    assert_eq!(url(&["-b", "dev"]), format!("{root}/src/branch/dev"));
    assert_eq!(url(&["-c", "0123456789abcdef"]), format!("{root}/commit/0123456789abcdef"));
    assert_eq!(url(&["-b", "dev", "docs/a b.md"]), format!("{root}/src/branch/dev/docs/a%20b.md"));
    assert_eq!(
        url(&["-b", "dev", "src/main.rs:120"]),
        format!("{root}/src/branch/dev/src/main.rs#L120")
    );

    // The one request: no branch, no checkout, so the default branch comes from the repository.
    assert_eq!(
        url(&["src/main.rs:120"]),
        format!("{root}/src/branch/main/src/main.rs#L120"),
        "a file path outside a checkout has to be anchored to the server's default branch"
    );

    // Two destinations at once is a usage error rather than a silent preference for one.
    let clash = inst.gea_in(outside.path(), ["browse", "-n", "-R", &repo.slug(), "42", "--issues"]);
    clash.assert_code(2, "gea browse with a target and a target flag");
}

// -------------------------------------------------------------------------------- completion

/// Completions are the first thing a new user generates, often before `auth login`, and each
/// shell has to get its own script rather than whichever one was generated first.
///
/// The distinctness check is the part `crates/gea/tests/setup.rs` does not make: a `<SHELL>`
/// argument that was parsed and then ignored would still produce five successful runs naming
/// `gea`, and every assertion about content would pass.
#[test]
fn every_supported_shell_gets_its_own_non_empty_completion_script() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["completion"]);

    let cfg = Config::new("completion");
    let env = cfg.env();
    let mut scripts: BTreeSet<String> = BTreeSet::new();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let run = inst.gea_env(Path::new("."), &env, ["completion", shell]);
        run.assert_ok(&format!("gea completion {shell}"));
        assert!(!run.stdout.trim().is_empty(), "the {shell} script is empty");
        assert!(run.stdout.contains("gea"), "the {shell} script does not name the binary");
        assert!(
            scripts.insert(run.stdout.clone()),
            "the {shell} script is byte-identical to another shell's, so <SHELL> was ignored"
        );
    }
    // Generating them must need no configuration at all — no host, no token, no files written.
    assert!(!cfg.exists("hosts.toml"), "completion wrote a credential file");
    assert!(!cfg.exists("config.toml"), "completion wrote a configuration file");
}

// ------------------------------------------------------------------------------------ search

/// Forgejo 16.0.4 (measured for fjo, which gea was ported from) has **no code-search endpoint**: `/explore/code` is web UI only. So
/// `search code` cannot search, and what it owes the user is to say why and name what does work.
///
/// This is asserted against the live instance rather than only as a unit test because the claim
/// is about *this server*: the control below runs the alternative the message recommends and
/// shows it finds the repository, so the advice is checked rather than merely quoted. If a later
/// Gitea grows the endpoint, the right response is to implement the command — and this test is
/// where that shows up.
#[test]
fn search_code_refuses_and_the_alternative_it_names_really_works() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["search code", "search repos"], hits: ["repoSearch"]);

    let repo = TestRepo::create(inst, "searchcode");

    let run = inst.gea(["search", "code", &repo.name]);
    run.assert_code(2, "gea search code");
    run.assert_says("code search is only available in the web interface");
    run.assert_says("--web");
    run.assert_says("gea search repos");

    // The recommended alternative, against the same instance.
    let found = inst.gea(["search", "repos", &repo.name, "--json", "full_name", "--limit", "50"]);
    found.assert_ok("gea search repos");
    let names: BTreeSet<String> = found
        .json()
        .as_array()
        .expect("an array of repositories")
        .iter()
        .filter_map(|r| r["full_name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        names.contains(&repo.slug()),
        "`gea search repos` is what the refusal recommends, and it did not find {}: {names:?}",
        repo.slug()
    );
}

/// `search prs` is `/repos/issues/search` with `type=pulls`, and the filters have to reach the
/// server rather than being applied to whatever it sent back.
///
/// An organization of this test's own is the isolation: the instance is shared with eight other
/// test binaries, all acting as the same admin, so `--owner <that admin>` would be a moving
/// target. With one owner holding exactly one pull request, both the positive and the negative
/// assertion are exact.
#[test]
fn search_prs_is_scoped_by_the_owner_and_state_it_was_given() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["search prs"], hits: ["issueSearchIssues"]);

    let org = format!("prsearch{}", std::process::id());
    let (code, body) = inst.api("POST", "orgs", Some(&format!(r#"{{"username":"{org}"}}"#)));
    assert!((200..300).contains(&code) || code == 422, "could not create {org}: {code} {body}");

    let name = inst.unique_repo_name("prsearch-repo");
    let (code, body) = inst.api(
        "POST",
        &format!("orgs/{org}/repos"),
        Some(&format!(
            r#"{{"name":"{name}","private":true,"auto_init":true,"default_branch":"main"}}"#
        )),
    );
    assert!((200..300).contains(&code), "could not create {org}/{name}: HTTP {code}: {body}");

    let slug = format!("{org}/{name}");
    let api = |method: &str, path: &str, body: Option<&str>| {
        inst.api(method, &format!("repos/{slug}/{path}"), body)
    };
    let (code, body) = api("POST", "branches", Some(r#"{"new_branch_name":"feature"}"#));
    assert!((200..300).contains(&code), "could not branch: HTTP {code}: {body}");
    let content = gitea_core::http::base64::encode("one\n");
    let (code, body) = api(
        "POST",
        "contents/f.txt",
        Some(&format!(r#"{{"content":"{content}","message":"add f.txt","branch":"feature"}}"#)),
    );
    assert!((200..300).contains(&code), "could not commit: HTTP {code}: {body}");
    let title = format!("prsearch-{}", std::process::id());
    let (code, body) = api(
        "POST",
        "pulls",
        Some(&format!(r#"{{"title":"{title}","head":"feature","base":"main"}}"#)),
    );
    assert!((200..300).contains(&code), "could not open the pull request: HTTP {code}: {body}");

    let titles = |args: &[&str]| -> BTreeSet<String> {
        let run = inst.gea(args);
        run.assert_ok(&format!("gea {}", args.join(" ")));
        run.json()
            .as_array()
            .expect("an array of pull requests")
            .iter()
            .filter_map(|p| p["title"].as_str().map(str::to_owned))
            .collect()
    };

    eventually("the pull request reached the issue index", || {
        titles(&["search", "prs", "--owner", &org, "--json", "number,title", "--limit", "50"])
            .contains(&title)
    });

    // `--state closed` is a server-side filter, and the pull request is open, so an empty answer
    // here is the evidence that the parameter travelled rather than being dropped.
    assert!(
        titles(&[
            "search",
            "prs",
            "--owner",
            &org,
            "--state",
            "closed",
            "--json",
            "number,title",
            "--limit",
            "50"
        ])
        .is_empty(),
        "--state closed returned an open pull request, so the filter never reached the server"
    );
    // `search issues` over the same owner must not return it either: one endpoint, two `type`s.
    assert!(
        !titles(&["search", "issues", "--owner", &org, "--json", "number,title", "--limit", "50"])
            .contains(&title),
        "a pull request came back from `search issues`, so `type` did not reach the server"
    );

    let _ = inst.api("DELETE", &format!("repos/{slug}"), None);
    let _ = inst.api("DELETE", &format!("orgs/{org}"), None);
}

// ------------------------------------------------------------------------------- run runners

/// `run runners` answers "which runners are registered for this repository", with their labels.
///
/// The empty case matters as much as the populated one. With no runner anywhere, every job this
/// repository starts waits forever, and a listing that merely printed nothing would leave a user
/// staring at a `queued` status with no explanation — so the note is asserted too, including its
/// pointer at the org and instance scopes Gitea lists separately.
#[test]
fn a_registered_runner_is_visible_to_the_repository_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["run runners"],
        hits: ["repoCreateRunnerRegistrationToken", "getRepoRunners", "deleteRepoRunner"],
    );

    let repo = TestRepo::create(inst, "runners");

    // Empty first: this is the state a user hits before they have set anything up.
    let empty = inst.gea_env(
        Path::new("."),
        &[("GEA_FORCE_TTY", "100")],
        ["run", "runners", "-R", &repo.slug()],
    );
    empty.assert_ok("gea run runners with no runners");
    empty.assert_says("no runner is registered against this repository");
    empty.assert_says("act_runner register");

    let name = inst.unique_repo_name("repo-runner");
    let id = inst.register_runner(&format!("repos/{}", repo.slug()), &name, &["docker"]);

    let listed =
        inst.gea(["run", "runners", "-R", &repo.slug(), "--json", "id,name,status,labels"]);
    listed.assert_ok("gea run runners");
    let rows = listed.json();
    let mine = rows
        .as_array()
        .expect("an array of runners")
        .iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .unwrap_or_else(|| panic!("runner {id} is missing from the listing: {}", listed.stdout));
    assert_eq!(mine["name"].as_str(), Some(name.as_str()), "the id names a different runner");
    // Never connected, so never online.
    assert_eq!(mine["status"], "offline", "{mine}");

    // The human table renders labels as names, not as the `{id,name,type}` objects Gitea sends.
    let human = inst.gea_env(
        Path::new("."),
        &[("GEA_FORCE_TTY", "100")],
        ["run", "runners", "-R", &repo.slug()],
    );
    human.assert_ok("gea run runners (table)");
    let row = human.stdout.lines().find(|l| l.contains(&name)).expect("the runner's row");
    assert!(row.contains("docker") && !row.contains('{'), "{row}");

    let (code, body) = repo.api("DELETE", &format!("actions/runners/{id}"), None);
    assert!((200..300).contains(&code), "could not delete runner {id}: HTTP {code}: {body}");
    let gone = inst.gea(["run", "runners", "-R", &repo.slug(), "--json", "id"]);
    gone.assert_ok("gea run runners after the runner was deleted");
    assert!(
        !gone.json().as_array().expect("an array").iter().any(|r| r["id"].as_i64() == Some(id)),
        "a deleted runner is still listed: {}",
        gone.stdout
    );
}

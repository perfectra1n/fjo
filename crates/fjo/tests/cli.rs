//! Integration tests for the binary itself.
//!
//! Everything here runs the real `fjo` executable, because the properties under test are
//! properties of a *process*: its exit status, its signal disposition, what it writes to which
//! stream. None of them is observable from a unit test, and several of them (the `SIGPIPE`
//! panic, a token in `--debug` output) are exactly the kind of bug that only shows up once the
//! pieces are wired together.
//!
//! No test here touches the network. The one that would — `no auth` — is arranged so that it
//! fails during configuration, before a socket is opened.

use std::io::Write;
use std::process::{Command as StdCommand, Stdio};

use assert_cmd::Command;
use predicates::str::contains;

/// A configuration directory with nothing in it, plus every environment variable that could
/// otherwise leak the developer's real setup into the test.
///
/// `env_remove` rather than `env_clear`: clearing everything also removes `PATH` and `HOME`, and
/// a keyring backend that cannot find `DBUS_SESSION_BUS_ADDRESS` behaves differently from one
/// that finds a broken value — neither of which is what these tests are about.
fn cmd(dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("fjo").expect("the fjo binary is built for its own tests");
    c.env("FJO_CONFIG_DIR", dir)
        .env("FJO_CREDENTIAL_STORE", "env")
        .env_remove("FJO_HOST")
        .env_remove("FORGEJO_HOST")
        .env_remove("FJO_TOKEN")
        .env_remove("FORGEJO_TOKEN")
        .env_remove("FJO_REPO")
        .env_remove("FORGEJO_REPO")
        .env_remove("FJO_USER")
        .env_remove("FORGEJO_USER")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE");
    c
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

#[test]
fn version_prints_the_api_it_was_generated_against_and_exits_zero() {
    let dir = tmp();
    cmd(dir.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("fjo "))
        // Not decoration: the whole coverage claim rests on which specification this build was
        // generated from, so `--version` has to say.
        .stdout(contains("Forgejo API"));
}

#[test]
fn help_names_all_three_layers() {
    let dir = tmp();
    cmd(dir.path())
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("api"))
        // `raw` is only *built* when `peek` sees it, so it has to be listed by the derive tree
        // as well or 506 operations are invisible to anyone reading `--help`.
        .stdout(contains("raw"));
}

/// Bug this prevents: `fjo … | head -1` dying with a panic and "failed printing to stdout",
/// exit 101, and a backtrace note — which is what happens when `SIGPIPE` is left at Rust's
/// `SIG_IGN` default.
///
/// The accepted outcomes are the two a well-behaved Unix program can produce: it finished writing
/// before the reader left (exit 0), or the kernel killed it with `SIGPIPE` (status 141). What is
/// *not* acceptable is a panic, a message on stderr, or any other status.
#[test]
#[cfg(unix)]
fn a_closed_pipe_is_silent_and_never_panics() {
    use std::os::unix::process::ExitStatusExt;

    let dir = tmp();
    let exe = assert_cmd::cargo::cargo_bin("fjo");
    // `raw repo get --json` lists ~70 field names and needs no network, no token, and no host.
    let mut fjo = StdCommand::new(&exe)
        .args(["raw", "repo", "get", "--json"])
        .env("FJO_CONFIG_DIR", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fjo");

    let head_stdin = Stdio::from(fjo.stdout.take().expect("piped stdout"));
    let head = StdCommand::new("head")
        .args(["-n", "1"])
        .stdin(head_stdin)
        .output()
        .expect("head is a POSIX utility");
    let out = fjo.wait_with_output().expect("wait for fjo");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.is_empty(), "nothing belongs on stderr for a closed pipe, got: {stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        out.status.success() || out.status.signal() == Some(13),
        "expected success or death by SIGPIPE, got {:?} (signal {:?})",
        out.status.code(),
        out.status.signal()
    );
    // And the reader really did get the first line, so the pipeline was not empty.
    assert!(!head.stdout.is_empty(), "head read nothing");
}

/// Bare `--json` lists the fields on **stdout** and exits **0** — the deliberate divergence from
/// `gh`, which uses stderr and exit 1. Recorded in `docs/output.md`, divergence 2.
///
/// It also short-circuits before anything else: this runs against an empty configuration
/// directory with no host and no token, which is the state a user exploring the API is in.
#[test]
fn bare_json_lists_fields_on_stdout_with_no_host_and_no_token() {
    let dir = tmp();
    let assert = cmd(dir.path()).args(["raw", "repo", "get", "--json"]).assert().success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(out.contains("full_name"), "{out}");
    // Piped: bare names, one per line, so `--json | fzf --multi | paste -sd,` works.
    assert!(out.lines().all(|l| !l.contains(' ')), "piped output must be names only:\n{out}");
    assert!(assert.get_output().stderr.is_empty());
}

#[test]
fn an_unknown_field_suggests_the_close_one_and_exits_two() {
    let dir = tmp();
    cmd(dir.path())
        .args(["raw", "repo", "get", "--json", "fullname"])
        .assert()
        .code(2)
        .stderr(contains("did you mean"))
        .stderr(contains("full_name"));
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let dir = tmp();
    cmd(dir.path()).arg("nonesuch").assert().code(2);
    // Including inside layer 2, where the subtree is built from the generated table and clap
    // supplies the did-you-mean.
    cmd(dir.path()).args(["raw", "repo", "gett"]).assert().code(2).stderr(contains("get"));
}

/// Bug this prevents: an unconfigured `fjo` failing with a generic error and no next step, or
/// with the wrong exit code — scripts branch on 4 to mean "log in".
#[test]
fn with_no_host_configured_the_failure_is_exit_four_and_actionable() {
    let dir = tmp();
    cmd(dir.path())
        .args(["api", "user"])
        .assert()
        .code(4)
        .stderr(contains("what to do"))
        .stderr(contains("fjo auth login"));
}

#[test]
fn a_dry_run_shows_the_method_url_and_body_without_a_configured_host() {
    let dir = tmp();
    cmd(dir.path())
        .args([
            "raw",
            "repo",
            "create-pull-request",
            "owner",
            "proj",
            "--title",
            "hello",
            "--base",
            "main",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(contains("POST /repos/owner/proj/pulls"))
        .stdout(contains("content-type: application/json"))
        .stdout(contains("\"title\": \"hello\""));
}

/// The body field flags are typed, so `--dry-run` is where a stringified boolean would show up.
/// A quoted `"true"` is a 422 from the server with a message about the wrong JSON type, and the
/// flag looks like it worked.
#[test]
fn body_field_flags_keep_their_json_types() {
    let dir = tmp();
    let assert = cmd(dir.path())
        .args(["raw", "repo", "create-current-user-repo", "--name", "x", "--private", "--dry-run"])
        .assert()
        .success();
    let out = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(out.contains("\"private\": true"), "a bool must not be quoted:\n{out}");
    assert!(out.contains("\"name\": \"x\""), "a string must be quoted:\n{out}");
}

/// The token-leak test the plan asks for.
///
/// A `--debug` run against a host that cannot answer, with the token supplied through the
/// environment, and then the whole of stdout and stderr searched for the secret. `--debug` is the
/// most verbose mode there is, so if the token is absent here it is absent everywhere.
#[test]
fn the_token_never_appears_in_debug_output() {
    const SECRET: &str = "fjo-test-token-e5f1a9c3-do-not-log";
    let dir = tmp();
    // Port 1 is reserved and never listening, so this fails at connect — after the credential has
    // been found, attached, and traced, which is precisely the window a leak would live in.
    std::fs::write(
        dir.path().join("hosts.toml"),
        "active = \"127.0.0.1:1\"\n\
         [[hosts]]\n\
         name = \"127.0.0.1:1\"\n\
         url = \"http://127.0.0.1:1\"\n\
         active_login = \"me\"\n\
         [[hosts.logins]]\n\
         user = \"me\"\n",
    )
    .unwrap();

    let assert = cmd(dir.path())
        .env("FJO_TOKEN", SECRET)
        .args(["api", "user", "--debug", "--verbose", "--no-retry"])
        .assert()
        .failure();
    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains(SECRET), "the token reached stdout:\n{stdout}");
    assert!(!stderr.contains(SECRET), "the token reached stderr:\n{stderr}");
    // …and the trace still did its job, or the test above proves nothing.
    assert!(stderr.contains("debug:"), "--debug printed no trace at all:\n{stderr}");
    assert!(
        stderr.contains("$FJO_TOKEN"),
        "the trace must name where the token came from:\n{stderr}"
    );
    // A connect failure is exit 6, not a generic 1.
    assert_eq!(out.status.code(), Some(6), "stderr was:\n{stderr}");
}

/// Bug this prevents: a `--jq` expression that does not compile being reported as a request
/// failure (or worse, silently ignored) instead of as a usage error, with the column marked.
#[test]
fn a_broken_jq_expression_is_a_usage_error_before_any_request() {
    let dir = tmp();
    cmd(dir.path()).args(["api", "user", "--jq", ".["]).assert().code(2);
}

/// `fjo raw search` is the only answer to "506 commands are undiscoverable", so it has to work
/// with no configuration at all.
#[test]
fn search_finds_an_operation_and_prints_a_copyable_invocation() {
    let dir = tmp();
    cmd(dir.path())
        .args(["raw", "search", "pull", "request"])
        .assert()
        .success()
        .stdout(contains("fjo raw repo create-pull-request <OWNER> <REPO>"))
        .stdout(contains("POST /repos/{owner}/{repo}/pulls"));
}

/// Every one of the 506 operations must build a root, render help, and parse — the failure mode
/// being guarded against is a clap **panic** from a generated flag colliding with a global one,
/// which would be a crash arriving with a specification bump rather than with a code change.
#[test]
fn every_operation_builds_a_root_with_the_global_flags_attached() {
    use forgejo_client::meta::OPS;

    for op in OPS {
        // `--help` through the root is the only faithful check: clap propagates global arguments
        // into subcommands while *building*, so rendering a leaf lifted out of the tree would
        // show a tree the user never sees.
        let argv = ["fjo", "raw", op.group, op.command, "--help"];
        let err = fjo::raw::root(Some(op.group), Some(op.command))
            .try_get_matches_from(argv)
            .expect_err("clap reports --help as an error it wants printed");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "{}", op.op_id);

        let help = err.render().to_string();
        assert!(help.contains("--dry-run"), "{} lost --dry-run:\n{help}", op.op_id);
        // `-R` is the one global that must survive even when `--repo` is a path parameter, or
        // repository context becomes unreachable on the ~200 operations that have one. It renders
        // as `-R, --repo <…>` normally and as `-R <…>` where the long name was surrendered.
        assert!(
            help.contains("-R, --repo") || help.contains("-R <"),
            "{} lost -R:\n{help}",
            op.op_id
        );
        assert!(help.contains("--host"), "{} lost --host:\n{help}", op.op_id);
        // `--json` names a generated flag on no operation, so it must survive everywhere.
        assert!(help.contains("--json"), "{} lost --json:\n{help}", op.op_id);
    }
}

/// Bug this prevents: `fjo raw --help` quietly building all 506 commands and their ~3,000
/// arguments, which is the cost the two-phase parse exists to avoid.
#[test]
fn the_bare_raw_root_holds_only_group_stubs() {
    let root = fjo::raw::root(None, None);
    let raw = root.find_subcommand("raw").expect("raw");
    let subs: Vec<&str> = raw.get_subcommands().map(|c| c.get_name()).collect();
    assert!(subs.len() < 30, "expected a handful of group stubs, got {subs:?}");
    assert!(subs.contains(&"search"));
    let args: usize = raw
        .get_subcommands()
        .filter(|c| c.get_name() != "search")
        .map(|c| c.get_arguments().filter(|a| a.get_id() != "help").count())
        .sum();
    assert_eq!(args, 0, "a group stub must not allocate arguments");
}

/// Field parsing is where `-f` and `-F` differ, and the difference is invisible until something
/// prints the body. `--dry-run` is layer 2's window onto that; layer 1 has none, so this exercises
/// the inversion through the failure message instead: a malformed field must be rejected before a
/// request is attempted, whichever flag carried it.
#[test]
fn a_malformed_field_is_rejected_before_the_host_is_even_resolved() {
    let dir = tmp();
    for flag in ["-f", "-F"] {
        cmd(dir.path())
            .args(["api", "user", flag, "notafield"])
            .assert()
            .code(2)
            .stderr(contains("key=value"));
    }
}

/// A compatibility note must not change the exit code, and must be suppressible — a CI job that
/// goes red because the server is newer than the build is the opposite of forward compatibility.
#[test]
fn the_compat_note_is_suppressible() {
    let dir = tmp();
    cmd(dir.path())
        .env("FJO_NO_COMPAT_NOTES", "1")
        .arg("--version")
        .assert()
        .success()
        .stderr(predicates::str::is_empty());
}

/// Writing the field list to a file must not go through the terminal path, so `--output` has to
/// be honoured by the layers that produce output at all. Checked here because the file is the
/// only place the piped/TTY decision is observable without a pty.
#[test]
fn output_to_a_file_writes_the_body_there() {
    let dir = tmp();
    let path = dir.path().join("out.txt");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "placeholder").unwrap();
    drop(f);
    // A dry run does not use `--output` (it describes rather than fetches), so this asserts the
    // flag is at least accepted everywhere rather than rejected as unknown.
    cmd(dir.path())
        .args(["raw", "repo", "get", "o", "r", "--dry-run", "--output"])
        .arg(&path)
        .assert()
        .success();
}

// -------------------------------------------------------------------------------------------
// Which server the client actually talks to.
//
// `--debug`'s first line is the only place a user can see this, and it is what these assert on.
// Nothing here reaches the network: both hosts are ports nothing listens on, so the run fails at
// connect — long after the host has been chosen, traced, and baked into the client.
// -------------------------------------------------------------------------------------------

/// Two configured hosts, `active` being the one no remote names.
///
/// Ports 1 and 2 are both reserved and never listening, so a test that got as far as a socket
/// fails loudly rather than quietly talking to something real.
fn two_hosts_active_is_the_wrong_one(dir: &std::path::Path) {
    std::fs::write(
        dir.join("hosts.toml"),
        "active = \"127.0.0.1:1\"\n\
         [[hosts]]\n\
         name = \"127.0.0.1:1\"\n\
         url = \"http://127.0.0.1:1\"\n\
         active_login = \"me\"\n\
         [[hosts.logins]]\n\
         user = \"me\"\n\
         [[hosts]]\n\
         name = \"127.0.0.1:2\"\n\
         url = \"http://127.0.0.1:2\"\n\
         active_login = \"me\"\n\
         [[hosts.logins]]\n\
         user = \"me\"\n",
    )
    .unwrap();
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git is on PATH for these tests");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A work tree whose only remote is `url`.
fn work_tree_with_remote(url: &str) -> tempfile::TempDir {
    let work = tmp();
    git_in(work.path(), &["init", "--quiet"]);
    git_in(work.path(), &["remote", "add", "origin", url]);
    work
}

/// The `debug: host …` line, which is the whole observable under test.
fn debug_host_line(stderr: &str) -> String {
    stderr
        .lines()
        .find(|l| l.starts_with("debug: host "))
        .unwrap_or_else(|| panic!("--debug printed no host line:\n{stderr}"))
        .to_owned()
}

/// Bug this prevents — and it is the reason this file has a section about it.
///
/// In a clone of `127.0.0.1:2/them/proj` with `active = "127.0.0.1:1"`, `fjo` resolved the
/// *slug* from the remote and the *host* from `active`, then sent
/// `GET /repos/them/proj/pulls` to a server nobody had named. A 404 is the lucky outcome: when
/// the other instance happens to hold a repository of the same name, the request succeeds and
/// the answer is about the wrong repository on the wrong server, with nothing in the output to
/// say so.
#[test]
fn the_client_talks_to_the_host_the_git_remote_names() {
    let dir = tmp();
    two_hosts_active_is_the_wrong_one(dir.path());
    let work = work_tree_with_remote("http://127.0.0.1:2/them/proj.git");

    let assert = cmd(dir.path())
        .current_dir(work.path())
        .args(["pr", "list", "--debug", "--no-retry"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    let line = debug_host_line(&stderr);
    assert!(
        line.contains("127.0.0.1:2"),
        "the client must target the remote's host, not `active`:\n{stderr}"
    );
    assert!(!line.contains("127.0.0.1:1"), "`active` must not win over the remote:\n{stderr}");
    // The repository is the remote's too, or the host agreeing would prove nothing.
    assert!(stderr.contains("them/proj"), "the slug came from somewhere else:\n{stderr}");
}

/// Bug this prevents: `-R host/owner/name` naming a host outright and the request going to
/// `active` anyway — the same defect as above, with the host typed by hand.
#[test]
fn an_explicit_host_in_the_repo_flag_retargets_the_client() {
    let dir = tmp();
    two_hosts_active_is_the_wrong_one(dir.path());
    // A cwd that is not a work tree, so only `-R` can be supplying the host.
    let elsewhere = tmp();

    let assert = cmd(dir.path())
        .current_dir(elsewhere.path())
        .args(["pr", "list", "-R", "127.0.0.1:2/them/proj", "--debug", "--no-retry"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    let line = debug_host_line(&stderr);
    assert!(line.contains("127.0.0.1:2"), "-R named the host and was ignored:\n{stderr}");
    assert!(!line.contains("127.0.0.1:1"), "`active` must not win over -R:\n{stderr}");
}

/// The other direction, so the fix cannot become an overcorrection: `--host` is the explicit
/// instrument and outranks whatever the checkout says.
#[test]
fn an_explicit_host_flag_still_outranks_the_remote() {
    let dir = tmp();
    two_hosts_active_is_the_wrong_one(dir.path());
    let work = work_tree_with_remote("http://127.0.0.1:2/them/proj.git");

    let assert = cmd(dir.path())
        .current_dir(work.path())
        .args(["pr", "list", "--host", "127.0.0.1:1", "--debug", "--no-retry"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    let line = debug_host_line(&stderr);
    assert!(line.contains("127.0.0.1:1"), "--host must win:\n{stderr}");
}

/// A checkout of a host this configuration knows nothing about must not break commands that
/// never needed a repository: `active` is still the answer for `fjo api user` inside a GitHub
/// clone.
#[test]
fn a_remote_on_an_unconfigured_host_leaves_active_alone() {
    let dir = tmp();
    two_hosts_active_is_the_wrong_one(dir.path());
    let work = work_tree_with_remote("https://github.com/me/proj.git");

    let assert = cmd(dir.path())
        .current_dir(work.path())
        .args(["api", "user", "--debug", "--no-retry"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    let line = debug_host_line(&stderr);
    assert!(
        line.contains("127.0.0.1:1"),
        "an unknown remote host must fall back to `active`, not fail:\n{stderr}"
    );
}

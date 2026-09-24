//! `cargo xtask itest` — run the integration suite against a real Gitea.
//!
//! The container lifecycle deliberately lives in `crates/gea-itest`, not here: the tests
//! themselves need to boot, reap and inspect instances, and duplicating that in the generator
//! crate would mean two implementations drifting apart. This subcommand's whole job is to run
//! `cargo test` with the right environment, and to make a skip impossible.
//!
//! That last part is the reason this exists rather than being a line in the README. The suite
//! *skips* when no Gitea is reachable, so that `cargo test --workspace` works on a laptop
//! without Docker — and a skip prints "ok". A CI job that silently tested nothing is worse than
//! one that failed, so `xtask itest` sets `GEA_ITEST_REQUIRE=1` and turns a skip into an error.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::Result;

pub struct Options {
    /// Leave the container running afterwards, for poking at by hand.
    pub keep: bool,
    /// Override the image, e.g. to try a newer Gitea than the vendored spec targets.
    pub image: Option<String>,
    /// Permit a skip. Only for a developer without Docker; CI must never pass this.
    pub allow_skip: bool,
    /// Passed through to `cargo test` as a filter.
    pub filter: Option<String>,
    /// Wall-clock ceiling for the whole suite. `None` uses [`DEFAULT_TIMEOUT`].
    pub timeout: Option<Duration>,
}

/// How long the whole suite may run before it is killed.
///
/// # Why this exists at all
///
/// `cargo test` has **no timeout**, and that is the one thing `.config/nextest.toml` was
/// written to fix for the hermetic suite: "a test that hangs hangs the whole run with no output
/// and no name — which is exactly what a pagination bug did here once". The integration suite
/// kept `cargo test` for an unrelated and still-good reason (one container per test *binary*
/// rather than per test), and inherited that hole along with it.
///
/// The hole got bigger when this suite grew from six test binaries to fifteen: more tests, each
/// driving a real server over HTTP, is more surface for something to block forever on a socket.
///
/// # Why a whole-suite ceiling and not a per-test one
///
/// A per-test clock needs a per-test runner, which is nextest, which is the container model
/// this subcommand deliberately does not use. So this buys the lesser guarantee honestly: the
/// run always terminates, and the failure says which guarantee it is and how to get the better
/// one. That is strictly better than a CI-level `timeout-minutes`, which kills the job with no
/// explanation at all.
///
/// Sized against a measured run rather than guessed: see the note this constant's failure
/// message prints.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often to ask whether the suite has finished. Short enough that the overshoot past the
/// deadline is invisible, long enough not to spin a core for half an hour.
const POLL: Duration = Duration::from_millis(250);

pub fn run(root: &Path, opts: Options) -> Result<()> {
    // Deliberately `cargo test`, not `cargo nextest run`, even though the rest of the
    // workspace moved to nextest in `mise run test`.
    //
    // nextest runs every test in its OWN PROCESS. This crate's harness shares one Gitea
    // instance per process (the `OnceLock` in `gea_itest::shared()`) and force-reaps every
    // labelled container on boot. Under `cargo test` that is one container per test BINARY —
    // six — and the reap only collects corpses from an earlier run. Under nextest it becomes
    // one container per TEST, each new boot deleting the server the running tests are using.
    // Measured, not assumed: `cargo nextest run --workspace` left 11+ `gea-itest-<pid>`
    // containers up at once and failed 8 tests, in a suite `cargo test` runs green.
    //
    // .config/nextest.toml serialises the crate into a `gitea-container` test group so the
    // deliberate `--ignore-default-filter` path works, but that is ~3x slower and boots one
    // container per test. This is the supported runner, and the faster one.
    //
    // The exit code this function depends on survives either way, for the record: `cargo test`
    // exits 101 on a failure and nextest exits 100, and this checks `status.success()`, not a
    // specific number. So the choice above is about the container model alone.
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(root).args(["test", "--package", "gea-itest"]);

    // The suite drives the `gea` binary, so it has to exist and be current.
    build_binary(root)?;

    if let Some(filter) = &opts.filter {
        cmd.arg("--").arg(filter);
    }

    // Coverage is recorded by the tests themselves, into one journal per process (see
    // crates/gea-itest/src/coverage.rs). Stale journals are cleared first: a previous run's
    // records would otherwise make a suite that has since stopped driving an operation keep
    // looking covered, which is the one direction this measurement must not drift.
    let coverage_dir = coverage_dir(root)?;
    cmd.env(crate::coverage::DIR_ENV, &coverage_dir);

    if !opts.allow_skip {
        cmd.env("GEA_ITEST_REQUIRE", "1");
    }
    if opts.keep {
        cmd.env("GEA_ITEST_KEEP", "1");
    }
    if let Some(image) = &opts.image {
        cmd.env("GEA_ITEST_IMAGE", image);
    }

    let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let status = wait_with_timeout(cmd, timeout)?;
    if !status.success() {
        bail!(
            "the integration suite failed.\n\
             If the failure was \"no Gitea instance available\", either make Docker usable or \
             point the suite at an existing instance:\n\
             \n    GEA_TEST_HOST=git.example.org GEA_TEST_TOKEN=... cargo xtask itest\n\
             \nTo inspect a failing instance, re-run with --keep."
        );
    }
    Ok(())
}

/// The integration tests invoke `gea` as a subprocess, so a stale binary would silently test
/// the previous build — the kind of failure that wastes an afternoon.
fn build_binary(root: &Path) -> Result<()> {
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["build", "--package", "gea"])
        .status()
        .map_err(|e| format!("could not build the gea binary: {e}"))?;
    if !status.success() {
        bail!("`cargo build -p gea` failed, so there is nothing to integration-test");
    }
    Ok(())
}

/// The journal directory, emptied of the previous run's live records.
///
/// Only `live-*.jsonl` is removed. The porcelain inventory and the hermetic contract journal
/// are written by the *other* suite, and deleting them here would make
/// `cargo xtask coverage-check` report a contract gap of 506 whenever the integration suite ran
/// second — a failure with nothing wrong behind it.
fn coverage_dir(root: &Path) -> Result<std::path::PathBuf> {
    let dir = root.join(crate::coverage::DEFAULT_DIR);
    std::fs::create_dir_all(&dir)?;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("live-") {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(dir)
}

/// Run `cmd` to completion, killing it if it outlives `timeout`.
///
/// `Command::status()` blocks forever, which is the behaviour [`DEFAULT_TIMEOUT`] exists to
/// remove. Polling `try_wait` rather than spawning a watchdog thread keeps the child handle on
/// one thread, so the kill cannot race a successful exit: by the time this decides to kill, it
/// has already observed that `try_wait` returned `None`.
fn wait_with_timeout(mut cmd: Command, timeout: Duration) -> Result<std::process::ExitStatus> {
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("could not run cargo test: {e}"))?;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(format!("could not wait for cargo test: {e}").into()),
        }

        if started.elapsed() >= timeout {
            // Best-effort: if the kill fails the process is already gone, which is the outcome
            // we wanted. Either way the message below is what the developer needs.
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the integration suite did not finish within {}s and was killed.\n\
                 \n\
                 This is a whole-suite ceiling, so it cannot name the test that hung. To get a \
                 per-test clock and a named failure, re-run under nextest:\n\
                 \n    cargo nextest run -p gea-itest --ignore-default-filter\n\
                 \n\
                 That boots one Gitea per test rather than per test binary (see \
                 .config/nextest.toml), so it is slower — but it reports the offender BY NAME.\n\
                 \n\
                 If the suite has simply grown past this ceiling, raise it deliberately with \
                 --timeout-secs and record the measured duration.",
                timeout.as_secs()
            );
        }

        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: the watchdog never firing, so `cargo xtask itest` keeps the exact
    /// behaviour it was added to remove — a run that hangs forever with no output and no name.
    ///
    /// `sleep` rather than a Rust helper binary because this must exercise the real
    /// spawn/poll/kill path on a real child process; a mocked one would only test the loop.
    #[cfg(unix)]
    #[test]
    fn a_child_that_outlives_the_ceiling_is_killed_and_told_how_to_get_a_named_failure() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");

        let started = Instant::now();
        let err = wait_with_timeout(cmd, Duration::from_millis(600))
            .expect_err("a 30s sleep must not survive a 600ms ceiling")
            .to_string();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the watchdog waited {:?}, so it did not kill the child",
            started.elapsed()
        );
        // The remedy matters as much as the kill: a ceiling cannot name the offending test, so
        // the message has to hand over the runner that can.
        assert!(err.contains("did not finish within"), "{err}");
        assert!(err.contains("cargo nextest run -p gea-itest"), "{err}");
    }

    /// Bug this prevents: the poll loop racing a fast child and reporting a kill for a run that
    /// actually succeeded — which would turn the guard into a source of red CI on green code.
    #[cfg(unix)]
    #[test]
    fn a_child_that_finishes_inside_the_ceiling_reports_its_own_exit_status() {
        let ok = wait_with_timeout(Command::new("true"), Duration::from_secs(30))
            .expect("`true` exits promptly, it does not hang");
        assert!(ok.success(), "a succeeding child must surface as success");

        let failed = wait_with_timeout(Command::new("false"), Duration::from_secs(30))
            .expect("`false` exits promptly, it does not hang");
        assert!(!failed.success(), "a failing child must surface as a failed status, not a kill");
    }
}

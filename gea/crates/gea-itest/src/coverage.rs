//! Which commands the live suite actually drove, recorded *as it runs*.
//!
//! # Why a runtime journal and not a grep over the sources
//!
//! The obvious way to answer "is every operation covered?" is to scan the test files for
//! mentions, the way `.mise/scan.py` scans for `unwrap()`. That is the wrong instrument here,
//! for a reason specific to this crate: [`crate::instance_or_skip`] makes a test **`return`
//! early** when no Gitea is reachable, printing `SKIPPED` and passing. A source scan cannot
//! tell that apart from a test that ran, so on a machine without Docker — or in a CI job where
//! the daemon died — the gate would report full coverage over a suite that executed nothing.
//! That is the exact failure `cargo xtask itest` already exists to prevent (it sets
//! `GEA_ITEST_REQUIRE` so a skip becomes an error), and a coverage gate that reintroduces it
//! would be worse than no gate.
//!
//! So [`record`] is called *inside* the test body, after the instance has been acquired. A
//! skipped test records nothing, an `#[ignore]`d test records nothing, and a test whose
//! assertions were commented out still records nothing unless the `cover!` line itself ran.
//! The number cannot be inflated by a test that did not execute.
//!
//! # Why one file per process
//!
//! `cargo test` compiles every file in `tests/` into its **own binary**, and the supported
//! runner (`cargo xtask itest`) runs them concurrently. A shared journal would interleave
//! writes from six processes. Naming each journal after its pid sidesteps that entirely, and
//! `cargo xtask coverage-check` unions them — coverage is an aggregate question, so nothing is
//! lost by splitting the record.
//!
//! # Why writing is opt-in
//!
//! Recording happens only when [`DIR_ENV`] is set, which `cargo xtask itest` does after
//! clearing the directory. A bare `cargo test -p gea-itest` therefore leaves no journal rather
//! than leaving a *partial* one, which matters because a partial journal read by the ratchet
//! would understate coverage and demand budgets be raised — the one direction a ratchet must
//! never be pushed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Directory for coverage journals. Set by `cargo xtask itest`; unset means record nothing.
///
/// Shared with the hermetic plane: `crates/gea/tests/porcelain_inventory.rs` writes the
/// porcelain command inventory into the same directory, so `cargo xtask coverage-check` has one
/// place to look for both halves of the picture.
pub const DIR_ENV: &str = "GEA_COVERAGE_DIR";

/// Which surface an entry names.
///
/// The two are counted separately because they answer different questions: `Raw` is "does this
/// API operation work against a real server", `Porcelain` is "does this hand-written command
/// work end to end". A porcelain test credits both, via `cover!`'s `hits:` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A generated layer-2 operation, named by its `op_id` (e.g. `repoAddTopic`).
    Raw,
    /// A layer-3 leaf command, named by its space-joined path (e.g. `topic add`).
    Porcelain,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Raw => "raw",
            Kind::Porcelain => "porcelain",
        }
    }
}

/// The journal for this process, opened once.
///
/// `None` when [`DIR_ENV`] is unset or the file could not be opened. A coverage journal is
/// diagnostics, never a test result: failing a passing test because a directory was read-only
/// would make the gate an obstacle rather than a measurement.
fn journal() -> Option<&'static Mutex<File>> {
    static JOURNAL: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    JOURNAL
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os(DIR_ENV)?);
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("live-{}.jsonl", std::process::id()));
            let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
            Some(Mutex::new(file))
        })
        .as_ref()
}

/// Record that `ids` were exercised, from `site` (a `file:line`).
///
/// Prefer the `cover!` macro, which fills `site` in for you.
///
/// The whole batch is rendered into one buffer and written with a single `write_all`. Tests
/// inside one binary run on parallel threads, and an append-mode write under 4 KiB is atomic on
/// the platforms this runs on — but the `Mutex` makes that a property of this code rather than
/// of the filesystem, which is cheaper to reason about than to verify.
pub fn record(kind: Kind, ids: &[&str], site: &str) {
    let Some(journal) = journal() else { return };
    let buf = render(kind, ids, site);

    if let Ok(mut file) = journal.lock() {
        let _ = file.write_all(buf.as_bytes());
        let _ = file.flush();
    }
}

/// The exact bytes [`record`] appends, as a pure function.
///
/// Split out so the format — the part with a real bug budget, since `cargo xtask
/// coverage-check` parses it — is unit-testable without touching the filesystem or the
/// process environment. `std::env::set_var` is `unsafe` in edition 2024 and racy besides,
/// so a test that configured a journal by mutating the environment would be both.
fn render(kind: Kind, ids: &[&str], site: &str) -> String {
    let mut buf = String::with_capacity(ids.len() * 96);
    for id in ids {
        let line = serde_json::json!({ "kind": kind.as_str(), "id": id, "site": site });
        buf.push_str(&line.to_string());
        buf.push('\n');
    }
    buf
}

/// Declare which commands and operations a test exercised.
///
/// Call it **after** `instance_or_skip!`, so a skipped test records nothing — see this module's
/// documentation for why that ordering is the whole point.
///
/// ```ignore
/// let inst = instance_or_skip!();
/// cover!(raw: ["repoListTopics"]);
/// cover!(porcelain: ["topic add", "topic list"], hits: ["repoAddTopic", "repoListTopics"]);
/// ```
///
/// `hits:` is what lets a porcelain test pay for the endpoints underneath it. Those ids are
/// validated against `spec/name-lock.toml` by `cargo xtask coverage-check`, so a typo or an
/// operation renamed by a spec bump fails the gate instead of quietly crediting nothing.
///
/// Lists are bracketed rather than bare-comma-separated on purpose: `macro_rules!` does not
/// backtrack, so `$($id:expr),+ , hits: [...]` would try to parse `hits` as an expression and
/// fail with an error pointing at the wrong token.
#[macro_export]
macro_rules! cover {
    (raw: [$($id:expr),+ $(,)?] $(,)?) => {
        $crate::coverage::record(
            $crate::coverage::Kind::Raw,
            &[$($id),+],
            concat!(file!(), ":", line!()),
        );
    };
    (porcelain: [$($p:expr),+ $(,)?], hits: [$($h:expr),+ $(,)?] $(,)?) => {
        $crate::coverage::record(
            $crate::coverage::Kind::Porcelain,
            &[$($p),+],
            concat!(file!(), ":", line!()),
        );
        $crate::coverage::record(
            $crate::coverage::Kind::Raw,
            &[$($h),+],
            concat!(file!(), ":", line!()),
        );
    };
    (porcelain: [$($p:expr),+ $(,)?] $(,)?) => {
        $crate::coverage::record(
            $crate::coverage::Kind::Porcelain,
            &[$($p),+],
            concat!(file!(), ":", line!()),
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: a journal written even with no directory configured, which would
    /// scatter `live-*.jsonl` files into whatever directory a developer happened to run
    /// `cargo test` from.
    #[test]
    fn nothing_is_written_when_the_directory_is_not_configured() {
        // `journal()` memoises, so this asserts the decision function rather than calling
        // `record` (which would poison the `OnceLock` for the rest of this binary).
        if std::env::var_os(DIR_ENV).is_none() {
            assert!(journal().is_none(), "a journal was opened with {DIR_ENV} unset");
        }
    }

    /// Bug this prevents: the two kinds sharing a string, which would let a porcelain path be
    /// counted as an operation id and inflate raw coverage by 243.
    #[test]
    fn the_two_kinds_are_distinguishable_in_the_journal() {
        assert_ne!(Kind::Raw.as_str(), Kind::Porcelain.as_str());
    }

    /// Bug this prevents: a multi-id `cover!` emitting one line holding an array, which the
    /// ratchet's line-oriented reader would count as a single unknown operation.
    #[test]
    fn every_id_gets_its_own_line() {
        let out = render(Kind::Raw, &["repoAddTopic", "repoListTopics"], "tests/topic.rs:31");
        assert_eq!(out.lines().count(), 2, "{out}");
        for line in out.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            assert_eq!(v["kind"], "raw");
            assert_eq!(v["site"], "tests/topic.rs:31");
        }
    }

    /// Bug this prevents: a porcelain path being written raw into JSON. The ids contain spaces
    /// today and nothing stops a future one containing a quote; hand-rolled formatting would
    /// produce a line the ratchet silently drops.
    #[test]
    fn a_porcelain_path_survives_the_round_trip_with_its_spaces() {
        let out = render(Kind::Porcelain, &["admin user create"], "tests/admin.rs:12");
        let v: serde_json::Value = serde_json::from_str(out.trim()).expect("JSON");
        assert_eq!(v["id"], "admin user create");
        assert_eq!(v["kind"], "porcelain");
    }

    /// Bug this prevents: a `cover!` arm failing to match and the error pointing at `hits`
    /// rather than at the call. `macro_rules!` does not backtrack, so arm order and the
    /// bracketed lists are load-bearing; this is the test that exercises all three shapes.
    ///
    /// With `GEA_COVERAGE_DIR` unset these are no-ops, so this is a compile-time assertion
    /// wearing a test's clothes — which is exactly what is needed, since a macro that does not
    /// match is a build failure in every file that uses it.
    #[test]
    fn all_three_cover_shapes_expand() {
        crate::cover!(raw: ["repoAddTopic"]);
        crate::cover!(raw: ["repoAddTopic", "repoListTopics",]);
        crate::cover!(porcelain: ["topic add"]);
        crate::cover!(porcelain: ["topic add", "topic list"], hits: ["repoAddTopic"]);
        crate::cover!(porcelain: ["topic add"], hits: ["repoAddTopic", "repoListTopics",],);
    }
}

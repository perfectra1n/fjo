//! Forward-compatibility notes, collected during a run and reported once at exit.
//!
//! This is the observable half of the tolerant-parsing strategy. Open enums accept unknown
//! values and unparseable timestamps become `None`, both silently as far as the request is
//! concerned — that is deliberate, because a newer server must never break the CLI. But
//! silence would also hide a real signal, so every such event is recorded here and `main`
//! drains the set into a single grouped note on stderr.
//!
//! The note never changes the exit code. Suppress it with `GEA_NO_COMPAT_NOTES=1`.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// One thing the server sent that this build did not fully understand.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Note {
    /// An enum value absent from the spec this build was generated from.
    UnknownEnum { type_name: &'static str, value: String },
    /// A value we could not parse and defaulted instead.
    Unparsed { what: &'static str, value: String },
    /// Entries a list response carried as `null`, which were left out of the result.
    NullRows { request: String },
}

fn store() -> &'static Mutex<BTreeSet<Note>> {
    static STORE: OnceLock<Mutex<BTreeSet<Note>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Truncated so a pathological response body cannot produce an unreadable note.
fn clip(v: &str) -> String {
    const MAX: usize = 64;
    if v.chars().count() <= MAX {
        return v.to_owned();
    }
    let head: String = v.chars().take(MAX).collect();
    format!("{head}…")
}

/// Record an enum value this build does not know. Called from generated `Deserialize` impls.
pub fn note_unknown_enum(type_name: &'static str, value: &str) {
    // A poisoned mutex must not take the process down over a diagnostic.
    if let Ok(mut s) = store().lock() {
        s.insert(Note::UnknownEnum { type_name, value: clip(value) });
    }
}

/// Record a value we could not parse and defaulted instead.
pub fn note_unparsed(what: &'static str, value: &str) {
    if let Ok(mut s) = store().lock() {
        s.insert(Note::Unparsed { what, value: clip(value) });
    }
}

/// Record that a list response carried `null` rows, which were left out.
///
/// Gitea converts each row of a listing separately and puts a nil in the array for one it fails
/// to convert — `convert.ToRepo` does for a fork whose parent it cannot load. Failing the whole
/// listing over it made `gea repo list` permanently unusable for an account that owned one such
/// repository; dropping it silently would hide a row. So it is dropped *and* reported here.
pub fn note_null_rows(request: &str) {
    if let Ok(mut s) = store().lock() {
        s.insert(Note::NullRows { request: clip(request) });
    }
}

/// Take everything collected so far, leaving the store empty.
pub fn drain() -> Vec<Note> {
    store().lock().map(|mut s| std::mem::take(&mut *s).into_iter().collect()).unwrap_or_default()
}

/// Discard everything collected so far without reporting it.
///
/// The note this module exists to print is a *diagnostic about the current command*: "the server
/// sent an issue state this build does not know". That framing breaks in two situations, and
/// both need a way to throw the collection away rather than let it leak into an unrelated
/// report.
///
/// The first is a deliberately-discarded response. A capability probe, a 404-disambiguation
/// probe, a speculative request whose failure is expected — these deserialize a body whose
/// unknown values the user never asked about and cannot act on, and reporting them attributes a
/// warning to a command that did not produce it.
///
/// The second is a long-lived process: a test harness, or anything calling this crate as a
/// library across many operations. Notes accumulate for the life of the process, so without a
/// reset the tenth operation reports the first operation's unknowns.
///
/// Prefer [`drain`] whenever the notes might still be worth showing — this is the explicit
/// "I have decided these are noise" call, and being explicit is the point.
pub fn forget() {
    let _ = drain();
}

pub fn is_suppressed() -> bool {
    std::env::var_os("GEA_NO_COMPAT_NOTES").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Serialises tests that read or reset the store.
///
/// This module is a deliberate process-global singleton — arbitrary `Deserialize` impls deep
/// inside generated code must be able to reach it with no handle threaded through. But
/// `cargo test` runs tests in parallel threads, so two tests touching that global race: one
/// calls `drain()` while the other is between its `note_*` and its own `drain()`, and the second
/// sees an empty set. Crate-visible because tests elsewhere in the crate assert on notes too.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    // A panicking test poisons the mutex; the remaining tests should still run.
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        super::test_guard()
    }

    #[test]
    fn clips_long_values() {
        let _g = guard();
        drain();
        let long = "x".repeat(200);
        note_unparsed("timestamp", &long);
        let notes = drain();
        let Note::Unparsed { value, .. } = &notes[0] else { panic!("wrong variant") };
        assert!(value.chars().count() <= 65, "got {} chars", value.chars().count());
        assert!(value.ends_with('…'));
    }

    #[test]
    fn deduplicates() {
        let _g = guard();
        drain();
        // A list of 500 issues all carrying the same unknown state should produce one note,
        // not 500.
        for _ in 0..500 {
            note_unknown_enum("StateType", "draft");
        }
        assert_eq!(drain().len(), 1);
    }

    #[test]
    fn forget_discards_without_reporting() {
        let _g = guard();
        forget();
        note_unknown_enum("StateType", "draft");
        forget();
        assert!(drain().is_empty(), "forget must leave nothing behind");
    }

    #[test]
    fn drain_leaves_the_store_empty() {
        let _g = guard();
        drain();
        note_unparsed("timestamp", "nope");
        assert_eq!(drain().len(), 1);
        assert!(drain().is_empty(), "a second drain must not repeat the same notes");
    }
}

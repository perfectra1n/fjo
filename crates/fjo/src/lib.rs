//! Library half of the `fjo` binary.
//!
//! `src/main.rs` is the binary; everything it needs lives here so that the pieces are
//! integration-testable from `tests/`. A `#[cfg(test)]` module inside a `bin` target cannot be
//! reached by an integration test, and `tests/output_*.rs` needs a library to `use`.
//!
//! `main.rs` should reach this code as `fjo::output::…` rather than declaring `mod output;`
//! of its own, which would compile the module a second time into the binary.
//! `main.rs` is the one place that may contain `unsafe`, for the single
//! `signal(SIGPIPE, SIG_DFL)` call that has to happen before anything writes to stdout. It is a
//! separate crate root, so the `forbid` below does not reach it and does not have to be relaxed
//! for everything else.
// This crate is `publish = false`, so its rustdoc exists for contributors, who read it with
// `--document-private-items`. Module docs here deliberately link to the private helpers they
// describe — that is the useful thing to link to when explaining how a module works — and those
// links resolve under that flag. Suppressing the lint keeps the links navigable rather than
// demoting them to inert code spans. The published crates (forgejo-core, -model, -client) do
// NOT carry this allow: docs.rs renders no private items, so there a link to one is genuinely
// broken for the only audience that sees it.
#![allow(rustdoc::private_intra_doc_links)]
#![forbid(unsafe_code)]
// `forgejo_core::Error` is 16 bytes but `ErrorKind` behind it is not, and clippy measures the
// enum rather than the handle on some paths. The right fix is upstream in `error/mod.rs`; the
// same allow already sits at the top of `forgejo-core`'s `config` and `context` modules.
#![allow(clippy::result_large_err)]

pub mod api;
pub mod cmd;
pub mod exit;
pub mod global;
pub mod oauth_refresh;
pub mod output;
pub mod raw;
pub mod runtime;
pub mod web;

/// The one-line description, shared by both parse phases so `fjo --help` and
/// `fjo raw … --help` cannot describe two different tools.
pub const ABOUT: &str = "A command-line interface for Forgejo";

pub const LONG_ABOUT: &str = "\
A command-line interface for Forgejo.

Choose a command:

  fjo api <endpoint>        any REST endpoint, by path
  fjo raw <group> <op>      commands generated from the API specification
  fjo <noun> <verb>         common tasks such as pr list and repo clone

`fjo raw search <words>` finds an operation when you do not know its name.";

/// The Forgejo release the vendored API description came from, e.g. `16.0.4`.
///
/// A re-export of the constant `cargo xtask codegen` emits into `forgejo-client`, so the version
/// in `fjo --version` and in the `User-Agent` is the same value that stamped the generated
/// tree. It used to be parsed out of an `include_str!` of `spec/lock.toml`, which only worked
/// because this crate is `publish = false` — `cargo package` drops files outside the package
/// root, so the same trick in a published crate would break the publish.
pub fn spec_version() -> &'static str {
    forgejo_client::SPEC_VERSION
}

/// `fjo 0.1.0 (Forgejo API 16.0.4)` — the version string both parse phases hand to clap.
pub fn version_string() -> String {
    format!("{} (Forgejo API {})", env!("CARGO_PKG_VERSION"), spec_version())
}

#[cfg(test)]
mod tests {
    /// Bug this prevents: `--version` reporting an API version that is not the one the client
    /// was generated against. The value now comes from the generator rather than from a parse
    /// of `spec/lock.toml`, so the failure mode is a stale regenerate, not a stale parser.
    #[test]
    fn the_spec_version_comes_from_the_generated_client() {
        let v = super::spec_version();
        assert!(v.chars().next().is_some_and(|c| c.is_ascii_digit()), "{v}");
        assert!(
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/lock.toml"))
                .is_ok_and(|lock| lock.contains(&format!("version = \"{v}\""))),
            "the generated SPEC_VERSION ({v}) disagrees with spec/lock.toml; re-run \
             `cargo xtask codegen`"
        );
    }
}

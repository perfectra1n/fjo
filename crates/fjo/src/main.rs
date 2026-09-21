//! `fjo` — a Forgejo command-line client.
//!
//! # The two-phase parse
//!
//! Layer 2 is all 506 API operations as clap commands. Building that tree eagerly would cost
//! thousands of `Arg` allocations on **every** invocation, `fjo --version` included: five to
//! twenty milliseconds spent constructing a parser the user will never reach, in a tool whose
//! whole pre-network budget is under twenty-five. So `main` looks at `argv` first:
//!
//! * [`fjo_raw::peek`] compares a few `OsStr`s. If the first non-flag word is not `raw` (or its
//!   hidden alias `x`), it returns `None` in nanoseconds, having allocated nothing.
//! * When it does match, only the named subtree is built — twelve group stubs for
//!   `fjo raw --help`, one full command for `fjo raw repo create-pull-request`.
//! * Everything else goes through the small derive tree.
//!
//! Both roots carry the same global flags, from [`fjo::global`]. Layer 2's root is built here
//! rather than by [`fjo_raw::build`] for exactly that reason: the globals belong to the binary,
//! and hanging the `raw` subtree off our own root is how they reach it.

// This crate is `publish = false`, so its rustdoc exists for contributors, who read it with
// `--document-private-items`. Module docs here deliberately link to the private helpers they
// describe — that is the useful thing to link to when explaining how a module works — and those
// links resolve under that flag. Suppressing the lint keeps the links navigable rather than
// demoting sixteen of them to inert code spans. The published crates (forgejo-core, -model,
// -client) do NOT carry this allow: docs.rs renders no private items, so there a link to one is
// genuinely broken for the only audience that sees it.
#![allow(rustdoc::private_intra_doc_links)]

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use fjo::exit::{self, Fail};
use fjo::global::{self, GlobalOpts};
use fjo::{ABOUT, LONG_ABOUT};
use forgejo_client::meta::OPS;
use forgejo_client::meta_types::lookup;

fn main() -> ExitCode {
    reset_sigpipe();
    let argv: Vec<OsString> = std::env::args_os().collect();
    exit::report(dispatch(&argv))
}

/// Restore `SIGPIPE` to its default disposition.
///
/// Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so a write to a closed pipe returns
/// `EPIPE` instead of killing the process — and the standard library's `println!` then panics
/// with "failed printing to stdout". `fjo pr list | head -1` is a completely ordinary thing to
/// type, and without this it panics, prints a backtrace hint, and exits 101.
///
/// Restoring the default is what every other Unix CLI does; `crate::exit` additionally treats
/// `BrokenPipe` as success for the writes that do return an error before the signal lands.
#[cfg(unix)]
fn reset_sigpipe() {
    // The one `unsafe` in the binary. `signal(2)` on a constant signal number with `SIG_DFL` has
    // no preconditions to violate; it is `unsafe` only because it is `extern "C"`. `lib.rs`
    // carries `forbid(unsafe_code)` and is a separate crate root, so nothing else can follow
    // this precedent by accident.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

#[derive(Parser)]
#[command(name = "fjo", about = ABOUT, long_about = LONG_ABOUT, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Call any Forgejo REST endpoint by path
    #[command(long_about = API_LONG_ABOUT)]
    Api(fjo::api::ApiArgs),

    /// Call a route under the instance's web root, with a signed-in session
    ///
    /// Layer 0: for the parts of Forgejo that have no REST API at all. Listed after `api`
    /// because it is the rarer answer -- anything reachable under `/api/v1` should go there.
    Web(fjo::web::WebArgs),

    /// Call any Forgejo API operation directly
    ///
    /// Present so that `fjo --help` lists it. Real `fjo raw …` invocations never reach here:
    /// `peek` diverts them to the layer-2 root before this tree is built. The variant still
    /// forwards rather than panicking, so a shape `peek` has not thought of degrades to a
    /// slightly slower correct answer instead of a crash.
    #[command(disable_help_flag = true)]
    Raw {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<OsString>,
    },

    /// The layer-3 groups, flattened in so that `fjo pr` sits beside `fjo api` rather than
    /// under a wrapper noun.
    #[command(flatten)]
    Porcelain(fjo::cmd::Porcelain),
}

const API_LONG_ABOUT: &str = "\
Call any Forgejo REST endpoint by path.

The endpoint is a path relative to /api/v1; a leading slash is optional and an /api/v1 prefix is
accepted and not doubled. {owner}, {repo}, and {branch} are filled in from the resolved
repository.

-f/--raw-field sends strings. -F/--field sends
JSON types and reads a file when the value starts with @ (@- is stdin).

  fjo api version
  fjo api user --jq .login
  fjo api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
  fjo api -X POST -f title=hi -F draft=true 'repos/{owner}/{repo}/issues'
  fjo api -i repos/o/r";

fn dispatch(argv: &[OsString]) -> Result<(), Fail> {
    // User aliases are expanded **before** either parse phase, because `fjo prs` is not a
    // subcommand either root knows about — the alias table is only readable once `config.toml`
    // has been. Returns `argv` unchanged, and reads nothing, when the first command word is not an
    // alias. See `fjo::cmd::alias`.
    let expanded = fjo::cmd::alias::expand(argv).map_err(Fail::from)?;
    let argv: &[OsString] = &expanded;

    // Nanoseconds, no allocation, and no layer-2 `Command` for anything that is not layer 2.
    if let Some(peeked) = fjo_raw::peek(argv) {
        return layer2(argv, peeked.group, peeked.leaf);
    }

    let root = global::augment(
        Cli::command().version(fjo::version_string()),
        &std::collections::BTreeSet::new(),
    )
    .arg(remote_nudge_arg());
    let matches = match root.try_get_matches_from(argv) {
        Ok(m) => m,
        // `fjo pr list --remote origin`: `--remote` is not an argument of *that* command, so
        // clap refuses it before the root's hidden copy is ever consulted. See `remote_nudge`.
        Err(e) if is_unknown_remote(&e) => return Err(Fail::from(remote_nudge(None))),
        Err(e) => return Err(Fail::from(e)),
    };
    // `fjo --remote origin pr list`: parsed, and never run. Checked before the command is
    // dispatched, because the answer is the same whatever the command was.
    if let Ok(Some(name)) = matches.try_get_one::<String>(REMOTE_NUDGE) {
        return Err(Fail::from(remote_nudge(Some(name.as_str()))));
    }
    let cli = Cli::from_arg_matches(&matches).map_err(Fail::from)?;

    match cli.command {
        Cmd::Api(args) => {
            let sub = matches.subcommand_matches("api").expect("clap matched `api`");
            let globals = GlobalOpts::from_chain(&[sub, &matches]);
            fjo::api::run(&globals, &args, sub).map_err(Fail::from)
        }
        Cmd::Web(args) => {
            // No `expect`: clap having matched `web` to reach this arm is true but not worth a
            // panicking construct, and the globals-only chain is a correct fallback rather
            // than a degraded one -- it simply reads them from the root matches.
            match matches.subcommand_matches("web") {
                Some(sub) => {
                    let globals = GlobalOpts::from_chain(&[sub, &matches]);
                    fjo::web::run(&globals, &args, sub).map_err(Fail::from)
                }
                None => {
                    let globals = GlobalOpts::from_chain(&[&matches]);
                    fjo::web::run(&globals, &args, &matches).map_err(Fail::from)
                }
            }
        }
        Cmd::Porcelain(p) => {
            // Globals may appear before or after the group, so read them off the whole chain.
            let globals = match matches.subcommand() {
                Some((_, sub)) => GlobalOpts::from_chain(&[sub, &matches]),
                None => GlobalOpts::from_chain(&[&matches]),
            };
            p.run(&globals).map_err(Fail::from)
        }

        // `peek` should have caught this; forward rather than assume it cannot happen.
        Cmd::Raw { .. } => {
            let peeked = fjo_raw::peek(argv).unwrap_or(fjo_raw::Peeked { group: None, leaf: None });
            layer2(argv, peeked.group, peeked.leaf)
        }
    }
}

// --------------------------------------------------------------------------- the --remote nudge

/// Id of the hidden root-level `--remote`. Prefixed like [`fjo::global`]'s so it cannot collide
/// with a generated layer-2 id.
const REMOTE_NUDGE: &str = "fjo:remote-nudge";

/// The hidden `--remote`, **deliberately not `.global(true)`**.
///
/// # Why it cannot be a global
///
/// `tea`'s `-R` is `--remote` and takes a git remote *name*; `fjo`'s `-R` is `--repo` and takes
/// `owner/name`. Those are different enough that a `tea` user typing `--remote origin` deserves to
/// be told so rather than left with clap's "a similar argument exists: --repo".
///
/// The obvious implementation — put `--remote` in [`fjo::global::args`] alongside `--repo` — is a
/// **crash**. Every global there is `.global(true)`, which propagates it into every subcommand, and
/// clap answers a duplicate long name with a `panic!`, not an error. `--remote` already exists,
/// with its real meaning of a git remote, on three commands:
///
/// ```text
/// fjo pr create --remote <name>     # the remote to push through
/// fjo repo create --remote <name>   # the remote to add to --source  (also -r)
/// fjo repo fork --remote            # a boolean: rewire this checkout
/// ```
///
/// A global copy would crash all three on an ordinary command line — the same failure
/// `crates/fjo/tests/porcelain_cli.rs` was written to catch after it happened three times in one
/// commit. Suppressing the global per-command is not available either: clap's suppression is
/// tree-wide.
///
/// A **non-global** root argument has none of that exposure: it is not propagated, so it is
/// invisible to every subcommand and cannot collide with anything. It catches
/// `fjo --remote origin pr list`, and [`is_unknown_remote`] catches the flag appearing after a
/// command, which is where `tea` users actually put it. Neither path can ever reach a command that
/// has a real `--remote`, because clap accepts it there and never errors.
fn remote_nudge_arg() -> clap::Arg {
    clap::Arg::new(REMOTE_NUDGE)
        .long("remote")
        .value_name("NAME")
        .num_args(0..=1)
        .default_missing_value("")
        .hide(true)
        .help("Use -R owner/name to select a repository")
}

/// What to say to somebody who typed `--remote`.
fn remote_nudge(given: Option<&str>) -> forgejo_core::Error {
    let named = given.filter(|g| !g.is_empty());
    let example = named.map(|g| format!(" (you gave {g:?}, which is a git remote name)"));
    forgejo_core::Error::new(forgejo_core::ErrorKind::Usage(format!(
        "--remote is not supported here{}. -R/--repo takes owner/name, for example \
         `fjo -R myorg/myrepo pr list`. To select a remote for this checkout, run \
         `fjo repo set-default`. --remote is available on pr create, repo create, and repo fork.",
        example.unwrap_or_default()
    )))
}

/// True when clap refused the command line because of a `--remote` the command does not have.
///
/// Narrow on purpose. It fires only where clap already produced an error, so a command that
/// genuinely takes `--remote` never reaches it, and only for that one flag name.
fn is_unknown_remote(e: &clap::Error) -> bool {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if e.kind() != ErrorKind::UnknownArgument {
        return false;
    }
    matches!(
        e.get(ContextKind::InvalidArg),
        Some(ContextValue::String(s))
            if s == "--remote" || s.starts_with("--remote=") || s.starts_with("--remote ")
    )
}

/// Build the layer-2 root and run whatever it parsed.
fn layer2(argv: &[OsString], group: Option<&str>, leaf: Option<&str>) -> Result<(), Fail> {
    // The root is built by `fjo::raw::root` so that the 506-operation smoke test builds exactly
    // what runs here — see the comment there on why a duplicate flag name would be a panic.
    let matches = fjo::raw::root(group, leaf).try_get_matches_from(argv)?;
    let raw_matches = matches
        .subcommand_matches("raw")
        .or_else(|| matches.subcommand_matches("x"))
        .expect("`raw` is the only subcommand on this root");

    // `subcommand()` rather than `subcommand_matches("search")`: the latter *panics* when the
    // name is not a subcommand of the command that was built, and `search` is only present when
    // no group matched. Asking what was matched cannot be wrong.
    let (group_name, group_matches) = raw_matches
        .subcommand()
        .expect("`raw` sets subcommand_required, so clap has already refused a bare invocation");
    if group_name == "search" {
        return search(group_matches);
    }
    let (command_name, leaf_matches) =
        group_matches.subcommand().expect("each group sets subcommand_required");

    let op = lookup::op(OPS, group_name, command_name).ok_or_else(|| {
        // Unreachable in practice: the subtree was built from this very table, so anything clap
        // accepted is in it. A wrong answer here would be a table sorted differently from what
        // `lookup::op`'s binary search assumes, which is worth naming rather than unwrapping.
        forgejo_core::Error::new(forgejo_core::ErrorKind::Usage(format!(
            "`{group_name} {command_name}` parsed but is not in the operation table; \
             this is a bug in fjo — please report it"
        )))
    })?;

    // Globals may appear at any of four depths: `fjo --host x raw repo get`,
    // `fjo raw --host x repo get`, `fjo raw repo --host x get`, `fjo raw repo get --host x`.
    let globals = GlobalOpts::from_chain(&[leaf_matches, group_matches, raw_matches, &matches]);
    fjo::raw::run(&globals, op, leaf_matches).map_err(Fail::from)
}

/// `fjo raw search <words>`: the answer to "506 commands are undiscoverable".
fn search(m: &clap::ArgMatches) -> Result<(), Fail> {
    use std::io::Write;
    let terms: Vec<String> =
        m.get_many::<String>("terms").map(|v| v.cloned().collect()).unwrap_or_default();
    let hits = fjo_raw::search::search(OPS, &terms, fjo_raw::search::DEFAULT_LIMIT);
    let text = fjo_raw::search::render(&hits, &terms);
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes()).map_err(|e| Fail::from(forgejo_core::Error::from(e)))?;
    out.flush().map_err(|e| Fail::from(forgejo_core::Error::from(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> clap::Command {
        global::augment(Cli::command(), &std::collections::BTreeSet::new()).arg(remote_nudge_arg())
    }

    /// The hazard this test exists for. A `--remote` added the obvious way — as one more entry in
    /// `global::args`, which marks everything `.global(true)` — is propagated into every
    /// subcommand, and clap answers a duplicate long name with a **panic**. Five commands already
    /// declare `--remote` with its real meaning, so the naive version crashes all five. This walks
    /// the whole tree through clap's own consistency checks with the nudge attached, which is the
    /// same check that would otherwise fire in a user's terminal.
    #[test]
    fn the_hidden_remote_nudge_does_not_collide_with_the_real_per_command_remote() {
        root().debug_assert();

        // Each of these has a genuine `--remote`; it must still parse, untouched.
        for argv in [
            &["fjo", "pr", "create", "--remote", "upstream", "--fill"][..],
            &["fjo", "repo", "create", "widget", "--remote", "origin"][..],
            &["fjo", "repo", "create", "widget", "-r", "origin"][..],
            &["fjo", "repo", "fork", "--remote"][..],
        ] {
            let m = root().try_get_matches_from(argv);
            assert!(m.is_ok(), "{argv:?} must parse: {:?}", m.err().map(|e| e.to_string()));
        }
    }

    /// `fjo --remote origin pr list` parses — and is then refused with the nudge, before any
    /// command runs.
    #[test]
    fn a_leading_remote_is_answered_with_the_repo_flag_instead_of_silence() {
        let m = root()
            .try_get_matches_from(["fjo", "--remote", "origin", "pr", "list"])
            .expect("the hidden argument accepts it");
        let given = m.try_get_one::<String>(REMOTE_NUDGE).ok().flatten().cloned();
        assert_eq!(given.as_deref(), Some("origin"));

        let e = remote_nudge(given.as_deref());
        assert_eq!(e.exit_code(), 2);
        let text = e.to_string();
        assert!(text.contains("-R/--repo takes owner/name"), "{text}");
        assert!(text.contains("\"origin\""), "the value they typed should be quoted back: {text}");
    }

    /// `fjo pr list --remote origin` — where a tea user actually puts it. The flag is not an
    /// argument of that command, so clap refuses it and the refusal is recognised and replaced.
    #[test]
    fn a_trailing_remote_on_a_command_that_has_none_is_recognised() {
        for argv in [
            &["fjo", "pr", "list", "--remote", "origin"][..],
            &["fjo", "issue", "list", "--remote=origin"][..],
            &["fjo", "repo", "view", "--remote", "upstream"][..],
        ] {
            let e = root().try_get_matches_from(argv).expect_err("clap must refuse it");
            assert!(is_unknown_remote(&e), "{argv:?} was not recognised as a --remote: {e}");
        }

        // ...and nothing else is mistaken for it.
        let e = root()
            .try_get_matches_from(["fjo", "pr", "list", "--remotely"])
            .expect_err("clap must refuse it");
        assert!(!is_unknown_remote(&e), "{e}");
        let e = root()
            .try_get_matches_from(["fjo", "pr", "list", "--nosuchflag"])
            .expect_err("clap must refuse it");
        assert!(!is_unknown_remote(&e), "{e}");
    }

    /// The nudge must never fire for a command that really does take `--remote`: those never
    /// produce an error for clap to reinterpret.
    #[test]
    fn the_nudge_never_fires_where_remote_is_real() {
        let ok = root().try_get_matches_from(["fjo", "pr", "create", "--remote", "upstream"]);
        assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.to_string()));
    }

    /// The hidden aliases are hidden: they must not show up as subcommands of the real tree, or
    /// `fjo --help` would grow five entries `gh` does not have.
    #[test]
    fn the_tea_aliases_are_not_subcommands_of_the_root() {
        let root = root();
        for (name, _) in fjo::cmd::alias::BUILTIN {
            assert!(
                !root.get_subcommands().any(|c| c.get_name() == *name),
                "{name} must stay an argv-level alias, not a clap subcommand"
            );
        }
    }

    /// ...and what they expand to has to parse. This is the end-to-end shape: `expand` rewrites
    /// argv, and the rewritten argv is what the root then sees.
    #[test]
    fn every_tea_alias_expands_to_something_the_root_accepts() {
        let root = root();
        for (name, expansion) in fjo::cmd::alias::BUILTIN {
            let mut argv = vec!["fjo".to_owned()];
            argv.extend(expansion.split_whitespace().map(str::to_owned));
            argv.push("--help".to_owned());
            let e = root.clone().try_get_matches_from(&argv).expect_err("--help is an Err");
            assert!(
                !e.use_stderr(),
                "the built-in alias {name} expands to {expansion:?}, which the root refuses: {e}"
            );
        }
    }
}

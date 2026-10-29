//! `fjo pr` — pull requests, including AGit.
//!
//! The group people will type most, so its shape is worth stating plainly.
//!
//! **Every verb takes the same optional selector**, and every verb infers it from the branch you are
//! on when you leave it out. `fjo pr merge`, `fjo pr diff`, `fjo pr checks` with no arguments, in a
//! checkout, are the commands this whole layer exists for. The selector accepts a number, a URL, or a
//! branch name; see [`common`], which owns the rule and its tests. `reply`, `resolve` and
//! `unresolve` keep the rule too: the comment they act on is a flag. See [`threads`].
//!
//! **AGit is here.** `fjo pr create --agit` opens a pull request with no branch and no fork, by
//! pushing to `refs/for/<base>/<topic>`. That is a Forgejo capability with no GitHub equivalent — not
//! a feature `gh` has not got round to, but one there is no API call for — and [`create`] documents
//! it at length because most users will never have met it.
//!
//! **Two Forgejo shapes that surprise people**, both documented where they bite:
//!
//! * A **draft** is a `WIP:` title prefix, not a field. `--draft` and `fjo pr ready` are title
//!   edits. See [`common`].
//! * A **merge style** is one enum value, not three booleans, and it has `fast-forward-only`, which
//!   `gh` lacks. `--merge`/`--rebase`/`--squash` are kept as aliases. See [`merge`].
//!
//! # Flag names that differ from `gh`
//!
//! The flags in [`crate::global`] are clap *globals*: propagated into every subcommand, and a
//! duplicate long or short name makes clap panic. So `-t/--title` is `--title` (the global
//! `--template` owns `-t`), `pr diff` uses the global `--color`, `pr checkout --force` is `-f`, and
//! `-L` is declared per command while `--limit` stays global. Each one says so in its own help.

pub mod checkout;
pub mod checks;
pub mod common;
pub mod create;
pub mod diff;
pub mod files;
pub mod list;
pub mod merge;
pub mod review;
pub mod state;
pub mod threads;
pub mod view;

use clap::{Args as ClapArgs, Subcommand};
use forgejo_core::Result;

use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Sub,
}

#[derive(Debug, Subcommand)]
pub enum Sub {
    /// Create a pull request, with a branch or over AGit
    Create(create::Args),
    /// List pull requests
    List(list::Args),
    /// Show one pull request
    View(view::Args),
    /// Check out a pull request locally, fork or AGit included
    Checkout(checkout::Args),
    /// Show a pull request's diff
    Diff(diff::Args),
    /// Merge a pull request
    Merge(merge::Args),
    /// Close a pull request without merging
    Close(state::CloseArgs),
    /// Reopen a closed pull request
    Reopen(state::ReopenArgs),
    /// Take a pull request out of draft
    Ready(state::ReadyArgs),
    /// Approve, comment on, or request changes to a pull request
    Review(review::Args),
    /// Comment on a pull request
    Comment(state::CommentArgs),
    /// Change a pull request's title, body, base or metadata
    Edit(state::EditArgs),
    /// State, mergeability and checks, at a glance
    Status(checks::StatusArgs),
    /// CI checks (exit code 8 while pending)
    Checks(checks::Args),
    /// Files a pull request changes
    Files(files::Args),
    /// Inline review threads, and whether each is resolved
    Threads(threads::ThreadsArgs),
    /// Reply to an inline review comment
    Reply(threads::ReplyArgs),
    /// Resolve inline review threads
    Resolve(threads::ResolveArgs),
    /// Reopen resolved inline review threads
    Unresolve(threads::UnresolveArgs),
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.cmd {
        Sub::Create(a) => create::run(globals, a),
        Sub::List(a) => list::run(globals, a),
        Sub::View(a) => view::run(globals, a),
        Sub::Checkout(a) => checkout::run(globals, a),
        Sub::Diff(a) => diff::run(globals, a),
        Sub::Merge(a) => merge::run(globals, a),
        Sub::Close(a) => state::run_close(globals, a),
        Sub::Reopen(a) => state::run_reopen(globals, a),
        Sub::Ready(a) => state::run_ready(globals, a),
        Sub::Review(a) => review::run(globals, a),
        Sub::Comment(a) => state::run_comment(globals, a),
        Sub::Edit(a) => state::run_edit(globals, a),
        Sub::Status(a) => checks::run_status(globals, a),
        Sub::Checks(a) => checks::run(globals, a),
        Sub::Files(a) => files::run(globals, a),
        Sub::Threads(a) => threads::run_threads(globals, a),
        Sub::Reply(a) => threads::run_reply(globals, a),
        Sub::Resolve(a) => threads::run_resolve(globals, a),
        Sub::Unresolve(a) => threads::run_unresolve(globals, a),
    }
}

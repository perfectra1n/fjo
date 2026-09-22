//! `fjo auth` — get a token onto this machine, and prove it works.
//!
//! This group is the tool's front door. Every authentication diagnostic in `forgejo-core` ends
//! with "run `fjo auth login`", so nothing else in `fjo` is reachable for a new user until this
//! works.
//!
//! Three decisions here are worth stating up front, because each of them is a bug someone
//! actually ships:
//!
//! * **The login is discovered, never accepted.** `login` calls `GET /user` and files the token
//!   under the account the *server* says it belongs to. Storing it under the name the user typed
//!   is how a token ends up recorded against the wrong account, after which every later error
//!   message points at the wrong identity.
//! * **A missing keyring is a warning, not a failure.** Headless servers, containers, CI and
//!   plain SSH sessions have no D-Bus Secret Service, and those are where a CLI lives. `login`
//!   falls back to `hosts.toml` at mode 0600, says so, and succeeds.
//! * **There is deliberately no `--show-token`.** `auth status` never prints a token, however
//!   convenient that would occasionally be, because status output is the thing people paste into
//!   issues and screenshots. `auth token` exists for scripting and warns when it is about to
//!   write a secret into a terminal's scrollback.

pub mod callback;
pub mod common;
mod git_credential;
mod login;
mod logout;
mod setup_git;
mod status;
mod switch;
mod token;
mod web_login;
mod web_password;
mod web_transfer;

use clap::{Args as ClapArgs, Subcommand};
use forgejo_core::Result;

use crate::global::GlobalOpts;

/// `after_long_help` rather than `long_about`, throughout this group and the rest of the setup
/// surface.
///
/// Not a style choice: clap's derive gives an enum *variant*'s doc comment precedence over the
/// payload struct's `long_about`, and every layer-3 variant in [`super::Porcelain`] carries one. A
/// `long_about` set here is therefore silently discarded — the kind of thing that looks fine in the
/// source and is missing from `--help`. `after_long_help` survives, appears only under `--help` (so
/// `-h` stays terse), and clap advertises it with "see more with '--help'".
#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

const LONG_HELP: &str = "\
Authenticate with a Forgejo server.

Token scopes use read:<area> and write:<area>, such as read:repository or write:issue.
Scopes are fixed when a token is created; create a new token to change them.

`fjo auth status` never prints tokens. Use `fjo auth token` to retrieve one.

--web logs in through the browser instead. OAuth sessions renew themselves and
lapse after about 30 days; scopes do not apply to them, because Forgejo does not
enforce scopes on an OAuth token. For CI, prefer a token, which does not expire.

  fjo auth login --host codeberg.org             # prompts for a token
  fjo auth login --host codeberg.org --web       # logs in through your browser
  fjo auth login --host git.example.org --with-token < token.txt
  fjo auth status                                # per host: who you are, and whether it works
  fjo auth switch --host git.example.org         # change the active host or login
  fjo auth setup-git --host git.example.org      # let git push and pull with your fjo token";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Store a token for a Forgejo instance
    Login(login::Args),
    /// Remove a stored credential
    Logout(logout::Args),
    /// Who you are on each host, and whether the token still works
    Status(status::Args),
    /// Change the active host or login
    Switch(switch::Args),
    /// Print the stored token, for scripting
    Token(token::Args),
    /// Print a web session, to reuse it elsewhere (e.g. in CI)
    Export(web_transfer::ExportArgs),
    /// Store a web session exported from another machine
    Import(web_transfer::ImportArgs),
    /// Configure git to authenticate with your fjo token
    SetupGit(setup_git::Args),
    /// git's credential-helper protocol, for `auth setup-git`
    ///
    /// Hidden because it is not for humans: `git` runs it, one line of key=value per stdin
    /// line. It is a subcommand rather than a separate binary so that `setup-git` can point
    /// git at whatever `fjo` the user actually invoked.
    #[command(hide = true)]
    GitCredential(git_credential::Args),
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.command {
        Cmd::Login(a) => login::run(globals, a),
        Cmd::Logout(a) => logout::run(globals, a),
        Cmd::Status(a) => status::run(globals, a),
        Cmd::Switch(a) => switch::run(globals, a),
        Cmd::Token(a) => token::run(globals, a),
        Cmd::Export(a) => web_transfer::export(globals, a),
        Cmd::Import(a) => web_transfer::import(globals, a),
        Cmd::SetupGit(a) => setup_git::run(globals, a),
        Cmd::GitCredential(a) => git_credential::run(globals, a),
    }
}

//! `gea admin` — instance administration.
//!
//! This group has **no equivalent in `gh` at all**, and `tea` covers only `tea admin users`. It is
//! the largest single capability gap between `gea` and both of them, so it is written for the
//! person most likely to be reading it: an operator with a broken instance and a terminal.
//!
//! Three rules follow from that reader:
//!
//! 1. **Every help string says what the command does *to the instance*,** not which endpoint it
//!    calls. "Create an account and, optionally, make it a site administrator" is useful at 03:00;
//!    "POST /admin/users" is not.
//! 2. **Nothing destructive happens without a confirmation** on a terminal, or `--yes` off one.
//!    `admin adopt delete` erases a git directory that Gitea does not have a record of, and
//!    there is nothing to undo it with.
//! 3. **A 403 names `write:admin`.** Gitea refuses a non-admin token on `/admin/…` with
//!    `user must be site admin`, which mentions neither a scope nor a token, so the generic
//!    classifier reports it as a plain `Forbidden`. Every command here routes its errors through
//!    [`scope::admin_scope`] so the message says the one thing that fixes it: **a Gitea token's
//!    scopes are fixed at creation, so this needs a new token.**
//!
//! # Everything here works outside a checkout
//!
//! No command in this group touches [`Runtime::repo`], so `gea admin user create` works from
//! `/tmp`. That is not incidental — an operator is usually not standing in a clone of the
//! repository they are fixing.
//!
//! # Not implemented yet
//!
//! User **badges** have three routes in Gitea's API and no porcelain here yet. They are reachable
//! as `gea raw admin list-user-badges`, `add-user-badges` and `delete-user-badges`.

pub mod adopt;
pub mod cron;
pub mod email;
pub mod org;
pub mod repo;
pub mod runner;
pub mod scope;
pub mod user;

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_core::error::Result;

use crate::cmd::support::{Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Accounts on this instance: create, inspect, change, remove
    #[command(subcommand)]
    User(user::Cmd),

    /// Organizations on this instance
    #[command(subcommand)]
    Org(org::Cmd),

    /// Repositories on this instance, including private ones
    #[command(subcommand)]
    Repo(repo::Cmd),

    /// The instance's scheduled maintenance tasks, and how to run one now
    #[command(subcommand)]
    Cron(cron::Cmd),

    /// Git directories on disk that this instance has no record of
    #[command(subcommand)]
    Adopt(adopt::Cmd),

    /// Actions runners registered anywhere on this instance
    #[command(subcommand)]
    Runner(runner::Cmd),

    /// Find an account by email address
    #[command(subcommand)]
    Email(email::Cmd),
}

impl Cmd {
    /// The operation whose response shape this command emits, for `--json` field discovery.
    /// Empty means "there is nothing to select".
    fn op(&self) -> &'static str {
        match self {
            Self::User(c) => user::op(c),
            Self::Org(c) => org::op(c),
            Self::Repo(c) => repo::op(c),
            Self::Cron(c) => cron::op(c),
            Self::Adopt(c) => adopt::op(c),
            Self::Runner(c) => runner::op(c),
            Self::Email(c) => email::op(c),
        }
    }

    /// The token scope a 403 should name. Reads need `read:admin`; anything that changes the
    /// instance needs `write:admin`, and telling a user to create the read scope when they need
    /// the write one costs them a second token.
    fn scope(&self) -> &'static str {
        let writes = match self {
            Self::User(c) => user::writes(c),
            Self::Org(c) => org::writes(c),
            Self::Repo(c) => repo::writes(c),
            Self::Cron(c) => cron::writes(c),
            Self::Adopt(c) => adopt::writes(c),
            Self::Runner(c) => runner::writes(c),
            Self::Email(_) => false,
        };
        if writes { "write:admin" } else { "read:admin" }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let op = args.command.op();
    let fields = if op.is_empty() {
        None
    } else {
        match Json::resolve(globals, op)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        }
    };

    crate::runtime::block_on(async move {
        // Deliberately never `rt.repo(globals)`: administering an instance is not something you
        // do from inside one of its clones.
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let host = rt.client().host().to_owned();
        let settings = rt.client().settings_url();
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        let outcome = match &args.command {
            Cmd::User(c) => user::run(&api, globals, &mut emit, c).await,
            Cmd::Org(c) => org::run(&api, globals, &mut emit, c).await,
            Cmd::Repo(c) => repo::run(&api, globals, &mut emit, c).await,
            Cmd::Cron(c) => cron::run(&api, globals, &mut emit, c).await,
            Cmd::Adopt(c) => adopt::run(&api, globals, &mut emit, c).await,
            Cmd::Runner(c) => runner::run(&api, globals, &mut emit, c).await,
            Cmd::Email(c) => email::run(&api, globals, &mut emit, c).await,
        };
        outcome.map_err(|e| scope::admin_scope(args.command.scope(), &host, &settings, e))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: a new subgroup shipping without a `--json` field table or with the
    /// wrong scope, both of which are invisible until someone hits them in anger. Walking the
    /// declared operations keeps `op()` honest as the group grows.
    #[test]
    fn every_declared_operation_has_a_field_table() {
        for op in [
            user::OP_USER,
            org::OP_ORG,
            repo::OP_REPO,
            cron::OP_CRON,
            runner::OP_RUNNER,
            email::OP_EMAIL,
        ] {
            assert!(
                !crate::cmd::support::fields::for_op(op).is_empty(),
                "{op} has no --json field table"
            );
        }
    }

    /// A read must not tell the operator to mint a `write:admin` token, and a write must not tell
    /// them `read:admin` will do — the second one costs them a second round trip through the
    /// token page, because Gitea scopes cannot be widened after creation.
    #[test]
    fn reads_ask_for_read_admin_and_writes_for_write_admin() {
        let list = Cmd::Cron(cron::Cmd::List);
        assert_eq!(list.scope(), "read:admin");
        let run = Cmd::Cron(cron::Cmd::Run(cron::Run { task: "update_mirrors".into(), yes: true }));
        assert_eq!(run.scope(), "write:admin");
    }
}

//! Layer 3: the hand-written, `gh`-shaped commands.
//!
//! Layer 2 (`fjo raw`) already reaches all 506 operations, so a command belongs here only if
//! it is *nicer* than layer 2 — by inferring the repository, orchestrating several calls,
//! prompting when something is missing, or rendering better than pretty-printed JSON. See
//! `docs/porcelain-conventions.md`, which is binding.
//!
//! Each group owns one module and follows the same shape as [`crate::api`] and [`crate::raw`]:
//! a `run` that is synchronous on the outside and enters `runtime::block_on` internally, so no
//! caller has to care that the client is async.
//!
//! Plumbing that is genuinely common to every group — field tables, the
//! `--json`/`--jq`/`--template` triad, pagination caps, prompting, the stderr note — lives in
//! [`support`]. It exists because six groups each grew a private copy while this file was
//! frozen; see that module's docs. A helper that needs an `Issue`, a `PullRequest` or a `Label`
//! is domain code and stays with its group.
//!
//! Groups whose module still holds a stub answer with a "not implemented yet" that names the
//! `fjo raw` command doing the same job. That is deliberate: every endpoint already works, so
//! an unimplemented porcelain command is a missing convenience, never a missing capability.

pub mod admin;
pub mod alias;
pub mod auth;
pub mod block;
pub mod browse;
pub mod completion;
pub mod config;
pub mod deploy_key;
pub mod git_hook;
pub mod issue;
pub mod label;
pub mod milestone;
pub mod mirror;
pub mod nodeinfo;
pub mod notification;
pub mod org;
pub mod package;
pub mod pr;
pub mod project;
pub mod quota;
pub mod reaction;
pub mod release;
pub mod repo;
pub mod run;
pub mod search;
pub mod secret;
pub mod status;
pub mod stopwatch;
pub mod support;
pub mod team;
pub mod times;
pub mod topic;
pub mod transfer;
pub mod user;
pub mod variable;
pub mod webhook;
pub mod wiki;
pub mod workflow;

use clap::Subcommand;
use forgejo_core::Result;

use crate::global::GlobalOpts;

/// Every layer-3 group.
#[derive(Debug, Subcommand)]
pub enum Porcelain {
    /// Authenticate with a Forgejo instance
    #[command(name = "auth")]
    Auth(auth::Args),
    /// Read and write fjo's configuration
    #[command(name = "config")]
    Config(config::Args),
    /// Shortcuts for commands you run often
    #[command(name = "alias")]
    Alias(alias::Args),
    /// Generate a shell completion script
    #[command(name = "completion")]
    Completion(completion::Args),
    /// What needs your attention across your repositories
    #[command(name = "status")]
    Status(status::Args),
    /// Repositories: create, clone, fork, view, transfer
    #[command(name = "repo")]
    Repo(repo::Args),
    /// Pull requests, including AGit
    #[command(name = "pr")]
    Pr(pr::Args),
    /// Issues
    #[command(name = "issue")]
    Issue(issue::Args),
    /// Labels
    #[command(name = "label")]
    Label(label::Args),
    /// Milestones
    #[command(name = "milestone")]
    Milestone(milestone::Args),
    /// Releases and their assets
    #[command(name = "release")]
    Release(release::Args),
    /// Forgejo Actions runs
    #[command(name = "run")]
    Run(run::Args),
    /// Forgejo Actions workflows
    #[command(name = "workflow")]
    Workflow(workflow::Args),
    /// Actions secrets
    #[command(name = "secret")]
    Secret(secret::Args),
    /// Actions variables
    #[command(name = "variable")]
    Variable(variable::Args),
    /// Organizations
    #[command(name = "org")]
    Org(org::Args),
    /// Teams
    #[command(name = "team")]
    Team(team::Args),
    /// Users, keys and tokens
    #[command(name = "user")]
    User(user::Args),
    /// Notification threads
    #[command(name = "notification")]
    Notification(notification::Args),
    /// Search repositories, issues and users
    #[command(name = "search")]
    Search(search::Args),
    /// Open a repository or resource in a browser
    #[command(name = "browse")]
    Browse(browse::Args),
    /// Tracked time on issues
    #[command(name = "times")]
    Times(times::Args),
    /// A running timer on an issue
    #[command(name = "stopwatch")]
    Stopwatch(stopwatch::Args),
    /// Wiki pages and revisions
    #[command(name = "wiki")]
    Wiki(wiki::Args),
    /// Push and pull mirrors
    #[command(name = "mirror")]
    Mirror(mirror::Args),
    /// Storage quotas
    #[command(name = "quota")]
    Quota(quota::Args),
    /// The package registry
    #[command(name = "package")]
    Package(package::Args),
    /// What kind of instance this is
    #[command(name = "nodeinfo")]
    Nodeinfo(nodeinfo::Args),
    /// Webhooks at repository, org, user and global scope
    #[command(name = "webhook")]
    Webhook(webhook::Args),
    /// Deploy keys
    #[command(name = "deploy-key")]
    DeployKey(deploy_key::Args),
    /// Server-side Git hooks
    #[command(name = "git-hook")]
    GitHook(git_hook::Args),
    /// Repository topics
    #[command(name = "topic")]
    Topic(topic::Args),
    /// Reactions on issues and comments
    #[command(name = "reaction")]
    Reaction(reaction::Args),
    /// Blocking users
    #[command(name = "block")]
    Block(block::Args),
    /// Repository transfer offers
    #[command(name = "transfer")]
    Transfer(transfer::Args),
    /// Instance administration
    #[command(name = "admin")]
    Admin(admin::Args),
}

impl Porcelain {
    pub fn run(&self, globals: &GlobalOpts) -> Result<()> {
        match self {
            Self::Auth(a) => auth::run(globals, a),
            Self::Config(a) => config::run(globals, a),
            Self::Alias(a) => alias::run(globals, a),
            Self::Completion(a) => completion::run(globals, a),
            Self::Status(a) => status::run(globals, a),
            Self::Repo(a) => repo::run(globals, a),
            Self::Pr(a) => pr::run(globals, a),
            Self::Issue(a) => issue::run(globals, a),
            Self::Label(a) => label::run(globals, a),
            Self::Milestone(a) => milestone::run(globals, a),
            Self::Release(a) => release::run(globals, a),
            Self::Run(a) => run::run(globals, a),
            Self::Workflow(a) => workflow::run(globals, a),
            Self::Secret(a) => secret::run(globals, a),
            Self::Variable(a) => variable::run(globals, a),
            Self::Org(a) => org::run(globals, a),
            Self::Team(a) => team::run(globals, a),
            Self::User(a) => user::run(globals, a),
            Self::Notification(a) => notification::run(globals, a),
            Self::Search(a) => search::run(globals, a),
            Self::Browse(a) => browse::run(globals, a),
            Self::Times(a) => times::run(globals, a),
            Self::Stopwatch(a) => stopwatch::run(globals, a),
            Self::Wiki(a) => wiki::run(globals, a),
            Self::Mirror(a) => mirror::run(globals, a),
            Self::Quota(a) => quota::run(globals, a),
            Self::Package(a) => package::run(globals, a),
            Self::Nodeinfo(a) => nodeinfo::run(globals, a),
            Self::Webhook(a) => webhook::run(globals, a),
            Self::DeployKey(a) => deploy_key::run(globals, a),
            Self::GitHook(a) => git_hook::run(globals, a),
            Self::Topic(a) => topic::run(globals, a),
            Self::Reaction(a) => reaction::run(globals, a),
            Self::Block(a) => block::run(globals, a),
            Self::Transfer(a) => transfer::run(globals, a),
            Self::Admin(a) => admin::run(globals, a),
        }
    }
}

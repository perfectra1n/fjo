//! `gea issue` — issues.
//!
//! Every subcommand here earns its place under `docs/porcelain-conventions.md`: the repository
//! is inferred, `@me` is resolved, labels and milestones are named rather than numbered, and
//! `create`/`edit` orchestrate several calls. What layer 2 cannot do is the point — `gea raw
//! issue create-issue` needs a milestone *id* and label *ids*, which nobody has memorised.
//!
//! # What Gitea's API cannot do, and so neither can this
//!
//! `gh issue` has four verbs with no Gitea endpoint behind them at Forgejo API version 16.0.4 (as measured for fjo):
//! `lock`/`unlock` (`Issue.is_locked` is reported but not writable), `transfer`, and `develop`.
//! They are deliberately absent rather than present-and-failing: a subcommand that exists and
//! always errors is worse than one that was never offered, because it survives into scripts.
//! The group's `--help` says so, so the absence is discoverable without reading this file.
//!
//! # What Gitea can do that GitHub cannot
//!
//! Issue **dependencies** and **pinning with a position**, both exposed here:
//!
//! ```text
//! gea issue depends list 42            # what blocks #42, and what #42 blocks
//! gea issue depends add 42 --blocked-by 7
//! gea issue pin 42 --position 1
//! ```
//!
//! The two directions are separate endpoints and they are easy to confuse:
//! `/issues/42/dependencies` is *what blocks 42*, `/issues/42/blocks` is *what 42 blocks*.
//! `--blocked-by` and `--blocks` name the direction the way a human says it out loud.

pub mod ops;
pub mod shared;

use clap::{Args as ClapArgs, Subcommand};
use gitea_core::Result;
use gitea_core::types::ids::{CommentId, IssueIndex};

use crate::global::GlobalOpts;

use shared::State;

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Manage issues, including dependencies and pinned issues.

The API does not support locking issues, moving them between repositories, or
creating branches from issues.

  gea issue list -s all -l bug
  gea issue create --title 'It broke' -b 'here is how' -a @me -m 1.0
  gea issue view 42 --comments
  gea issue edit 42 --add-label bug --remove-assignee @me
  gea issue depends add 42 --blocked-by 7";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create an issue
    Create(CreateArgs),
    /// List issues in a repository
    List(ListArgs),
    /// Show one issue
    View(ViewArgs),
    /// Close an issue
    Close(CloseArgs),
    /// Reopen a closed issue
    Reopen(TargetArgs),
    /// Add a comment to an issue
    Comment(CommentArgs),
    /// Change an issue's title, body, labels, assignees or milestone
    Edit(EditArgs),
    /// Delete an issue permanently
    Delete(DeleteArgs),
    /// Pin an issue, optionally at a position
    Pin(PinArgs),
    /// Unpin an issue
    Unpin(TargetArgs),
    /// Issue dependencies
    #[command(subcommand)]
    Depends(DependsCmd),
}

/// Just an issue number, for the verbs that need nothing else.
#[derive(Debug, ClapArgs)]
pub struct TargetArgs {
    /// Issue number, as you would write it: 42 or '#42'
    #[arg(value_name = "NUMBER", value_parser = clap::value_parser!(IssueIndex))]
    pub number: IssueIndex,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Issue title. Prompted for when omitted and both streams are terminals
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Issue body
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the body from a file; '-' reads stdin
    #[arg(short = 'F', long = "body-file", value_name = "FILE")]
    pub body_file: Option<String>,

    /// Compose in $EDITOR; the first line is the title
    #[arg(short = 'e', long)]
    pub editor: bool,

    /// Open the browser's new-issue form instead of creating one
    #[arg(short = 'w', long)]
    pub web: bool,

    /// Assign a user; '@me' is you. Repeatable
    #[arg(short = 'a', long, value_name = "USER")]
    pub assignee: Vec<String>,

    /// Add a label by name. Repeatable
    #[arg(short = 'l', long, value_name = "NAME")]
    pub label: Vec<String>,

    /// Milestone, by title
    #[arg(short = 'm', long, value_name = "TITLE")]
    pub milestone: Option<String>,

    /// Start from a repository issue template, by name or file name
    #[arg(short = 'T', value_name = "NAME")]
    pub template: Option<String>,

    /// Restore a draft written by an earlier failed run
    #[arg(long, value_name = "FILE")]
    pub recover: Option<std::path::PathBuf>,

    /// Print what would be created and send nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Which issues to show
    #[arg(short = 's', long, value_enum, default_value_t = State::Open)]
    pub state: State,

    /// Only issues assigned to this user; '@me' is you
    #[arg(short = 'a', long, value_name = "USER")]
    pub assignee: Option<String>,

    /// Only issues opened by this user; '@me' is you
    #[arg(short = 'A', long, value_name = "USER")]
    pub author: Option<String>,

    /// Only issues with this label. Repeatable
    #[arg(short = 'l', long, value_name = "NAME")]
    pub label: Vec<String>,

    /// Only issues in this milestone, by title
    #[arg(short = 'm', long, value_name = "TITLE")]
    pub milestone: Option<String>,

    /// Maximum number of issues (also settable as --limit)
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Search issue titles and bodies
    #[arg(short = 'S', long, value_name = "QUERY")]
    pub search: Option<String>,

    /// Open the issue list in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Also show the issue's comments
    #[arg(short = 'c', long)]
    pub comments: bool,

    /// Open the issue in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct CloseArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Leave a comment before closing
    #[arg(short = 'c', long, value_name = "TEXT")]
    pub comment: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct CommentArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Comment body
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the comment from a file; '-' reads stdin
    #[arg(short = 'F', long = "body-file", value_name = "FILE")]
    pub body_file: Option<String>,

    /// Compose the comment in $EDITOR
    #[arg(short = 'e', long)]
    pub editor: bool,

    /// Edit an existing comment instead of adding one. This is a comment id — a database id,
    /// not an issue number — as printed by `gea issue view --comments`
    #[arg(long, value_name = "COMMENT-ID", value_parser = clap::value_parser!(CommentId))]
    pub edit: Option<CommentId>,

    /// Open the issue in a browser to comment there
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// New title
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// New body
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the new body from a file; '-' reads stdin
    #[arg(short = 'F', long = "body-file", value_name = "FILE")]
    pub body_file: Option<String>,

    /// Edit title and body in $EDITOR, prefilled with the current ones
    #[arg(short = 'e', long)]
    pub editor: bool,

    /// Add a label. Repeatable
    #[arg(long, value_name = "NAME")]
    pub add_label: Vec<String>,

    /// Remove a label. Repeatable
    #[arg(long, value_name = "NAME")]
    pub remove_label: Vec<String>,

    /// Add an assignee; '@me' is you. Repeatable
    #[arg(long, value_name = "USER")]
    pub add_assignee: Vec<String>,

    /// Remove an assignee; '@me' is you. Repeatable
    #[arg(long, value_name = "USER")]
    pub remove_assignee: Vec<String>,

    /// Set the milestone, by title
    #[arg(short = 'm', long, value_name = "TITLE")]
    pub milestone: Option<String>,

    /// Clear the milestone
    #[arg(long, conflicts_with = "milestone")]
    pub remove_milestone: bool,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct PinArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Move the pin to this position, 1 being first. Gitea-only
    #[arg(long, value_name = "N")]
    pub position: Option<i64>,
}

#[derive(Debug, Subcommand)]
pub enum DependsCmd {
    /// List what blocks an issue and what it blocks
    List(TargetArgs),
    /// Record a dependency
    Add(DependsArgs),
    /// Remove a dependency
    Remove(DependsArgs),
}

#[derive(Debug, ClapArgs)]
#[command(group(clap::ArgGroup::new("direction").required(true).args(["blocked_by", "blocks"])))]
pub struct DependsArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// The other issue must be finished first
    #[arg(long, value_name = "NUMBER", value_parser = clap::value_parser!(IssueIndex))]
    pub blocked_by: Option<IssueIndex>,

    /// This issue must be finished before the other one
    #[arg(long, value_name = "NUMBER", value_parser = clap::value_parser!(IssueIndex))]
    pub blocks: Option<IssueIndex>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    ops::run(globals, args)
}

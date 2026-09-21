//! `fjo project` — Forgejo's kanban boards.
//!
//! These are layer 0 commands: Projects has no REST API, so every one of them speaks to
//! Forgejo's web routes with a session cookie. See `docs/layers.md`.
//!
//! What this adds over `fjo web`, which can reach the same routes: **everything is named by
//! title.** Forgejo's project routes are all `/projects/{id}/{columnID}`, and nobody knows that
//! a board called `Roadmap` is id 3 or that its `Done` column is id 12. The same reasoning put
//! `milestone_by_title` behind `fjo milestone`, and the same corollary applies — an ambiguous
//! title is refused with the matches listed, never guessed, because Forgejo is perfectly happy
//! to hold two boards called `Roadmap`.
//!
//! All markup knowledge lives in [`parse`]. Nothing here names a tag, a class or an attribute.

pub mod ops;
pub mod parse;

use clap::{Args as ClapArgs, Subcommand, ValueEnum};

use forgejo_core::error::{Error, ErrorKind, Result};

pub use parse::{Board, Card, Column, ProjectRef, parse_board, parse_index};

use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;
use ops::Scope;

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Manage project boards (kanban).

Boards are named by title, not by id, and an ambiguous title is refused
rather than guessed. Closed boards are searched too, so closing one does
not make it unreachable.

These are web-only routes: Forgejo has no REST API for projects, so they
need a session rather than a token. Sign in with
`fjo auth login --with-password`.

  fjo project list
  fjo project view Roadmap
  fjo project card move 42 --to Done";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List boards
    List(ListArgs),
    /// Show a board's columns and cards
    View(ViewArgs),
    /// Create a board
    Create(CreateArgs),
    /// Close a board
    Close(NameArgs),
    /// Reopen a closed board
    Reopen(NameArgs),
    /// Delete a board
    Delete(DeleteArgs),
    /// Columns on a board
    #[command(subcommand)]
    Column(ColumnCmd),
    /// Cards (issues) on a board
    #[command(subcommand)]
    Card(CardCmd),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum State {
    Open,
    Closed,
    All,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

/// Which starter columns a new board gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Template {
    /// No columns
    None,
    /// Backlog, To Do, In Progress, Done
    BasicKanban,
    /// Forgejo's bug-triage layout
    BugTriage,
}

impl Template {
    fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::BasicKanban => 1,
            Self::BugTriage => 2,
        }
    }
}

/// Where the boards live: a repository (the default) or an owner.
#[derive(Debug, ClapArgs)]
pub struct ScopeArgs {
    /// An organisation or user's boards instead of the repository's
    #[arg(long, value_name = "ORG")]
    pub owner: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Which boards to show
    #[arg(short = 's', long, value_enum, default_value_t = State::Open)]
    pub state: State,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

/// A board, by title — or by id when two share a title.
#[derive(Debug, ClapArgs)]
pub struct NameArgs {
    /// Board title
    #[arg(value_name = "TITLE")]
    pub title: String,
    /// Pick by id, when a title is ambiguous
    #[arg(long, value_name = "ID")]
    pub id: Option<i64>,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    #[command(flatten)]
    pub name: NameArgs,
    /// Open the board in a browser instead
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Board title
    #[arg(value_name = "TITLE")]
    pub title: String,
    /// Description
    #[arg(long, default_value = "")]
    pub description: String,
    /// Starter columns
    ///
    /// Named `--from-template` and not `--template` because the global `-t/--template` already
    /// owns that name, and clap answers a duplicate long name with a **panic** rather than an
    /// error — so the collision crashes on an ordinary command line instead of printing usage.
    /// `repo create --from-template` exists for exactly this reason; see
    /// docs/porcelain-conventions.md.
    #[arg(long = "from-template", value_enum, default_value_t = Template::None)]
    pub template: Template,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub name: NameArgs,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum ColumnCmd {
    /// Add a column
    Add(ColumnAddArgs),
    /// Delete a column
    Delete(ColumnRefArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ColumnAddArgs {
    /// Board title
    #[arg(value_name = "PROJECT")]
    pub project: String,
    /// New column's title
    #[arg(value_name = "TITLE")]
    pub title: String,
    /// The column's colour, as `#rrggbb`
    ///
    /// Named `--hex` and not `--color` because the global `--color` (when to colourise output)
    /// already owns that name, and a duplicate long name makes clap **panic** rather than error.
    /// `quota rules create --bytes` names the value type for the same reason; see
    /// docs/porcelain-conventions.md.
    #[arg(long = "hex", value_name = "RRGGBB", default_value = "")]
    pub color: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct ColumnRefArgs {
    /// Board title
    #[arg(value_name = "PROJECT")]
    pub project: String,
    /// Column title
    #[arg(value_name = "COLUMN")]
    pub column: String,
    /// Do not ask for confirmation
    #[arg(long)]
    pub yes: bool,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, Subcommand)]
pub enum CardCmd {
    /// Put an issue on a board
    Add(CardAddArgs),
    /// Move an issue into a column
    Move(CardMoveArgs),
}

#[derive(Debug, ClapArgs)]
pub struct CardAddArgs {
    /// Issue number
    #[arg(value_name = "ISSUE")]
    pub issue: i64,
    /// Board title
    #[arg(long, value_name = "TITLE")]
    pub project: String,
    /// Column to place it in; defaults to the board's own default column
    #[arg(long, value_name = "COLUMN")]
    pub column: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct CardMoveArgs {
    /// Issue number
    #[arg(value_name = "ISSUE")]
    pub issue: i64,
    /// Column to move it into
    #[arg(long, value_name = "COLUMN")]
    pub to: String,
    /// Board title; inferred from the issue when omitted
    #[arg(long, value_name = "TITLE")]
    pub project: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let mut rt = Runtime::new(globals)?;
    crate::runtime::block_on(dispatch(&mut rt, globals, args))
}

async fn dispatch(rt: &mut Runtime, globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.cmd {
        Cmd::List(a) => cmd_list(rt, globals, a).await,
        Cmd::View(a) => cmd_view(rt, globals, a).await,
        Cmd::Create(a) => {
            let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
            ops::create(rt, &scope, &a.title, &a.description, a.template.code()).await?;
            println!("✓ created board {:?}", a.title);
            Ok(())
        }
        Cmd::Close(a) => set_state(rt, globals, a, "close", "closed").await,
        Cmd::Reopen(a) => set_state(rt, globals, a, "open", "reopened").await,
        Cmd::Delete(a) => cmd_delete(rt, globals, a).await,
        Cmd::Column(c) => cmd_column(rt, globals, c).await,
        Cmd::Card(c) => cmd_card(rt, globals, c).await,
    }
}

async fn cmd_list(rt: &mut Runtime, globals: &GlobalOpts, a: &ListArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
    let boards = ops::list(rt, &scope, a.state.as_str()).await?;

    if globals.json.is_some() || globals.jq.is_some() || globals.template.is_some() {
        return emit(rt, globals, &boards);
    }
    let term = *rt.term();
    let mut table = Table::new(&term);
    table.headers(["TITLE", "STATE", "ID"]);
    for b in &boards {
        table.row([
            b.title.clone(),
            if b.closed { "closed" } else { "open" }.to_owned(),
            b.id.to_string(),
        ]);
    }
    let mut out = std::io::stdout();
    table.render(&mut out).map_err(|e| usage(format!("could not write the table: {e}")))
}

async fn cmd_view(rt: &mut Runtime, globals: &GlobalOpts, a: &ViewArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, a.name.scope.owner.as_deref())?;
    let found = ops::find(rt, &scope, &a.name.title, a.name.id).await?;

    if a.web {
        // The WebClient already knows the instance's web root and has validated it; asking it
        // avoids a second way to build the same URL, which is a second way to get it wrong.
        let url = rt.web_client()?.url_for(&scope.board(found.id))?;
        return open::that(&url).map_err(|e| usage(format!("could not open a browser: {e}")));
    }

    let board = ops::view(rt, &scope, found.id).await?;
    if globals.json.is_some() || globals.jq.is_some() || globals.template.is_some() {
        return emit(rt, globals, &board);
    }

    let term = *rt.term();
    let mut table = Table::new(&term);
    table.headers(["COLUMN", "ISSUE", "TITLE"]);
    for column in &board.columns {
        if column.cards.is_empty() {
            table.row([column.title.clone(), "-".to_owned(), "(empty)".to_owned()]);
            continue;
        }
        for card in &column.cards {
            table.row([column.title.clone(), format!("#{}", card.number), card.title.clone()]);
        }
    }
    table.banner(format!("{} — {} columns", board.title, board.columns.len()));
    let mut out = std::io::stdout();
    table.render(&mut out).map_err(|e| usage(format!("could not write the table: {e}")))
}

async fn set_state(
    rt: &mut Runtime,
    globals: &GlobalOpts,
    a: &NameArgs,
    action: &str,
    said: &str,
) -> Result<()> {
    let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
    let found = ops::find(rt, &scope, &a.title, a.id).await?;
    ops::set_state(rt, &scope, found.id, action).await?;
    println!("✓ {said} board {:?}", found.title);
    Ok(())
}

async fn cmd_delete(rt: &mut Runtime, globals: &GlobalOpts, a: &DeleteArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, a.name.scope.owner.as_deref())?;
    let found = ops::find(rt, &scope, &a.name.title, a.name.id).await?;
    // Deleting a board destroys its columns and its cards' placement. Confirming is the house
    // rule for a destructive verb, and `--yes` is how a script says it meant it.
    confirm(a.yes, &format!("delete board {:?} and its columns?", found.title))?;
    ops::set_state(rt, &scope, found.id, "delete").await?;
    println!("✓ deleted board {:?}", found.title);
    Ok(())
}

async fn cmd_column(rt: &mut Runtime, globals: &GlobalOpts, c: &ColumnCmd) -> Result<()> {
    match c {
        ColumnCmd::Add(a) => {
            let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
            let found = ops::find(rt, &scope, &a.project, None).await?;
            ops::add_column(rt, &scope, found.id, &a.title, &a.color).await?;
            println!("✓ added column {:?} to {:?}", a.title, found.title);
            Ok(())
        }
        ColumnCmd::Delete(a) => {
            let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
            let found = ops::find(rt, &scope, &a.project, None).await?;
            let board = ops::view(rt, &scope, found.id).await?;
            let column = ops::column_of(&board, &a.column)?;
            confirm(a.yes, &format!("delete column {:?} from {:?}?", a.column, board.title))?;
            ops::delete_column(rt, &scope, found.id, column).await?;
            println!("✓ deleted column {:?}", a.column);
            Ok(())
        }
    }
}

async fn cmd_card(rt: &mut Runtime, globals: &GlobalOpts, c: &CardCmd) -> Result<()> {
    match c {
        CardCmd::Add(a) => {
            let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
            let found = ops::find(rt, &scope, &a.project, None).await?;
            let internal = internal_id(rt, globals, a.issue).await?;
            ops::add_cards(rt, &scope, found.id, &[internal]).await?;
            if let Some(col) = &a.column {
                let board = ops::view(rt, &scope, found.id).await?;
                let column = ops::column_of(&board, col)?;
                ops::move_cards(rt, &scope, found.id, column, &[internal]).await?;
            }
            println!("✓ put #{} on {:?}", a.issue, found.title);
            Ok(())
        }
        CardCmd::Move(a) => {
            let scope = Scope::resolve(rt, globals, a.scope.owner.as_deref())?;
            let internal = internal_id(rt, globals, a.issue).await?;
            // An issue sits on at most one board per repository, so naming the board is
            // unnecessary when it can be found -- which is what makes
            // `fjo project card move 42 --to Done` a one-liner in CI.
            let (found, board) = locate(rt, &scope, a.project.as_deref(), internal).await?;
            let column = ops::column_of(&board, &a.to)?;
            ops::move_cards(rt, &scope, found.id, column, &[internal]).await?;
            println!("✓ moved #{} to {:?} on {:?}", a.issue, a.to, board.title);
            Ok(())
        }
    }
}

/// The board an issue is already on, or the one the user named.
async fn locate(
    rt: &mut Runtime,
    scope: &Scope,
    named: Option<&str>,
    internal: i64,
) -> Result<(ProjectRef, Board)> {
    if let Some(title) = named {
        let found = ops::find(rt, scope, title, None).await?;
        let board = ops::view(rt, scope, found.id).await?;
        return Ok((found, board));
    }
    for found in ops::list(rt, scope, "open").await? {
        let board = ops::view(rt, scope, found.id).await?;
        if board.columns.iter().any(|c| c.cards.iter().any(|card| card.id == internal)) {
            return Ok((found, board));
        }
    }
    Err(usage(
        "that issue is not on any open board. Put it on one with `fjo project card add`, or \
         name the board with --project.",
    ))
}

/// The issue's internal database id, which the move endpoint wants and the user never sees.
async fn internal_id(rt: &Runtime, globals: &GlobalOpts, number: i64) -> Result<i64> {
    let repo = rt.repo(globals)?;
    let api = forgejo_client::Api::new(rt.client().clone());
    let issue = api.issue().get_issue(&repo.slug.owner, &repo.slug.name, number).await?;
    Ok(issue.id.get())
}

fn emit<T: serde::Serialize>(rt: &Runtime, globals: &GlobalOpts, value: &T) -> Result<()> {
    let json = serde_json::to_value(value)
        .map_err(|e| usage(format!("could not render that as JSON: {e}")))?;
    let fields = globals.json.as_deref().map(|l| {
        l.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>()
    });
    let filter = globals.jq.as_deref().map(crate::output::Filter::compile).transpose()?;
    let template = globals.template.as_deref().map(crate::output::Template::parse).transpose()?;
    let pipeline = crate::output::Pipeline::new()
        .fields(fields.as_deref())
        .jq(filter.as_ref())
        .template(template.as_ref());
    let mut out = std::io::stdout();
    pipeline.render(json, rt.term(), &mut out)
}

fn confirm(yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(usage(format!("{question} pass --yes to confirm")));
    }
    let ok = inquire::Confirm::new(question)
        .with_default(false)
        .prompt()
        .map_err(|e| usage(format!("could not read the answer: {e}")))?;
    if ok { Ok(()) } else { Err(usage("cancelled")) }
}

fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage(msg.into()))
}

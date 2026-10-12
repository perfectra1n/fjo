//! `gea project` — Gitea's kanban boards.
//!
//! Built on Gitea 28's REST projects API (`gea raw repo list-projects` and friends). Before 28
//! there was no such API and these were layer 0 commands driving the board UI's web routes with
//! a session cookie; see `docs/layers.md`.
//!
//! What this adds over `gea raw`, which reaches the same routes: **everything is named by
//! title.** Gitea's project routes are all `/projects/{id}/{columnID}`, and nobody knows that
//! a board called `Roadmap` is id 3 or that its `Done` column is id 12. The same reasoning put
//! `milestone_by_title` behind `gea milestone`, and the same corollary applies — an ambiguous
//! title is refused with the matches listed, never guessed, because Gitea is perfectly happy
//! to hold two boards called `Roadmap`.
//!
pub mod model;
pub mod ops;

use clap::{Args as ClapArgs, Subcommand, ValueEnum};

use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};

pub use model::{Board, Card, Column, ProjectRef};

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

These use Gitea's projects API, which needs Gitea 28 or later, and an
ordinary token: no web session is involved.

  gea project list
  gea project view Roadmap
  gea project card move 42 --to Done";

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
    /// Gitea's bug-triage layout
    BugTriage,
}

impl Template {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BasicKanban => "basic_kanban",
            Self::BugTriage => "bug_triage",
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
    /// Rename a column or change its colour
    Edit(ColumnEditArgs),
    /// Reorder a column
    Move(ColumnMoveArgs),
    /// Delete a column
    Delete(ColumnRefArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ColumnEditArgs {
    /// Board title
    #[arg(value_name = "PROJECT")]
    pub project: String,
    /// Column to change
    #[arg(value_name = "COLUMN")]
    pub column: String,
    /// New title
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,
    /// New colour, as `#rrggbb` (see `column add --hex` for why it is not `--color`)
    #[arg(long = "hex", value_name = "RRGGBB")]
    pub color: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
#[command(group = clap::ArgGroup::new("where").required(true).multiple(false))]
pub struct ColumnMoveArgs {
    /// Board title
    #[arg(value_name = "PROJECT")]
    pub project: String,
    /// Column to move
    #[arg(value_name = "COLUMN")]
    pub column: String,
    /// Put it before this column
    #[arg(long, value_name = "COLUMN", group = "where")]
    pub before: Option<String>,
    /// Put it after this column
    #[arg(long, value_name = "COLUMN", group = "where")]
    pub after: Option<String>,
    /// Put it first
    #[arg(long, group = "where")]
    pub first: bool,
    /// Put it last
    #[arg(long, group = "where")]
    pub last: bool,
    #[command(flatten)]
    pub scope: ScopeArgs,
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
    let rt = Runtime::new(globals)?;
    let api = Api::new(rt.client().clone());
    crate::runtime::block_on(dispatch(&rt, globals, &api, args))
}

async fn dispatch(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &Args) -> Result<()> {
    match &args.cmd {
        Cmd::List(a) => cmd_list(rt, globals, api, a).await,
        Cmd::View(a) => cmd_view(rt, globals, api, a).await,
        Cmd::Create(a) => {
            let scope = Scope::resolve(rt, globals, api, a.scope.owner.as_deref()).await?;
            let made =
                ops::create(api, &scope, &a.title, &a.description, a.template.as_str()).await?;
            println!("✓ created board {:?} (id {})", made.title, made.id);
            Ok(())
        }
        Cmd::Close(a) => set_state(rt, globals, api, a, true, "closed").await,
        Cmd::Reopen(a) => set_state(rt, globals, api, a, false, "reopened").await,
        Cmd::Delete(a) => cmd_delete(rt, globals, api, a).await,
        Cmd::Column(c) => cmd_column(rt, globals, api, c).await,
        Cmd::Card(c) => cmd_card(rt, globals, api, c).await,
    }
}

async fn cmd_list(rt: &Runtime, globals: &GlobalOpts, api: &Api, a: &ListArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, api, a.scope.owner.as_deref()).await?;
    let boards = ops::list(api, &scope, a.state.as_str()).await?;

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

async fn cmd_view(rt: &Runtime, globals: &GlobalOpts, api: &Api, a: &ViewArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, api, a.name.scope.owner.as_deref()).await?;
    let found = ops::find(api, &scope, &a.name.title, a.name.id).await?;

    if a.web {
        // The server's own link to the board, so there is no second way to build the URL.
        return open::that(&found.html_url)
            .map_err(|e| usage(format!("could not open a browser: {e}")));
    }

    let board = ops::view(api, &scope, &found).await?;
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
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    a: &NameArgs,
    closed: bool,
    said: &str,
) -> Result<()> {
    let scope = Scope::resolve(rt, globals, api, a.scope.owner.as_deref()).await?;
    let found = ops::find(api, &scope, &a.title, a.id).await?;
    ops::set_state(api, &scope, found.id, closed).await?;
    println!("✓ {said} board {:?}", found.title);
    Ok(())
}

async fn cmd_delete(rt: &Runtime, globals: &GlobalOpts, api: &Api, a: &DeleteArgs) -> Result<()> {
    let scope = Scope::resolve(rt, globals, api, a.name.scope.owner.as_deref()).await?;
    let found = ops::find(api, &scope, &a.name.title, a.name.id).await?;
    // Deleting a board destroys its columns and its cards' placement. Confirming is the house
    // rule for a destructive verb, and `--yes` is how a script says it meant it.
    confirm(a.yes, &format!("delete board {:?} and its columns?", found.title))?;
    ops::delete(api, &scope, found.id).await?;
    println!("✓ deleted board {:?}", found.title);
    Ok(())
}

/// The board a column command names, read whole.
async fn board_of(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    owner: Option<&str>,
    project: &str,
) -> Result<(Scope, Board)> {
    let scope = Scope::resolve(rt, globals, api, owner).await?;
    let found = ops::find(api, &scope, project, None).await?;
    let board = ops::view(api, &scope, &found).await?;
    Ok((scope, board))
}

async fn cmd_column(rt: &Runtime, globals: &GlobalOpts, api: &Api, c: &ColumnCmd) -> Result<()> {
    match c {
        ColumnCmd::Add(a) => {
            let scope = Scope::resolve(rt, globals, api, a.scope.owner.as_deref()).await?;
            let found = ops::find(api, &scope, &a.project, None).await?;
            ops::add_column(api, &scope, found.id, &a.title, &a.color).await?;
            println!("✓ added column {:?} to {:?}", a.title, found.title);
            Ok(())
        }
        ColumnCmd::Edit(a) => {
            // Nothing to change is a usage error before any request, the rule every other edit
            // command here follows (see `gea milestone edit`).
            if a.title.is_none() && a.color.is_none() {
                return Err(usage("nothing to change: pass --title, --hex, or both"));
            }
            let (scope, board) =
                board_of(rt, globals, api, a.scope.owner.as_deref(), &a.project).await?;
            let column = ops::column_of(&board, &a.column)?;
            let (title, color) = (a.title.as_deref(), a.color.as_deref());
            ops::edit_column(api, &scope, board.id, column, title, color).await?;
            println!("✓ updated column {:?} on {:?}", a.column, board.title);
            Ok(())
        }
        ColumnCmd::Move(a) => {
            let (scope, board) =
                board_of(rt, globals, api, a.scope.owner.as_deref(), &a.project).await?;
            let moving = ops::column_of(&board, &a.column)?;
            let to = match (&a.before, &a.after, a.first, a.last) {
                (Some(x), _, _, _) => ops::Move::Before(x),
                (_, Some(x), _, _) => ops::Move::After(x),
                (_, _, true, _) => ops::Move::First,
                (_, _, _, true) => ops::Move::Last,
                // clap's ArgGroup is `required`, so this cannot be reached; an error beats a
                // panic for a shape clap has not thought of.
                _ => return Err(usage("say where: --before, --after, --first or --last")),
            };
            let ordered = ops::reordered(&board.columns, moving, to)?;
            ops::reorder_columns(api, &scope, board.id, &ordered).await?;
            println!("✓ moved column {:?} on {:?}", a.column, board.title);
            Ok(())
        }
        ColumnCmd::Delete(a) => {
            let (scope, board) =
                board_of(rt, globals, api, a.scope.owner.as_deref(), &a.project).await?;
            let column_id = ops::column_of(&board, &a.column)?;
            let column = board
                .columns
                .iter()
                .find(|c| c.id == column_id)
                .ok_or_else(|| usage("that column vanished from the board"))?;
            confirm(a.yes, &format!("delete column {:?} from {:?}?", a.column, board.title))?;
            ops::delete_column(api, &scope, board.id, column).await?;
            println!("✓ deleted column {:?}", a.column);
            Ok(())
        }
    }
}

async fn cmd_card(rt: &Runtime, globals: &GlobalOpts, api: &Api, c: &CardCmd) -> Result<()> {
    match c {
        CardCmd::Add(a) => {
            let (scope, board) =
                board_of(rt, globals, api, a.scope.owner.as_deref(), &a.project).await?;
            let issue = issue_id(rt, globals, api, a.issue).await?;
            let column = match &a.column {
                Some(name) => ops::column_of(&board, name)?,
                None => ops::default_column(&board)?,
            };
            ops::add_card(api, &scope, board.id, column, issue).await?;
            println!("✓ put #{} on {:?}", a.issue, board.title);
            Ok(())
        }
        CardCmd::Move(a) => {
            let scope = Scope::resolve(rt, globals, api, a.scope.owner.as_deref()).await?;
            let issue = issue_id(rt, globals, api, a.issue).await?;
            // Naming the board is unnecessary when the issue is on exactly one, which is what
            // makes `gea project card move 42 --to Done` a one-liner in CI.
            let board = locate(api, &scope, a.project.as_deref(), issue).await?;
            let column = ops::column_of(&board, &a.to)?;
            ops::move_card(api, &scope, board.id, column, issue).await?;
            println!("✓ moved #{} to {:?} on {:?}", a.issue, a.to, board.title);
            Ok(())
        }
    }
}

/// The board an issue is already on, or the one the user named.
async fn locate(api: &Api, scope: &Scope, named: Option<&str>, issue: i64) -> Result<Board> {
    if let Some(title) = named {
        let found = ops::find(api, scope, title, None).await?;
        return ops::view(api, scope, &found).await;
    }
    // Gitea lets an issue sit on several boards, so every open board is read and more than
    // one hit is refused rather than resolved to whichever the listing returned first.
    let mut hits = Vec::new();
    for found in ops::list(api, scope, "open").await? {
        let board = ops::view(api, scope, &found).await?;
        if board.columns.iter().any(|c| c.cards.iter().any(|card| card.id == issue)) {
            hits.push(board);
        }
    }
    match hits.len() {
        0 => Err(usage(
            "that issue is not on any open board. Put it on one with `gea project card add`, or \
             name the board with --project.",
        )),
        1 => Ok(hits.remove(0)),
        _ => Err(usage(format!(
            "that issue is on {} open boards ({}); name the one you mean with --project",
            hits.len(),
            hits.iter().map(|b| format!("{:?}", b.title)).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// The global id of issue `#number` in the current repository — the id every project route
/// takes, which differs from the number the user typed as soon as an issue is transferred.
async fn issue_id(rt: &Runtime, globals: &GlobalOpts, api: &Api, number: i64) -> Result<i64> {
    let repo = rt.repo(globals)?;
    let issue = api.issue().get_issue(&repo.slug.owner, &repo.slug.name, number).await?;
    Ok(issue.id.get())
}

fn emit<T: serde::Serialize>(rt: &Runtime, globals: &GlobalOpts, value: &T) -> Result<()> {
    let json = serde_json::to_value(value)
        .map_err(|e| usage(format!("could not render that as JSON: {e}")))?;
    // A bare `--json` has an empty field list, which must mean "the whole document" and not
    // "project nothing" -- the latter renders `{}`, which looks like an empty board.
    let fields = globals
        .json
        .as_deref()
        .map(|l| {
            l.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|f: &Vec<String>| !f.is_empty());
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

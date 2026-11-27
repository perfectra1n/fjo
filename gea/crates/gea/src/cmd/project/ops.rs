//! The REST operations behind `gea project`, and the title→id resolution they all need.
//!
//! # Why every verb resolves a title
//!
//! Gitea's project routes are `/projects/{id}` and `/projects/{id}/columns/{column_id}`. Nobody
//! knows that `Roadmap` is 3 or that its `Done` column is 12, and a command that demanded those
//! numbers would be `gea raw` with extra steps. So each verb reads the listing (or the board) and
//! maps the name the user typed onto the id the route wants.
//!
//! An **ambiguous** name is refused with the matches listed, never guessed. Gitea is perfectly
//! willing to hold two boards called `Roadmap`, exactly as it holds two milestones called `1.0`
//! — which real testing found, and which is why `gea milestone` established this rule first.
//!
//! # History
//!
//! Gitea had no REST API for projects before 28, so this module used to drive the web routes
//! behind the board UI with a session cookie and scrape the board's HTML. Gitea 28 added the API
//! and this moved onto it, as `docs/layers.md` planned: the commands now need only a token, and
//! the markup parser and its captured fixtures are gone.

use futures::{StreamExt, TryStreamExt};
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::{
    CreateProjectColumnOption, CreateProjectOption, EditProjectColumnOption, EditProjectOption,
    EditProjectOptionState, MoveProjectColumnsOption, MoveProjectIssueOption,
};

use super::model::{Board, Card, Column, ProjectRef};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// The most boards, columns or cards one listing is drained to. A board with more than this is
/// not something a terminal table serves, and an unbounded walk is a hang waiting to happen.
const RESOLVE_CAP: usize = 1000;

/// Whose boards: a repository's, an organization's, the signed-in user's, or — read-only —
/// another user's.
///
/// Gitea serves the same API under `/repos/{o}/{r}/projects`, `/orgs/{org}/projects` and
/// `/user/projects`. A user's boards can be *managed* only by that user, through `/user`;
/// anyone else gets the public listing at `/users/{u}/projects` and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Repo { owner: String, repo: String },
    Org(String),
    Me,
    User(String),
}

impl Scope {
    /// `--owner` names an organization or a user, and the API addresses the two differently,
    /// so which one it is gets asked rather than assumed.
    pub async fn resolve(
        rt: &Runtime,
        globals: &GlobalOpts,
        api: &Api,
        owner: Option<&str>,
    ) -> Result<Self> {
        let Some(owner) = owner else {
            let repo = rt.repo(globals)?;
            return Ok(Self::Repo { owner: repo.slug.owner.clone(), repo: repo.slug.name.clone() });
        };
        match api.org().get(owner).await {
            Ok(_) => return Ok(Self::Org(owner.to_owned())),
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e),
        }
        let me = api.user().get_current().await?;
        if me.login.eq_ignore_ascii_case(owner) {
            Ok(Self::Me)
        } else {
            Ok(Self::User(owner.to_owned()))
        }
    }
}

/// Run one project operation at whichever owner the scope names.
///
/// The three route families take the same arguments after their owner prefix, and the
/// generated user methods are the repository ones with a `current_` prefix, so one macro keeps
/// each operation written once rather than three times.
macro_rules! at {
    ($api:expr, $scope:expr, $repo_fn:ident / $user_fn:ident ( $($arg:expr),* $(,)? )) => {
        match $scope {
            Scope::Repo { owner, repo } => $api.repo().$repo_fn(owner, repo, $($arg),*).await,
            Scope::Org(org) => $api.org().$repo_fn(org, $($arg),*).await,
            Scope::Me => $api.user().$user_fn($($arg),*).await,
            Scope::User(name) => Err(read_only(name)),
        }
    };
}

/// The same, for the streaming listings, which are collected rather than awaited.
macro_rules! all {
    ($api:expr, $scope:expr, $repo_fn:ident / $user_fn:ident ( $($arg:expr),* $(,)? )) => {
        match $scope {
            Scope::Repo { owner, repo } => {
                $api.repo().$repo_fn(owner, repo, $($arg,)* &Default::default())
                    .take(RESOLVE_CAP).try_collect().await
            }
            Scope::Org(org) => {
                $api.org().$repo_fn(org, $($arg,)* &Default::default())
                    .take(RESOLVE_CAP).try_collect().await
            }
            Scope::Me => {
                $api.user().$user_fn($($arg,)* &Default::default())
                    .take(RESOLVE_CAP).try_collect().await
            }
            Scope::User(name) => Err(read_only(name)),
        }
    };
}

/// Every board in one state.
pub async fn list(api: &Api, scope: &Scope, state: &str) -> Result<Vec<ProjectRef>> {
    let state = Some(state.to_owned());
    let boards: Vec<gitea_model::Project> = match scope {
        Scope::Repo { owner, repo } => {
            let q = gitea_client::query::RepoListProjectsQuery { state, ..Default::default() };
            api.repo().list_projects(owner, repo, &q).take(RESOLVE_CAP).try_collect().await?
        }
        Scope::Org(org) => {
            let q = gitea_client::query::OrgListProjectsQuery { state, ..Default::default() };
            api.org().list_projects(org, &q).take(RESOLVE_CAP).try_collect().await?
        }
        Scope::Me => {
            let q =
                gitea_client::query::UserCurrentListProjectsQuery { state, ..Default::default() };
            api.user().current_list_projects(&q).take(RESOLVE_CAP).try_collect().await?
        }
        Scope::User(name) => {
            let q = gitea_client::query::UserListProjectsQuery { state, ..Default::default() };
            api.user().list_projects(name, &q).take(RESOLVE_CAP).try_collect().await?
        }
    };
    Ok(boards.into_iter().map(ProjectRef::from).collect())
}

/// The board a user named, by title or by `--id`.
pub async fn find(api: &Api, scope: &Scope, name: &str, id: Option<i64>) -> Result<ProjectRef> {
    // Closed boards are searched too: closing one must not make it unreachable, which is the
    // rule `gea milestone list -s all` already follows.
    let all = list(api, scope, "all").await?;
    if let Some(id) = id {
        return all
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| not_found(&format!("no board with id {id}")));
    }
    let mut hits: Vec<ProjectRef> = all.into_iter().filter(|p| p.title == name).collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(not_found(&format!("no board called {name:?}"))),
        _ => {
            let ids: Vec<String> = hits.iter().map(|p| p.id.to_string()).collect();
            Err(Error::new(ErrorKind::Usage(format!(
                "{} boards are called {name:?} (ids {}). Name one with --id <ID>.",
                hits.len(),
                ids.join(", ")
            ))))
        }
    }
}

/// A board's full contents: its columns in order, and each column's cards.
pub async fn view(api: &Api, scope: &Scope, board: &ProjectRef) -> Result<Board> {
    let mut columns: Vec<gitea_model::ProjectColumn> =
        all!(api, scope, list_project_columns / current_list_project_columns(board.id))?;
    columns.sort_by_key(|c| (c.sorting, c.id));
    let mut out = Vec::with_capacity(columns.len());
    for column in columns {
        let issues: Vec<gitea_model::Issue> = all!(
            api,
            scope,
            list_project_column_issues / current_list_project_column_issues(board.id, column.id)
        )?;
        out.push(Column::new(column, issues.into_iter().map(Card::from).collect()));
    }
    Ok(Board { id: board.id, title: board.title.clone(), columns: out })
}

/// A column on a board, by title.
pub fn column_of(board: &Board, name: &str) -> Result<i64> {
    let hits: Vec<&Column> = board.columns.iter().filter(|c| c.title == name).collect();
    match hits.len() {
        1 => Ok(hits[0].id),
        0 => {
            let have: Vec<&str> = board.columns.iter().map(|c| c.title.as_str()).collect();
            Err(not_found(&format!(
                "no column called {name:?} on {:?}. It has: {}",
                board.title,
                have.join(", ")
            )))
        }
        _ => Err(Error::new(ErrorKind::Usage(format!(
            "{} columns on {:?} are called {name:?}; rename one",
            hits.len(),
            board.title
        )))),
    }
}

pub async fn create(
    api: &Api,
    scope: &Scope,
    title: &str,
    description: &str,
    template: &str,
) -> Result<ProjectRef> {
    // `images_and_text` matches what the web UI creates, so a board made here looks the same as
    // one made by hand.
    let body = CreateProjectOption {
        title: title.to_owned(),
        description: Some(description.to_owned()),
        template_type: Some(template.to_owned()),
        card_type: Some("images_and_text".to_owned()),
    };
    let made: gitea_model::Project =
        at!(api, scope, create_project / current_create_project(&body))?;
    Ok(made.into())
}

pub async fn set_state(api: &Api, scope: &Scope, id: i64, closed: bool) -> Result<()> {
    let state = if closed { EditProjectOptionState::Closed } else { EditProjectOptionState::Open };
    let body = EditProjectOption { state: Some(state), ..Default::default() };
    let _: gitea_model::Project = at!(api, scope, edit_project / current_edit_project(id, &body))?;
    Ok(())
}

pub async fn delete(api: &Api, scope: &Scope, id: i64) -> Result<()> {
    at!(api, scope, delete_project / current_delete_project(id))
}

pub async fn add_column(api: &Api, scope: &Scope, id: i64, title: &str, color: &str) -> Result<()> {
    let body = CreateProjectColumnOption {
        title: title.to_owned(),
        color: Some(color.to_owned()).filter(|c| !c.is_empty()),
    };
    let _: gitea_model::ProjectColumn =
        at!(api, scope, create_project_column / current_create_project_column(id, &body))?;
    Ok(())
}

/// Rename or recolour a column. The body is a partial update, so only what changes is sent and
/// the column's position is left alone.
pub async fn edit_column(
    api: &Api,
    scope: &Scope,
    id: i64,
    column: i64,
    title: Option<&str>,
    color: Option<&str>,
) -> Result<()> {
    let body = EditProjectColumnOption {
        title: title.map(str::to_owned),
        color: color.map(str::to_owned),
        sorting: None,
    };
    let _: gitea_model::ProjectColumn =
        at!(api, scope, edit_project_column / current_edit_project_column(id, column, &body))?;
    Ok(())
}

/// Write a new column order for a whole board.
pub async fn reorder_columns(api: &Api, scope: &Scope, id: i64, ordered: &[i64]) -> Result<()> {
    let body = MoveProjectColumnsOption { column_ids: ordered.to_vec() };
    at!(api, scope, move_project_columns / current_move_project_columns(id, &body))
}

pub async fn delete_column(api: &Api, scope: &Scope, id: i64, column: &Column) -> Result<()> {
    // Checked here rather than left to the server, so the refusal says what to do about it.
    if column.default {
        return Err(Error::new(ErrorKind::Usage(format!(
            "{:?} is the board's default column, where issues land when no column is given, and \
             Gitea will not delete it. Make another column the default first.",
            column.title
        ))));
    }
    at!(api, scope, delete_project_column / current_delete_project_column(id, column.id))
}

/// Put an issue on a board, in `column`.
///
/// Unlike the web route this replaced, which *set* an issue's whole collection of boards and so
/// had to be sent the ones it was already on, the API adds one placement and leaves the issue's
/// other boards alone.
pub async fn add_card(api: &Api, scope: &Scope, id: i64, column: i64, issue: i64) -> Result<()> {
    at!(
        api,
        scope,
        add_issue_to_project_column / current_add_issue_to_project_column(id, column, issue)
    )
}

/// Move an issue that is already on a board into another of its columns.
pub async fn move_card(api: &Api, scope: &Scope, id: i64, column: i64, issue: i64) -> Result<()> {
    let body = MoveProjectIssueOption { column_id: column, sorting: None };
    at!(api, scope, move_project_issue / current_move_project_issue(id, issue, &body))
}

/// The column an issue lands in when none is named: the board's default, as in the web UI.
pub fn default_column(board: &Board) -> Result<i64> {
    board
        .columns
        .iter()
        .find(|c| c.default)
        .or_else(|| board.columns.first())
        .map(|c| c.id)
        .ok_or_else(|| {
            Error::new(ErrorKind::Usage(format!(
                "{:?} has no columns to put the issue in; add one with `gea project column add`",
                board.title
            )))
        })
}

fn read_only(user: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "{user} is another user, and Gitea lets only a user manage their own boards. Their boards \
         can be listed (`gea project list --owner {user}`); to change one, sign in as {user}."
    )))
}

fn is_not_found(e: &Error) -> bool {
    matches!(e.kind(), ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. })
}

/// Where a column should end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move<'a> {
    First,
    Last,
    Before(&'a str),
    After(&'a str),
}

/// The new column order, as ids.
///
/// Pure, and separated from the request for exactly that reason: off-by-one is the entire risk
/// in a reorder, and this is the part a test can pin without a server. Every case below was a
/// wrong answer in a first draft.
pub fn reordered(columns: &[Column], moving: i64, to: Move<'_>) -> Result<Vec<i64>> {
    let mut ids: Vec<i64> = columns.iter().map(|c| c.id).collect();
    let Some(from) = ids.iter().position(|id| *id == moving) else {
        return Err(not_found("that column is not on this board"));
    };
    ids.remove(from);

    let index_of = |name: &str| -> Result<usize> {
        let target = columns
            .iter()
            .find(|c| c.title == name)
            .ok_or_else(|| not_found(&format!("no column called {name:?} on this board")))?;
        if target.id == moving {
            return Err(Error::new(ErrorKind::Usage(
                "a column cannot be moved relative to itself".to_owned(),
            )));
        }
        // Positions are looked up in the list with the moving column ALREADY REMOVED, so the
        // anchor's index is the one that matters after the removal, not before it. Computing it
        // against the original list is off by one whenever the column moves rightwards.
        ids.iter()
            .position(|id| *id == target.id)
            .ok_or_else(|| not_found("the anchor column vanished from the board"))
    };

    let at = match to {
        Move::First => 0,
        Move::Last => ids.len(),
        Move::Before(name) => index_of(name)?,
        Move::After(name) => index_of(name)? + 1,
    };
    ids.insert(at, moving);
    Ok(ids)
}

fn not_found(msg: &str) -> Error {
    Error::new(ErrorKind::Usage(msg.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::project::model::Column;

    fn board() -> Vec<Column> {
        ["Backlog", "To Do", "Doing", "Done"]
            .iter()
            .enumerate()
            .map(|(i, t)| Column {
                id: i as i64 + 1,
                title: (*t).to_owned(),
                color: None,
                sorting: i as i64,
                default: i == 0,
                cards: Vec::new(),
            })
            .collect()
    }

    fn order(moving: i64, to: Move<'_>) -> Vec<i64> {
        reordered(&board(), moving, to).expect("reorders")
    }

    #[test]
    fn a_column_can_be_sent_to_either_end() {
        assert_eq!(order(3, Move::First), [3, 1, 2, 4]);
        assert_eq!(order(2, Move::Last), [1, 3, 4, 2]);
        // A no-op move is still a valid order, not an error.
        assert_eq!(order(1, Move::First), [1, 2, 3, 4]);
        assert_eq!(order(4, Move::Last), [1, 2, 3, 4]);
    }

    /// Bug this prevents: computing the anchor's index against the ORIGINAL list. The moving
    /// column is removed first, so every anchor to its right shifts left by one — which makes a
    /// rightward move land one place short, and only for rightward moves, which is exactly the
    /// kind of asymmetry that survives a casual test.
    #[test]
    fn a_rightward_move_accounts_for_the_gap_the_column_left_behind() {
        // Backlog(1) after Doing(3): remove 1 -> [2,3,4]; 3 is at index 1; insert at 2.
        assert_eq!(order(1, Move::After("Doing")), [2, 3, 1, 4]);
        // And the leftward direction, where the naive answer happens to be right.
        assert_eq!(order(4, Move::After("Backlog")), [1, 4, 2, 3]);
    }

    #[test]
    fn before_and_after_the_same_anchor_differ_by_exactly_one_place() {
        assert_eq!(order(1, Move::Before("Done")), [2, 3, 1, 4]);
        assert_eq!(order(1, Move::After("Done")), [2, 3, 4, 1]);
    }

    #[test]
    fn every_order_is_a_permutation_of_the_board() {
        for to in [Move::First, Move::Last, Move::Before("Doing"), Move::After("To Do")] {
            for moving in 1..=4 {
                // Anchoring a column against itself is refused on purpose, and is covered by
                // its own test; a sweep over every pair necessarily produces those combinations.
                let anchor = match to {
                    Move::Before(n) | Move::After(n) => {
                        board().iter().find(|c| c.title == n).map(|c| c.id)
                    }
                    _ => None,
                };
                if anchor == Some(moving) {
                    continue;
                }
                let got = reordered(&board(), moving, to).expect("reorders");
                let mut sorted = got.clone();
                sorted.sort_unstable();
                assert_eq!(sorted, [1, 2, 3, 4], "{to:?} of {moving} lost or duplicated a column");
                assert_eq!(got.len(), 4);
            }
        }
    }

    /// Moving a column relative to itself has no meaningful answer, so it is refused rather than
    /// silently treated as a no-op — the user meant something, and it was not this.
    #[test]
    fn a_column_cannot_anchor_against_itself() {
        let err = reordered(&board(), 2, Move::Before("To Do")).expect_err("refuses");
        assert!(err.to_string().contains("itself"), "{err}");
    }

    #[test]
    fn an_unknown_column_or_anchor_is_named_in_the_error() {
        let err = reordered(&board(), 99, Move::First).expect_err("refuses");
        assert!(err.to_string().contains("not on this board"), "{err}");
        let err = reordered(&board(), 1, Move::Before("Nope")).expect_err("refuses");
        assert!(err.to_string().contains("Nope"), "{err}");
    }
}

//! The web operations behind `fjo project`, and the title→id resolution they all need.
//!
//! # Why every verb resolves a title
//!
//! Forgejo's project routes are `/projects/{id}` and `/projects/{id}/{columnID}`. Nobody knows
//! that `Roadmap` is 3 or that its `Done` column is 12, and a command that demanded those
//! numbers would be `fjo web` with extra steps. So each verb reads the index (or the board) and
//! maps the name the user typed onto the id the route wants.
//!
//! An **ambiguous** name is refused with the matches listed, never guessed. Forgejo is perfectly
//! willing to hold two boards called `Roadmap`, exactly as it holds two milestones called `1.0`
//! — which real testing found, and which is why `fjo milestone` established this rule first.

use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::http::Method;
use forgejo_core::web::WebBody;

use super::parse::{Board, ProjectRef, parse_board, parse_index};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// Which board tree to talk to: a repository's, or an owner's.
///
/// Forgejo exposes the same verbs under both `/{owner}/{repo}/projects` and
/// `/{owner}/-/projects`, so this is the only thing that differs between them.
pub struct Scope {
    base: String,
}

impl Scope {
    pub fn resolve(rt: &Runtime, globals: &GlobalOpts, owner: Option<&str>) -> Result<Self> {
        match owner {
            // The `-` is Forgejo's own placeholder for "this owner, not a repository".
            Some(o) => Ok(Self { base: format!("{o}/-/projects") }),
            None => {
                let repo = rt.repo(globals)?;
                Ok(Self { base: format!("{}/{}/projects", repo.slug.owner, repo.slug.name) })
            }
        }
    }

    pub fn index(&self) -> &str {
        &self.base
    }

    pub fn board(&self, id: i64) -> String {
        format!("{}/{id}", self.base)
    }

    pub fn column(&self, project: i64, column: i64) -> String {
        format!("{}/{project}/{column}", self.base)
    }
}

/// Every board on the index, for one state.
pub async fn list(rt: &mut Runtime, scope: &Scope, state: &str) -> Result<Vec<ProjectRef>> {
    // `all` is two requests rather than a `state=all` guess: the template offers only open and
    // closed, and a query value Forgejo does not understand would silently return one of them.
    let states: &[&str] = if state == "all" { &["open", "closed"] } else { &[state] };
    let mut out = Vec::new();
    for s in states {
        let path = format!("{}?state={s}", scope.index());
        let resp = crate::web::request(rt, Method::GET, &path, WebBody::None).await?;
        out.extend(parse_index(&resp.text())?);
    }
    Ok(out)
}

/// The board a user named, by title or by `--id`.
pub async fn find(
    rt: &mut Runtime,
    scope: &Scope,
    name: &str,
    id: Option<i64>,
) -> Result<ProjectRef> {
    if let Some(id) = id {
        let all = list(rt, scope, "all").await?;
        return all
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| not_found(&format!("no board with id {id}")));
    }

    // Closed boards are searched too: closing one must not make it unreachable, which is the
    // rule `fjo milestone list -s all` already follows.
    let all = list(rt, scope, "all").await?;
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

/// A board's full contents.
pub async fn view(rt: &mut Runtime, scope: &Scope, id: i64) -> Result<Board> {
    let resp = crate::web::request(rt, Method::GET, &scope.board(id), WebBody::None).await?;
    parse_board(&resp.text())
}

/// A column on a board, by title.
pub fn column_of(board: &Board, name: &str) -> Result<i64> {
    let hits: Vec<&super::parse::Column> =
        board.columns.iter().filter(|c| c.title == name).collect();
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
    rt: &mut Runtime,
    scope: &Scope,
    title: &str,
    description: &str,
    template: u8,
) -> Result<()> {
    // Lowercase field names, and `card_type` 1 ("images and text") to match what the web UI
    // creates -- both established by driving a real instance, not by reading the Go structs,
    // whose field names are NOT what the form binding reads.
    let form = vec![
        ("title".to_owned(), title.to_owned()),
        ("content".to_owned(), description.to_owned()),
        ("template_type".to_owned(), template.to_string()),
        ("card_type".to_owned(), "1".to_owned()),
    ];
    let path = format!("{}/new", scope.index());
    expect_ok(rt, Method::POST, &path, WebBody::Form(form)).await
}

pub async fn set_state(rt: &mut Runtime, scope: &Scope, id: i64, action: &str) -> Result<()> {
    let path = format!("{}/{action}", scope.board(id));
    expect_ok(rt, Method::POST, &path, WebBody::None).await
}

pub async fn add_column(
    rt: &mut Runtime,
    scope: &Scope,
    id: i64,
    title: &str,
    color: &str,
) -> Result<()> {
    let form = vec![
        ("title".to_owned(), title.to_owned()),
        ("sorting".to_owned(), "0".to_owned()),
        ("color".to_owned(), color.to_owned()),
    ];
    expect_ok(rt, Method::POST, &scope.board(id), WebBody::Form(form)).await
}

pub async fn edit_column(
    rt: &mut Runtime,
    scope: &Scope,
    id: i64,
    column: &super::parse::Column,
    title: &str,
    color: &str,
) -> Result<()> {
    // `sorting` is sent back as it was. The form takes all three fields, so omitting it would
    // renumber the column to 0 and silently move it to the front -- an edit of the title
    // quietly reordering the board is the kind of surprise that erodes trust in a tool.
    let form = vec![
        ("title".to_owned(), title.to_owned()),
        ("sorting".to_owned(), column.sorting.to_string()),
        ("color".to_owned(), color.to_owned()),
    ];
    expect_ok(rt, Method::PUT, &scope.column(id, column.id), WebBody::Form(form)).await
}

/// Write a new column order for a whole board.
pub async fn reorder_columns(
    rt: &mut Runtime,
    scope: &Scope,
    id: i64,
    ordered: &[i64],
) -> Result<()> {
    let columns: Vec<serde_json::Value> = ordered
        .iter()
        .enumerate()
        .map(|(i, cid)| serde_json::json!({ "columnID": cid, "sorting": i }))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({ "columns": columns }))
        .map_err(|e| Error::new(ErrorKind::Usage(format!("could not build the reorder: {e}"))))?;
    let path = format!("{}/move", scope.board(id));
    expect_ok(rt, Method::POST, &path, WebBody::Json(body)).await
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
pub fn reordered(columns: &[super::parse::Column], moving: i64, to: Move<'_>) -> Result<Vec<i64>> {
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

pub async fn delete_column(
    rt: &mut Runtime,
    scope: &Scope,
    id: i64,
    column: &super::parse::Column,
) -> Result<()> {
    // Checked here rather than left to the server. Forgejo refuses this with a bare
    // `errors.New`, which `ctx.ServerError` renders as a 500 with no message -- so a user who
    // tried it got "500 Internal Server Error" for a mistake they could have fixed instantly.
    if column.default {
        return Err(Error::new(ErrorKind::Usage(format!(
            "{:?} is the board's default column, where issues land when no column is given, and \
             Forgejo will not delete it. Make another column the default first.",
            column.title
        ))));
    }
    let sent = expect_ok(rt, Method::DELETE, &scope.column(id, column.id), WebBody::None).await;
    // Forgejo answers this refusal with a 500 carrying no message, so if one arrives anyway --
    // the `default` flag above is read from an absence in the template and may not survive every
    // layout -- say what it almost certainly means rather than relaying "500".
    sent.map_err(|e| {
        if e.to_string().contains("500") {
            return Error::new(ErrorKind::Usage(format!(
                "Forgejo refused to delete {:?} and gave no reason (it answers a 500 here). The \
                 usual cause is that this is the board's default column, which it will not \
                 delete; make another column the default first.",
                column.title
            )));
        }
        e
    })
}

/// Put issues on a board. They land in its default column.
pub async fn add_cards(
    rt: &mut Runtime,
    scope: &Scope,
    project: i64,
    issues: &[i64],
) -> Result<()> {
    // A single comma-joined field, not repeated parameters -- verified against a real instance.
    let joined = issues.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let base = scope.index().trim_end_matches("/projects");
    let path = format!("{base}/issues/projects?id={project}");
    expect_ok(rt, Method::POST, &path, WebBody::Form(vec![("issue_ids".to_owned(), joined)])).await
}

/// Move issues into a column.
///
/// `ids` are **internal database ids**, not per-repo issue numbers. They are equal in a
/// repository whose issues were all created in it and diverge as soon as one is transferred, and
/// a move posted with the wrong one silently targets a different issue.
pub async fn move_cards(
    rt: &mut Runtime,
    scope: &Scope,
    project: i64,
    column: i64,
    ids: &[i64],
) -> Result<()> {
    let issues: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| serde_json::json!({ "issueID": id, "sorting": i }))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({ "issues": issues }))
        .map_err(|e| Error::new(ErrorKind::Usage(format!("could not build the move: {e}"))))?;
    let path = format!("{}/move", scope.column(project, column));
    expect_ok(rt, Method::POST, &path, WebBody::Json(body)).await
}

/// Send, and turn anything that is not a success into an error.
///
/// Web routes answer a refused write with a JSON `{"message": …}` and a 4xx, or with a redirect
/// back to the page. Neither is an `Err` from the transport's point of view, so without this a
/// failed write would look exactly like a successful one.
async fn expect_ok(rt: &mut Runtime, method: Method, path: &str, body: WebBody) -> Result<()> {
    let resp = crate::web::request(rt, method, path, body).await?;
    if resp.status.is_success() || resp.status.is_redirection() {
        return Ok(());
    }
    let said = serde_json::from_slice::<serde_json::Value>(&resp.body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned));
    Err(Error::new(ErrorKind::Usage(match said {
        Some(m) => format!("Forgejo refused that: {m}"),
        None => format!("Forgejo answered {} to {path}", resp.status),
    })))
}

fn not_found(msg: &str) -> Error {
    Error::new(ErrorKind::Usage(msg.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::project::parse::Column;

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

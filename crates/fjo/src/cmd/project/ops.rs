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

pub async fn delete_column(rt: &mut Runtime, scope: &Scope, id: i64, column: i64) -> Result<()> {
    expect_ok(rt, Method::DELETE, &scope.column(id, column), WebBody::None).await
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

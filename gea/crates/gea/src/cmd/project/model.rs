//! What `gea project` prints, and how it is built from the REST types.
//!
//! These are the shapes `--json` has always emitted, kept field for field when the porcelain
//! moved from scraping the board's web page to Gitea 28's REST API, so a script reading
//! `gea project view X --json` did not notice the move.

/// A board with its columns and cards.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Board {
    pub id: i64,
    pub title: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Column {
    pub id: i64,
    pub title: String,
    /// `None` when the column has no custom colour. The API sends `""` for that, which is
    /// mapped to absence so the field means the same thing it did when it was scraped.
    pub color: Option<String>,
    pub sorting: i64,
    /// Whether this is the board's default column — where issues land when added with no
    /// column, and the one column Gitea refuses to delete.
    pub default: bool,
    pub cards: Vec<Card>,
}

impl Column {
    pub fn new(c: gitea_model::ProjectColumn, cards: Vec<Card>) -> Self {
        Self {
            id: c.id,
            title: c.title,
            color: Some(c.color).filter(|s| !s.is_empty()),
            sorting: c.sorting,
            default: c.default,
            cards,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Card {
    /// The issue's per-repository index — what a user means by "#42".
    pub number: i64,
    /// The issue's global database id — what every project route takes.
    ///
    /// These are equal in a repository whose issues were all created in it, which is exactly
    /// the shape of a test fixture and exactly why the distinction is easy to lose. They diverge
    /// as soon as an issue is transferred in, and a route given the wrong one silently targets
    /// another issue.
    pub id: i64,
    pub title: String,
}

impl From<gitea_model::Issue> for Card {
    fn from(i: gitea_model::Issue) -> Self {
        Self { number: i.number.get(), id: i.id.get(), title: i.title }
    }
}

/// One board as a listing names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectRef {
    pub id: i64,
    pub title: String,
    pub closed: bool,
    /// The board's page, which is what `view --web` opens.
    #[serde(skip)]
    pub html_url: String,
}

impl From<gitea_model::Project> for ProjectRef {
    fn from(p: gitea_model::Project) -> Self {
        // `state`, not the deprecated `is_closed` that carries the same fact.
        let closed = p.state == gitea_model::ProjectState::Closed;
        Self { id: p.id, title: p.title, closed, html_url: p.html_url }
    }
}

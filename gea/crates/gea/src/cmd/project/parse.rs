//! Reading a project board out of the HTML Gitea renders for it.
//!
//! # Why this file exists, and why it is only this file
//!
//! Gitea has no JSON read for a board. Every *write* is a JSON endpoint — the board's own
//! JavaScript posts to them — but `ViewProject` renders `templates/projects/view.tmpl` and that
//! is the only way to learn what is on a board. So one function has to know what that template
//! looks like.
//!
//! **All markup knowledge in the command group lives here.** Nothing else in `cmd::project`
//! contains a tag name, a class or an attribute. That is deliberate: this is the one part of
//! layer 0 pinned to Gitea's *templates* rather than its routes, so it is the part most likely
//! to rot, and when it does the damage should be one file and one test rather than ten commands
//! failing in ten ways.
//!
//! # Why there is no HTML parser here
//!
//! `scraper`/`html5ever` would cost roughly 1.5–2 MB, against a release binary budget
//! (`mise run budget-check`) with about 384 KiB of headroom. A real parser would also buy less
//! than it looks: the template is machine-generated and carries explicit `data-` hooks precisely
//! so its own JavaScript can find these elements, so the thing being matched is a stable
//! attribute, not arbitrary nesting. A scanner over those hooks is the right size of tool.
//!
//! What it is NOT is a general HTML parser, and it must not grow into one. If a future Gitea
//! stops putting the ids in attributes, the answer is a JSON endpoint or a real parser, not a
//! cleverer scanner.
//!
//! # Pinned by a real page
//!
//! `tests/fixtures/projects/view-1.27.3.html` is a board captured from the pinned Gitea image,
//! and `view-1.27.3.facts.json` beside it is what this must produce from it. That pair is the
//! whole reason this can be trusted: it is a recording of the real template, not a guess at it.
//! The index pages and an issue page beside them were captured from the same instance.

use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::web::{VERIFIED_AGAINST, decode_entities};

/// A board, as read from its page.
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
    /// `None` when the column has no custom colour.
    ///
    /// Absence, not an empty string: the template omits the `style` attribute entirely for a
    /// default column, so "no colour" and "colour I could not read" are different facts and
    /// only the first is normal.
    pub color: Option<String>,
    pub sorting: i64,
    /// Whether this is the board's default column — where issues land when added with no column.
    ///
    /// Read from the star the template draws in the default column's title (`octicon-star`),
    /// which it renders for every viewer — unlike the "set as default" action, which only a
    /// writer sees. Knowing it matters because Gitea refuses to delete the default column with a
    /// bare `errors.New`, which `ctx.ServerError` turns into a **500 with no message**. Without
    /// this a user gets "500 Internal Server Error" for something that is entirely their
    /// answerable mistake.
    pub default: bool,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Card {
    /// The issue's per-repository index — what a user means by "#42".
    pub number: i64,
    /// The issue's internal database id — what the move endpoint wants.
    ///
    /// These are equal in a repository whose issues were all created in it, which is exactly
    /// the shape of a test fixture and exactly why the distinction is easy to lose. They
    /// diverge as soon as an issue is transferred in, and a move posted with the wrong one
    /// silently targets another issue.
    pub id: i64,
    pub title: String,
}

/// One board as the index page lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProjectRef {
    pub id: i64,
    pub title: String,
    pub closed: bool,
}

// --------------------------------------------------------------------------- the hooks
//
// Every string the template contributes, in one place, so a template change is a diff in this
// block rather than a hunt through the functions below.

const HEADER_OPEN: &str = r#"project-header">"#;
const TITLE_OPEN: &str = "<h2>";
const COLUMN_OPEN: &str = r#"<div class="project-column""#;
const COLUMN_TITLE: &str = r#"class="project-column-title-text"#;
const DEFAULT_MARK: &str = "octicon-star";
const CARD_ID: &str = r#"data-issue=""#;
const CARD_TITLE: &str = r#"class="issue-card-title"#;
const PROJECT_ID: &str = r#"data-project=""#;
const ISSUE_ID: &str = r#"data-issue-id=""#;
const ISSUE_PROJECTS: &str = r#"name="project_ids" type="hidden" value=""#;
const INDEX_CARD: &str = r#"<div class="item flex-relaxed-list">"#;
const INDEX_LINK: &str = r#"<a class="muted" href=""#;

/// Read a board out of the page Gitea rendered for it.
pub fn parse_board(html: &str) -> Result<Board> {
    // The first `<h2>` after the header's own class, since a bare `<h2>` is not unique to it.
    let title = html
        .split_once(HEADER_OPEN)
        .and_then(|(_, rest)| between(rest, TITLE_OPEN, "</h2>"))
        .map(|t| decode_entities(t.trim()))
        .ok_or_else(|| rotted("the board's title"))?;

    let columns = parse_columns(html)?;

    // The project's own id is on every card container. Falling back to a column's `data-url`
    // (`…/projects/{id}/{columnID}`) covers a board whose columns are all empty, which renders
    // no card container at all — an easy case to never see in a fixture.
    let id = attr_value(html, PROJECT_ID)
        .and_then(|v| v.parse().ok())
        .or_else(|| project_id_from_column_url(html))
        .ok_or_else(|| rotted("the board's id"))?;

    Ok(Board { id, title, columns })
}

/// The internal database id of an issue, from its own page.
///
/// The move endpoint wants this, not the `#42` a user types — they are equal in a repository
/// whose issues were all created in it and diverge as soon as one is transferred in, and a move
/// posted with the wrong one silently targets a different issue.
///
/// Read from the web page rather than from `GET /api/v1/repos/{o}/{r}/issues/{n}` **so that
/// `gea project` needs only a web session**. Using the API here would mean every card command
/// required an API token *as well*, which defeats the point: these routes exist precisely
/// because the API cannot reach them, and asking for a second credential to use the first is a
/// poor bargain. Found by running `card add` with only a session and watching it 404.
pub fn parse_issue_id(html: &str) -> Result<i64> {
    attr_value(html, ISSUE_ID).and_then(|v| v.parse().ok()).ok_or_else(|| rotted("the issue's id"))
}

/// The boards an issue is on, from its own page.
///
/// Gitea lets an issue sit on several boards, and the route that puts it on one *replaces* the
/// issue's whole set: posting board 2 for an issue on board 1 takes it off board 1. So adding a
/// card has to send what is already there as well, and this is where "what is already there"
/// comes from — the sidebar's hidden `project_ids`, which is the same list the page's own
/// picker posts back.
pub fn parse_issue_projects(html: &str) -> Result<Vec<i64>> {
    let raw = attr_value(html, ISSUE_PROJECTS).ok_or_else(|| rotted("the issue's boards"))?;
    raw.split(',')
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().parse().map_err(|_| rotted("the issue's boards")))
        .collect()
}

/// Read the boards out of `/{owner}/{repo}/projects`.
///
/// The index is a separate template from the board itself (`projects/list.tmpl`), so it gets its
/// own function rather than a flag on [`parse_board`] — they share no markup beyond the id
/// living in a URL.
///
/// State comes from *which* action link the template rendered, not from a class: an open board
/// offers `…/close` and a closed one offers `…/open`. That is a `{{if .IsClosed}}` in the
/// template, so it is exactly as reliable as the field itself.
pub fn parse_index(html: &str) -> Result<Vec<ProjectRef>> {
    let mut out = Vec::new();
    for card in html.split(INDEX_CARD).skip(1) {
        let href = match between(card, INDEX_LINK, "\"") {
            Some(h) => h,
            // A `milestone-card` with no project link is not a board row. Skipping is right
            // here (unlike a card with no title, which would under-report a board): the class
            // is shared with the milestone list, so a stray match is expected, not corruption.
            None => continue,
        };
        let Some(id) =
            href.trim_end_matches('/').rsplit('/').next().and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        let title = between(card, &format!("{INDEX_LINK}{href}\">"), "</a>")
            .map(|t| decode_entities(t.trim()))
            .ok_or_else(|| rotted("a board's title"))?;
        // `…/open` offered means it is currently closed, and vice versa.
        let closed = card.contains(&format!("data-url=\"{href}/open\""));
        out.push(ProjectRef { id, title, closed });
    }
    Ok(out)
}

fn parse_columns(html: &str) -> Result<Vec<Column>> {
    let mut columns = Vec::new();
    // `split` rather than a running cursor: each column's markup runs to the next column's open
    // tag, so the chunks are exactly the per-column regions, and the first piece is the page
    // header, which is discarded.
    for chunk in html.split(COLUMN_OPEN).skip(1) {
        let open_tag = chunk.split_once('>').map_or(chunk, |(t, _)| t);
        let id = attr_value(open_tag, r#"data-id=""#)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| rotted("a column's id"))?;
        let sorting = attr_value(open_tag, r#"data-sorting=""#)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| rotted("a column's position"))?;
        // The title is the text of its `div`, after the star the default column carries. Read
        // from the text rather than the edit menu's `data-modal-…-title-input`, because only a
        // writer is shown that menu.
        let title_div = between(chunk, COLUMN_TITLE, "</div>")
            .and_then(|d| d.split_once('>'))
            .map(|(_, inner)| inner)
            .ok_or_else(|| rotted("a column's title"))?;
        let default = title_div.contains(DEFAULT_MARK);
        let text = title_div.rsplit_once("</svg>").map_or(title_div, |(_, t)| t);
        let title = decode_entities(text.trim());
        columns.push(Column {
            id,
            title,
            color: colour_of(open_tag),
            sorting,
            default,
            cards: cards_in(chunk)?,
        });
    }
    if columns.is_empty() && html.contains(COLUMN_OPEN) {
        return Err(rotted("any column"));
    }
    Ok(columns)
}

/// `style="background: #1f883d !important; color: #fff !important"` -> `#1f883d`.
///
/// Returns `None` for a column with no `style` at all, which is what an uncoloured column looks
/// like, and also for a `style` this does not understand — both mean "no colour to show", and
/// neither is worth failing a whole board over.
fn colour_of(open_tag: &str) -> Option<String> {
    let style = attr_value(open_tag, r#"style=""#)?;
    let after = style.split_once("background:")?.1.trim_start();
    let value = after.split([';', ' ']).next()?.trim();
    value.starts_with('#').then(|| value.to_owned())
}

fn cards_in(chunk: &str) -> Result<Vec<Card>> {
    let mut cards = Vec::new();
    // Split on the id attribute rather than on the card's class: `issue-card` is a PREFIX of
    // `issue-card-icon` and `issue-card-title`, so matching the class would find three elements
    // per card and two of them have no issue on them.
    for piece in chunk.split(CARD_ID).skip(1) {
        let id = piece
            .split_once('"')
            .and_then(|(v, _)| v.parse().ok())
            .ok_or_else(|| rotted("a card's issue id"))?;
        let anchor = match piece.split_once(CARD_TITLE) {
            Some((_, rest)) => rest,
            // A card element with no title link is not a card this can describe. Failing is
            // right: silently dropping it would under-report a board, and a user acting on a
            // board that is missing a card is worse than an error.
            None => return Err(rotted("a card's title link")),
        };
        let href = attr_value(anchor, r#"href=""#).ok_or_else(|| rotted("a card's issue link"))?;
        let number = href
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| rotted("a card's issue number"))?;
        let title = between(anchor, ">", "</a>")
            .map(|t| decode_entities(t.trim()))
            .ok_or_else(|| rotted("a card's title"))?;
        cards.push(Card { number, id, title });
    }
    Ok(cards)
}

fn project_id_from_column_url(html: &str) -> Option<i64> {
    let url = attr_value(html.split(COLUMN_OPEN).nth(1)?, r#"data-url=""#)?;
    let after = url.split_once("/projects/")?.1;
    after.split('/').next()?.parse().ok()
}

/// The text between two markers, starting after the first.
fn between<'a>(hay: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let rest = hay.split_once(open)?.1;
    Some(rest.split_once(close)?.0)
}

/// The value of an attribute written exactly as `name="`, up to the closing quote.
fn attr_value<'a>(hay: &'a str, name: &str) -> Option<&'a str> {
    between(hay, name, "\"")
}

/// The board page did not have the shape this was written against.
///
/// Names the version this was verified against, because that is the one moment the pin is worth
/// mentioning: a board that parses fine needs no commentary about which Gitea it came from.
/// The caller adds the server's own version, which it can only learn by asking.
fn rotted(what: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "could not find {what} on the board page. `gea project` reads Gitea's own board \
         template, which is not a documented API and changes between releases; it was verified \
         against Gitea {VERIFIED_AGAINST}. Use `gea web GET <owner>/<repo>/projects/<id>` to \
         see the page itself."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real page, and the real answer.
    ///
    /// Asserts the WHOLE structure against the recorded ground truth rather than probing for a
    /// substring: a scanner that found one column and gave up would satisfy almost any looser
    /// check, and "it parsed" is not the property that matters.
    #[test]
    fn the_captured_board_parses_to_its_recorded_facts() {
        let html = include_str!("../../../tests/fixtures/projects/view-1.27.3.html");
        let facts: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/projects/view-1.27.3.facts.json"
        ))
        .expect("the ground truth is valid JSON");

        let got = parse_board(html).expect("the captured page parses");
        let want = serde_json::to_value(&got).expect("a board serialises");

        assert_eq!(want["id"], facts["project"]["id"]);
        assert_eq!(want["title"], facts["project"]["title"]);
        assert_eq!(
            want["columns"], facts["columns"],
            "the parsed columns must match the recorded ground truth exactly"
        );
    }

    /// Bug this prevents: showing a user `Fix &lt;script&gt; &amp; &#34;quotes&#34;`, which looks
    /// almost right and is wrong in a way that would survive review.
    #[test]
    fn a_card_title_is_entity_decoded() {
        let html = include_str!("../../../tests/fixtures/projects/view-1.27.3.html");
        let board = parse_board(html).expect("parses");
        let title = board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .map(|c| c.title.as_str())
            .find(|t| t.contains("script"))
            .expect("the fixture carries an entity-bearing title");
        assert_eq!(title, r#"Fix <script> & "quotes""#);
    }

    /// Bug this prevents: reading `issue-card-icon` and `issue-card-title` as cards, because
    /// `issue-card` is a prefix of both. That would treble every column's card count.
    #[test]
    fn card_counting_is_not_confused_by_the_prefixed_class_names() {
        let html = include_str!("../../../tests/fixtures/projects/view-1.27.3.html");
        let board = parse_board(html).expect("parses");
        let total: usize = board.columns.iter().map(|c| c.cards.len()).sum();
        assert_eq!(total, 3, "the fixture has exactly three cards");
        assert!(
            board.columns.iter().any(|c| c.cards.is_empty()),
            "and at least one empty column, which must parse as empty rather than fail"
        );
    }

    /// The index pages, captured from the pinned image — one listing open boards and one
    /// closed, because state is the thing with no class to read: an open board offers
    /// `…/close` and a closed one offers `…/open`.
    #[test]
    fn the_index_yields_each_board_with_its_id_and_state() {
        let open = include_str!("../../../tests/fixtures/projects/index-open-1.27.3.html");
        let got = parse_index(open).expect("parses");
        assert_eq!(
            got,
            [ProjectRef { id: 2, title: "Fixture Kanban Board".to_owned(), closed: false }]
        );

        let closed = include_str!("../../../tests/fixtures/projects/index-closed-1.27.3.html");
        let got = parse_index(closed).expect("parses");
        assert_eq!(got, [ProjectRef { id: 1, title: "Roadmap".to_owned(), closed: true }]);
    }

    /// Entity decoding on the index, which the captured pages happen not to exercise.
    #[test]
    fn an_index_title_is_entity_decoded() {
        let html = r##"<div class="item flex-relaxed-list">
          <a class="muted" href="/geatest/r/projects/4">Ship &amp; tell</a>
          <a class="link-action flex-text-inline" href data-url="/geatest/r/projects/4/open">Open</a>
        </div>"##;
        let got = parse_index(html).expect("parses");
        assert_eq!(got[0].title, "Ship & tell");
        assert!(got[0].closed);
    }

    /// Bug this prevents: a list item with no board link producing a phantom entry.
    #[test]
    fn an_item_that_is_not_a_board_is_skipped_rather_than_invented() {
        let html = r##"<div class="item flex-relaxed-list"><a href="/geatest/r/milestones/1">1.0</a></div>"##;
        assert_eq!(parse_index(html).expect("parses").len(), 0);
    }

    /// Bug this prevents: `card add` taking an issue off every other board it was on, because
    /// Gitea's route replaces the set and the current set was never read.
    #[test]
    fn an_issue_page_yields_its_id_and_every_board_it_is_on() {
        let html = include_str!("../../../tests/fixtures/projects/issue-1.27.3.html");
        assert_eq!(parse_issue_id(html).expect("id"), 1);
        assert_eq!(parse_issue_projects(html).expect("boards"), [1, 2]);
        let none = r#"<input class="combo-value" name="project_ids" type="hidden" value="">"#;
        assert_eq!(parse_issue_projects(none).expect("none"), Vec::<i64>::new());
    }

    /// The default column is the one with a star in its title. Getting this wrong would let
    /// `column delete` send a request Gitea answers with an unexplained 500.
    #[test]
    fn the_default_column_is_the_starred_one() {
        let html = include_str!("../../../tests/fixtures/projects/view-1.27.3.html");
        let board = parse_board(html).expect("parses");
        let defaults: Vec<&str> =
            board.columns.iter().filter(|c| c.default).map(|c| c.title.as_str()).collect();
        assert_eq!(defaults, ["Backlog"], "exactly one column is the default");
    }

    /// Absence of a colour and an unreadable colour are different, and only the first is normal.
    #[test]
    fn a_colour_is_read_only_when_the_template_set_one() {
        assert_eq!(
            colour_of(
                r#" style="background: #1f883d !important; color: #fff !important" data-id="5""#
            ),
            Some("#1f883d".to_owned())
        );
        assert_eq!(colour_of(r#" data-id="1" data-sorting="0""#), None);
        assert_eq!(colour_of(r#" style="color: #fff""#), None);
    }

    /// A page that is not a board fails with advice, rather than yielding an empty board that a
    /// user would read as "this board has nothing on it".
    #[test]
    fn a_page_that_is_not_a_board_is_an_error_not_an_empty_board() {
        let err = parse_board("<html><body>signed out</body></html>").expect_err("refuses");
        let msg = err.to_string();
        assert!(msg.contains("1.27.3"), "the error should name the verified version: {msg}");
    }
}

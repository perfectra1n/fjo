//! Reading a project board out of the HTML Forgejo renders for it.
//!
//! # Why this file exists, and why it is only this file
//!
//! Forgejo has no JSON read for a board. Every *write* is a JSON endpoint — the board's own
//! JavaScript posts to them — but `ViewProject` renders `templates/projects/view.tmpl` and that
//! is the only way to learn what is on a board. So one function has to know what that template
//! looks like.
//!
//! **All markup knowledge in the command group lives here.** Nothing else in `cmd::project`
//! contains a tag name, a class or an attribute. That is deliberate: this is the one part of
//! layer 0 pinned to Forgejo's *templates* rather than its routes, so it is the part most likely
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
//! What it is NOT is a general HTML parser, and it must not grow into one. If a future Forgejo
//! stops putting the ids in attributes, the answer is a JSON endpoint or a real parser, not a
//! cleverer scanner.
//!
//! # Pinned by a real page
//!
//! `tests/fixtures/projects/view-16.0.5.html` is a board captured from the pinned Forgejo image,
//! and `view-16.0.5.facts.json` beside it is what this must produce from it. That pair is the
//! whole reason this can be trusted: it is a recording of the real template, not a guess at it.

use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::web::{VERIFIED_AGAINST, decode_entities};

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

// --------------------------------------------------------------------------- the hooks
//
// Every string the template contributes, in one place, so a template change is a diff in this
// block rather than a hunt through the functions below.

const TITLE_OPEN: &str = r#"<h2 class="tw-mb-0 tw-flex-1 tw-break-anywhere">"#;
const COLUMN_OPEN: &str = r#"<div class="project-column""#;
const COLUMN_TITLE: &str = r#"project-column-title-label">"#;
const CARD_ID: &str = r#"data-issue=""#;
const CARD_TITLE: &str = r#"class="issue-card-title"#;
const PROJECT_ID: &str = r#"data-project=""#;

/// Read a board out of the page Forgejo rendered for it.
pub fn parse_board(html: &str) -> Result<Board> {
    let title = between(html, TITLE_OPEN, "</h2>")
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
        let title = between(chunk, COLUMN_TITLE, "<")
            .map(|t| decode_entities(t.trim()))
            .ok_or_else(|| rotted("a column's title"))?;

        columns.push(Column {
            id,
            title,
            color: colour_of(open_tag),
            sorting,
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
/// mentioning: a board that parses fine needs no commentary about which Forgejo it came from.
/// The caller adds the server's own version, which it can only learn by asking.
fn rotted(what: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "could not find {what} on the board page. `fjo project` reads Forgejo's own board \
         template, which is not a documented API and changes between releases; it was verified \
         against Forgejo {VERIFIED_AGAINST}. Use `fjo web GET <owner>/<repo>/projects/<id>` to \
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
        let html = include_str!("../../../tests/fixtures/projects/view-16.0.5.html");
        let facts: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/projects/view-16.0.5.facts.json"
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
        let html = include_str!("../../../tests/fixtures/projects/view-16.0.5.html");
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
        let html = include_str!("../../../tests/fixtures/projects/view-16.0.5.html");
        let board = parse_board(html).expect("parses");
        let total: usize = board.columns.iter().map(|c| c.cards.len()).sum();
        assert_eq!(total, 3, "the fixture has exactly three cards");
        assert!(
            board.columns.iter().any(|c| c.cards.is_empty()),
            "and at least one empty column, which must parse as empty rather than fail"
        );
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
        assert!(msg.contains("16.0.5"), "the error should name the verified version: {msg}");
    }
}

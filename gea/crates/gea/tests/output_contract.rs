//! Integration tests for the output system's *public* contract.
//!
//! These deliberately go through `gea::output`'s exported API only, so they fail if a type the
//! command layer needs stops being reachable — something the in-module unit tests cannot catch,
//! since they can see private items.
//!
//! The contract under test is the one shell scripts depend on: the same data, through the same
//! pipeline, must produce a human layout on a TTY and real TSV when piped.

use gea::output::{Dest, FieldKind, FieldSpec, Filter, Pipeline, Selection, Table, Template, Term};
use serde_json::{Value, json};

const FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "number", kind: FieldKind::Int, doc: "Index within the repository" },
    FieldSpec { name: "title", kind: FieldKind::Str, doc: "Title" },
    FieldSpec { name: "state", kind: FieldKind::Enum(&["open", "closed"]), doc: "State" },
    FieldSpec { name: "html_url", kind: FieldKind::Str, doc: "Web URL" },
];

/// Deliberately includes a row with an empty cell and a row with CJK, because those are the two
/// cases where the TTY and piped paths are most likely to disagree.
fn data() -> Value {
    json!([
        {"number": 1, "title": "add a thing", "state": "open",
         "html_url": "https://forge.example/o/r/pulls/1"},
        {"number": 22, "title": "", "state": "closed",
         "html_url": "https://forge.example/o/r/pulls/22"},
        {"number": 333, "title": "日本語のタイトル", "state": "merged",
         "html_url": "https://forge.example/o/r/pulls/333"}
    ])
}

fn render(pipeline: &Pipeline, term: &Term) -> String {
    let mut buf = Vec::new();
    pipeline.render(data(), term, &mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

/// Bug this prevents: the two rendering paths drifting apart, so `gea pr list | cut -f2`
/// returns padded text, a header row, or a truncated title.
#[test]
fn the_same_data_renders_as_a_table_on_a_tty_and_as_tsv_when_piped() {
    let template = Template::parse("{{range .}}{{tablerow .number .state .title}}{{end}}").unwrap();
    let pipeline = Pipeline::new().template(Some(&template));

    let tty = render(&pipeline, &Term::tty(80));
    assert_eq!(
        tty,
        concat!("1    open    add a thing\n", "22   closed\n", "333  merged  日本語のタイトル\n",)
    );

    let piped = render(&pipeline, &Term::piped());
    assert_eq!(
        piped,
        concat!("1\topen\tadd a thing\n", "22\tclosed\t\n", "333\tmerged\t日本語のタイトル\n",)
    );

    // The piped form must be parseable as TSV with a stable field count, including the empty
    // cell on row 2. This is the actual promise: `cut -f3` works on every row.
    let rows: Vec<Vec<&str>> = piped.lines().map(|l| l.split('\t').collect()).collect();
    assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3, 3]);
    assert_eq!(rows[1][2], "", "the empty cell must survive as an empty field");
    assert_eq!(rows[2][2], "日本語のタイトル");
}

/// Bug this prevents: padding computed from byte or `char` counts, which leaves the third
/// column ragged as soon as a title contains CJK or an emoji.
#[test]
fn wide_characters_do_not_break_column_alignment() {
    let mut table = Table::new(&Term::tty(80));
    table.headers(["A", "B"]);
    table.row(["日本語", "1"]);
    table.row(["ab", "2"]);
    table.row(["🚀x", "3"]);
    let out = table.render_to_string();
    // The second column must begin at the same *display* column on every row. Byte offsets
    // would differ here by construction, which is the whole point of the test.
    let column_starts: Vec<usize> = out
        .lines()
        .map(|line| {
            let (before, _) = line.rsplit_once("  ").expect(line);
            gea::output::display_width(before) + 2
        })
        .collect();
    assert_eq!(column_starts, vec![8, 8, 8, 8], "{out}");
}

/// Bug this prevents: field discovery needing a network call. It runs on the field table alone,
/// which is what lets `gea pr list --json` answer without auth or connectivity.
#[test]
fn field_discovery_is_offline_and_exits_successfully() {
    assert_eq!(gea::output::project::resolve("", FIELDS).unwrap(), Selection::Discover);

    let mut piped = Vec::new();
    gea::output::project::write_field_list(FIELDS, &Term::piped(), &mut piped).unwrap();
    assert_eq!(String::from_utf8(piped).unwrap(), "number\ntitle\nstate\nhtml_url\n");

    let mut tty = Vec::new();
    gea::output::project::write_field_list(FIELDS, &Term::tty(100), &mut tty).unwrap();
    let tty = String::from_utf8(tty).unwrap();
    assert!(tty.contains("enum(open|closed)"), "{tty}");
    assert!(tty.contains("Index within the repository"), "{tty}");
}

/// Bug this prevents: the transform stages being reordered. `--json` narrows first, so the
/// filter only ever sees the projected document.
#[test]
fn projection_runs_before_jq() {
    let filter = Filter::compile("[.[] | keys] | unique").unwrap();
    let fields = vec!["number".to_string(), "state".to_string()];
    let pipeline = Pipeline::new().fields(Some(&fields)).jq(Some(&filter));
    assert_eq!(render(&pipeline, &Term::piped()), "[[\"number\",\"state\"]]\n");
}

/// Bug this prevents: dumping an artifact's bytes into the user's terminal, which can leave it
/// needing `reset`.
#[test]
fn binary_output_to_a_terminal_is_refused() {
    let err =
        gea::output::guard_binary("application/octet-stream", &Term::tty(80), &Dest::Stdout, false)
            .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(
        gea::output::guard_binary("application/octet-stream", &Term::tty(80), &Dest::Stdout, true)
            .is_ok()
    );
}

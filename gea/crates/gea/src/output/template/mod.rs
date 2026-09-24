//! `--template`: a hand-written Go-template subset.
//!
//! # Why hand-written
//!
//! A Go-template crate would bring the *whole* language — `define`/`block`/`template`,
//! `with`, Go's full `printf` verb and flag set, comparison functions, and reflection
//! semantics that only make sense over Go values. `gh` exposes a small, documented subset, and
//! users copy `gh` templates verbatim. Implementing exactly that subset over
//! `serde_json::Value` is roughly 700 lines including tests, and it means every construct that
//! parses is a construct we have tested, rather than a construct that might work depending on
//! how the crate maps Go's reflection onto JSON.
//!
//! # The subset
//!
//! `{{ }}` actions with `{{-` / `-}}` whitespace trimming · `.field` and `.a.b` paths (a
//! numeric segment indexes an array) · `$`, `$var`, and `:=` assignment · string, number,
//! `true`/`false`/`nil` literals · pipelines (`a | f x`, Go's "piped value is the last
//! argument" rule) · `if`/`else`/`end` · `range`/`else`/`end` with one or two variables ·
//! parenthesized sub-pipelines · the eleven functions in [`funcs::FUNCTIONS`], including
//! `printf` over the verbs in [`funcs::PRINTF_VERBS`].
//!
//! Missing data yields the empty string; only template *authoring* mistakes are errors. See
//! [`eval`].

pub mod eval;
pub mod funcs;
pub mod lex;
pub mod parse;

use gitea_core::Error;
use serde_json::Value;

use super::tty::Term;

/// A parsed template, ready to render many values.
///
/// Parsed once and reused: with `--paginate` the template is applied per page, and reparsing
/// would repay the cost on every HTTP round trip and could report a syntax error on page 7
/// after six pages had printed.
#[derive(Debug, Clone)]
pub struct Template {
    nodes: Vec<parse::Node>,
    source: String,
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, Error> {
        Ok(Self { nodes: parse::parse(source)?, source: source.to_string() })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Render one value.
    ///
    /// Table rows buffered by `tablerow` are flushed at the end even if the template never
    /// calls `tablerender`. See [`eval::render`].
    pub fn render(&self, data: &Value, term: &Term) -> Result<String, Error> {
        eval::render(&self.nodes, data, term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::tty::MapEnv;
    use serde_json::json;

    fn prs() -> Value {
        json!([
            {"number": 1, "title": "add the widget subsystem, which is quite a long title",
             "state": "open", "draft": false, "html_url": "https://forge.example/o/r/pulls/1",
             "labels": [{"name": "bug"}, {"name": "ui"}], "created_at": "2024-06-01T15:04:05Z"},
            {"number": 22, "title": "fix", "state": "merged", "draft": true,
             "html_url": "https://forge.example/o/r/pulls/22",
             "labels": [], "created_at": "2024-06-01T09:00:00Z"}
        ])
    }

    fn render(src: &str, term: &Term) -> String {
        Template::parse(src).expect(src).render(&prs(), term).expect(src)
    }

    /// Bug this prevents: a template that builds rows but never calls `{{tablerender}}`
    /// exiting 0 with no output, which reads exactly like "the API returned nothing".
    #[test]
    fn tablerow_without_tablerender_auto_flushes() {
        let src = "{{range .}}{{tablerow .number .state .title}}{{end}}";
        let with = "{{range .}}{{tablerow .number .state .title}}{{end}}{{tablerender}}";
        let term = Term::tty(80);
        assert_eq!(render(src, &term), render(with, &term));
        assert!(render(src, &term).contains("open"));
    }

    /// Bug this prevents: `tablerender` leaving the buffer full, so a second table repeats the
    /// first table's rows.
    #[test]
    fn tablerender_clears_the_buffer() {
        let src = "{{range .}}{{tablerow .number}}{{end}}{{tablerender}}--\n{{tablerender}}";
        assert_eq!(render(src, &Term::tty(80)), "1\n22\n--\n");
    }

    /// Bug this prevents: `tablerender` growing its own width algorithm, so the same data
    /// renders differently under `--template` than under the default human view.
    #[test]
    fn tablerender_uses_the_same_renderer_as_the_default_view() {
        let src = "{{range .}}{{tablerow .number .state}}{{end}}";
        // On a TTY: padded columns.
        insta::assert_snapshot!(render(src, &Term::tty(80)), @r"
        1   open
        22  merged
        ");
        // Piped: TSV, no padding.
        assert_eq!(render(src, &Term::piped()), "1\topen\n22\tmerged\n");
    }

    /// Bug this prevents: `tablerow` printing its arguments as it goes, so the row appears
    /// twice (once inline, once in the table).
    #[test]
    fn tablerow_itself_prints_nothing() {
        let src = "[{{tablerow \"a\" \"b\"}}]";
        assert_eq!(render(src, &Term::piped()), "[]a\tb\n");
    }

    /// Bug this prevents: `{{-` / `-}}` not trimming, so a `range` written readably across
    /// several lines emits a blank line and two spaces of indentation per iteration.
    #[test]
    fn whitespace_trimming() {
        let trimmed = "{{range . -}}\n  {{.number}}\n{{- end}}";
        assert_eq!(render(trimmed, &Term::piped()), "122");
        // Without the markers, the template's own layout leaks into the output.
        let untrimmed = "{{range .}}\n  {{.number}}\n{{end}}";
        assert_eq!(render(untrimmed, &Term::piped()), "\n  1\n\n  22\n");
    }

    /// Every template function, in one golden, on a TTY with color forced so the styling
    /// functions actually emit something.
    ///
    /// Bug this prevents: a function silently changing shape (arity, argument order, output)
    /// during a refactor. Argument order is the fragile part — `truncate 30 .title` and
    /// `.title | truncate 30` must agree.
    #[test]
    fn all_functions() {
        let term = Term { tty: true, width: 80, color: true, hyperlinks: true };
        let src = concat!(
            "truncate:   [{{truncate 10 (index_placeholder)}}]\n",
            "truncate-p: [{{(index_placeholder) | truncate 10}}]\n",
            "join:       [{{.labels | pluck \"name\" | join \",\"}}]\n",
            "pluck:      [{{pluck \"name\" .labels}}]\n",
            "color:      [{{color \"bold+green\" .state}}]\n",
            "autocolor:  [{{autocolor .state}}]\n",
            "autocolor2: [{{autocolor \"cyan\" .state}}]\n",
            "hyperlink:  [{{hyperlink .html_url .title}}]\n",
            "timefmt:    [{{timefmt \"2006-01-02\" .created_at}}]\n",
            "printf:     [{{printf \"#%v %s (%d%%)\" .number .state .number}}]\n",
            "printf-p:   [{{printf \"#%v\" .number | autocolor \"green\"}}]\n",
            "tablerow:   [{{tablerow .number .state}}]{{tablerender}}",
        )
        .replace("(index_placeholder)", ".title");
        let out = Template::parse(&src)
            .unwrap()
            .render(&prs()[0], &term)
            .unwrap()
            // Escapes are replaced with visible markers so the golden stays readable and a
            // change to *which* escape is emitted still shows up in the diff.
            .replace('\x1b', "<ESC>");
        insta::assert_snapshot!(out);
    }

    /// The `--template` example from `docs/output.md`, **verbatim**.
    ///
    /// Bug this prevents: documentation rot. This exact expression was the reason `printf` had
    /// to exist at all — without it the headline example in our own docs failed with "unknown
    /// function". Copy any change to the doc into this string, or delete it from the doc.
    ///
    /// It also exercises Go's last-argument pipe rule against a real case: `printf` produces
    /// `#1`, and `| autocolor "green"` receives *that*, not `.number`.
    #[test]
    fn the_documented_template_example_works() {
        // docs/output.md, "## `--template`" — the argument to --template, unchanged.
        // `r##` because the expression itself contains `"#`.
        const DOC_EXAMPLE: &str = r##"{{range .}}{{tablerow (printf "#%v" .number | autocolor "green") .title .head_branch (timeago .updated_at)}}{{end}}"##;

        // Shaped like the `--json number,title,head_branch,updated_at` the doc pairs it with,
        // including the projected `null` for a field the server did not send.
        let data = json!([
            {"number": 1, "title": "add a thing", "head_branch": "feat/a",
             "updated_at": "2024-06-01T12:00:00Z"},
            {"number": 22, "title": "fix a thing", "head_branch": "fix/b", "updated_at": null}
        ]);

        let piped = Template::parse(DOC_EXAMPLE).unwrap().render(&data, &Term::piped()).unwrap();
        let rows: Vec<Vec<&str>> = piped.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.len(), 2, "{piped}");
        assert_eq!(rows[0][0], "#1", "printf's result, not .number");
        assert_eq!(rows[0][1], "add a thing");
        assert_eq!(rows[0][2], "feat/a");
        assert!(rows[0][3].ends_with(" ago"), "{:?}", rows[0][3]);
        // An absent timestamp yields an empty cell, and the cell is still there.
        assert_eq!(rows[1], vec!["#22", "fix a thing", "fix/b", ""]);

        // On a TTY the same expression colors the first column and pads instead of tabbing.
        let term = Term { tty: true, width: 100, color: true, hyperlinks: false };
        let tty = Template::parse(DOC_EXAMPLE).unwrap().render(&data, &term).unwrap();
        assert!(tty.contains("\x1b[32m#1\x1b[0m"), "{tty:?}");
        assert!(!tty.contains('\t'), "{tty:?}");
    }

    /// The `--jq` example from `docs/output.md` (the camelCase workaround offered in place of a
    /// `--json-case` flag), **verbatim**.
    ///
    /// Bug this prevents: shipping a documented `--jq` one-liner that `jaq` cannot even compile.
    /// It is long enough that nobody would notice by reading, and it is the answer we give to
    /// every user who asks for camelCase.
    #[test]
    fn the_documented_jq_camel_case_example_works() {
        // docs/output.md, "## Divergence 1" — the argument to --jq, unchanged.
        const DOC_EXAMPLE: &str = r#"map(with_entries(.key |= (split("_") | .[0] + (.[1:] | map(. | ascii_upcase[0:1] + .[1:]) | join(""))))) "#;

        let filter =
            crate::output::Filter::compile(DOC_EXAMPLE).expect("the doc example must compile");
        let data = json!([{"number": 1, "head_branch": "feat/a", "html_url": "u"}]);
        let out = filter.run(&data).unwrap();
        assert_eq!(
            serde_json::to_string(&out[0]).unwrap(),
            r#"[{"number":1,"headBranch":"feat/a","htmlUrl":"u"}]"#
        );
    }

    /// `timeago` is exercised separately because it reads the clock; the bucket boundaries are
    /// pinned by `funcs::timeago_between`. Here we only assert the plumbing: a real timestamp
    /// produces a relative phrase, and an unset one produces nothing.
    #[test]
    fn timeago_plumbing() {
        let t = Template::parse("[{{timeago .a}}][{{timeago .b}}]").unwrap();
        let data = json!({"a": "2020-01-01T00:00:00Z", "b": "0001-01-01T00:00:00Z"});
        let out = t.render(&data, &Term::piped()).unwrap();
        assert!(out.ends_with("ago][]"), "{out}");
    }

    /// Bug this prevents: an unknown function producing a bare "template error" with no clue
    /// which function was wrong or what is available.
    #[test]
    fn unknown_function_lists_what_exists() {
        let err =
            Template::parse("{{nope .x}}").unwrap().render(&json!({}), &Term::piped()).unwrap_err();
        let gitea_core::ErrorKind::Template { message, .. } = &*err.kind else { panic!() };
        assert!(message.contains("nope"), "{message}");
        assert!(message.contains("tablerender"), "{message}");
    }

    /// Bug this prevents: an arity mistake being absorbed (extra arguments ignored, missing
    /// ones defaulted), which turns a typo into wrong output instead of a message.
    #[test]
    fn arity_errors_name_the_function() {
        let err = Template::parse("{{truncate .x}}")
            .unwrap()
            .render(&json!({}), &Term::piped())
            .unwrap_err();
        let gitea_core::ErrorKind::Template { message, .. } = &*err.kind else { panic!() };
        assert!(message.contains("truncate takes 2"), "{message}");
    }

    /// Bug this prevents: styling functions emitting escapes when color is off, which would
    /// put them into TSV output.
    #[test]
    fn functions_respect_no_color() {
        let term = Term::detect_with(&MapEnv::new().with("NO_COLOR", "1"), true, Some(80));
        let out = Template::parse("{{color \"green\" .state}}{{autocolor .state}}")
            .unwrap()
            .render(&prs()[0], &term)
            .unwrap();
        assert_eq!(out, "openopen");
    }
}

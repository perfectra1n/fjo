//! `--jq`, and the only place in the tree that names a `jaq` type.
//!
//! # Why the facade
//!
//! `jaq`'s crate split has churned across releases: the value type moved out to `jaq-json`,
//! the standard library to `jaq-std`, `Ctx` gained a data-kind type parameter, and the
//! loader/compiler pipeline was reshaped more than once. Every one of those changes is a
//! mechanical edit *here* and nowhere else, because the rest of `gea` sees exactly two
//! methods: [`Filter::compile`] and [`Filter::run`]. Nothing outside this module mentions
//! `Val`, `Ctx`, `Loader`, `Arena`, or `DataT`.
//!
//! # Compile once
//!
//! [`Filter`] is `Send`-able owned state with no borrow of the source text (`jaq`'s compiled
//! `Filter` interns everything it needs), so one compile is reused for every page of a
//! `--paginate` run. Recompiling per page would repay the parser cost on every HTTP round
//! trip and, worse, could report a compile error on page 7 after six pages had already been
//! printed.
//!
//! # Output rules
//!
//! Copied from `jq` (and therefore from `gh --jq`), because these are what shell pipelines
//! are written against:
//!
//! * a **string** result prints raw and unquoted — `--jq '.title'` must not print `"hi"`,
//!   or every user has to pipe through `tr -d '"'`;
//! * everything else prints as **compact JSON**;
//! * **multiple results print one per line**, which is what makes `--jq '.[].number'`
//!   equivalent to a list of numbers.

use std::io::{self, Write};

use gitea_core::{Error, ErrorKind};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_json::Val;
use serde_json::Value;

use super::color::display_width;

/// A compiled `--jq` program.
///
/// The whole `jaq` surface `gea` uses is the two inherent methods below.
pub struct Filter {
    /// `jaq_core::Filter` is `compile::Filter<Native<D>>` and owns its program graph — it
    /// borrows neither the source text nor the arena the loader used, which is what lets this
    /// struct be stored and reused.
    program: jaq_core::Filter<data::JustLut<Val>>,
    expr: String,
}

impl std::fmt::Debug for Filter {
    /// Hand-written because `jaq`'s compiled program has no useful `Debug` and printing it
    /// would dump the entire term table into a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filter").field("expr", &self.expr).finish_non_exhaustive()
    }
}

impl Filter {
    /// Compile a jq expression, or report [`ErrorKind::JqCompile`].
    pub fn compile(expr: &str) -> Result<Self, Error> {
        let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
        let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());

        // The arena backs the module strings during loading only; the compiled program is
        // independent of it, so it can be a local.
        let arena = Arena::default();
        let program = File { code: expr, path: () };

        let modules =
            Loader::new(defs).load(&arena, program).map_err(|errs| load_error(expr, &errs))?;
        let program = Compiler::default()
            .with_funs(funs)
            .compile(modules)
            .map_err(|errs| compile_error(expr, &errs))?;

        Ok(Self { program, expr: expr.to_string() })
    }

    /// Run the filter over one input document.
    ///
    /// Returns *all* outputs: a jq program is a stream transformer, and `.[]` over a
    /// ten-element array legitimately produces ten results.
    pub fn run(&self, input: &Value) -> Result<Vec<Value>, Error> {
        let ctx = Ctx::<data::JustLut<Val>>::new(&self.program.lut, Vars::new([]));
        let mut out = Vec::new();
        for result in self.program.id.run((ctx, to_val(input))).map(unwrap_valr) {
            match result {
                Ok(v) => out.push(from_val(&v)),
                // A runtime failure (`"x" + 1`) is reported as a usage error rather than
                // `JqCompile`: the expression compiled fine, so a caret pointing into it
                // would be misleading. Both map to exit code 2 either way.
                Err(e) => {
                    return Err(Error::new(ErrorKind::Usage(format!(
                        "--jq {:?} failed while running: {e}",
                        self.expr
                    ))));
                }
            }
        }
        Ok(out)
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }
}

/// Write jq results using jq's own output rules. See the module docs.
pub fn write_results(results: &[Value], out: &mut impl Write) -> io::Result<()> {
    for value in results {
        match value {
            Value::String(s) => writeln!(out, "{s}")?,
            other => writeln!(out, "{}", serde_json::to_string(other).expect("JSON is writable"))?,
        }
    }
    Ok(())
}

/// Render a compile error with a caret under the offending column.
///
/// `col` is a **1-based display column**, and the caret is placed by summing display widths
/// rather than bytes so that a multi-byte or double-width character earlier in the expression
/// does not shift the marker.
///
/// ```text
///   .items[] | .nam(
///                  ^ expected closing parenthesis
/// ```
pub fn render_compile_error(expr: &str, message: &str, col: Option<usize>) -> String {
    let Some(col) = col else {
        return format!("  {expr}\n  {message}");
    };
    // Locate the line containing the column; a `--jq` expression from a script file can be
    // several lines long, and a caret under line 1 for an error on line 3 is worse than none.
    let mut line_start = 0usize;
    let mut consumed = 0usize;
    let mut line = expr;
    for candidate in expr.split_inclusive('\n') {
        line = candidate;
        line_start = consumed;
        consumed += candidate.chars().count();
        if col <= consumed {
            break;
        }
    }
    let line = line.trim_end_matches(['\n', '\r']);
    let in_line = col.saturating_sub(line_start).saturating_sub(1);
    let prefix: String = line.chars().take(in_line).collect();
    let pad = display_width(&prefix);
    format!("  {line}\n  {}^ {message}", " ".repeat(pad))
}

// --------------------------------------------------------------------------- errors

/// A `jaq` diagnostic reduced to a message plus a 1-based column into the expression.
struct Diag {
    message: String,
    col: Option<usize>,
}

/// Turn a slice of the expression back into a 1-based character column.
///
/// `jaq` reports every error site as a `&str` *into the source we handed it*, so pointer
/// arithmetic recovers the position exactly — there is no position field to read. The bounds
/// check matters: for an unexpected end of input `jaq` can hand back a zero-length slice, and
/// an empty `&str` is not guaranteed to point inside the original buffer.
fn column_of(expr: &str, slice: &str) -> Option<usize> {
    let base = expr.as_ptr() as usize;
    let at = slice.as_ptr() as usize;
    if at < base || at > base + expr.len() {
        return None;
    }
    let byte = at - base;
    Some(expr.get(..byte).map_or(byte, |p| p.chars().count()) + 1)
}

fn load_error<P>(expr: &str, errs: &[(File<&str, P>, jaq_core::load::Error<&str>)]) -> Error {
    use jaq_core::load::Error as LoadError;
    // `jaq` can report several errors for one expression (`.["a` yields both an unterminated
    // string and an unterminated bracket). Only the first is shown: the later ones are
    // usually cascade noise, and a caret can only point at one place.
    let diag = errs
        .iter()
        .find_map(|(_file, e)| match e {
            LoadError::Io(items) => items
                .first()
                .map(|(path, msg)| Diag { message: format!("{path}: {msg}"), col: None }),
            LoadError::Lex(items) => items.first().map(|(expect, found)| Diag {
                message: format!("expected {}{}", expect.as_str(), found_suffix(found)),
                col: column_of(expr, found),
            }),
            LoadError::Parse(items) => items.first().map(|(expect, found)| Diag {
                message: format!("expected {}{}", expect.as_str(), found_suffix(found)),
                col: column_of(expr, found),
            }),
        })
        .unwrap_or_else(|| Diag { message: "could not be parsed".into(), col: None });
    jq_compile(expr, diag)
}

/// `jaq`'s compile-error shape: per module file, a list of (symbol, what-kind-of-symbol).
type CompileErrors<'a, P> = [(File<&'a str, P>, Vec<(&'a str, jaq_core::compile::Undefined)>)];

fn compile_error<P>(expr: &str, errs: &CompileErrors<'_, P>) -> Error {
    let diag = errs
        .iter()
        .flat_map(|(_file, es)| es.iter())
        .next()
        .map(|(name, undefined)| Diag {
            message: format!("{} {name:?} is not defined", undefined.as_str()),
            col: column_of(expr, name),
        })
        .unwrap_or_else(|| Diag { message: "could not be compiled".into(), col: None });
    jq_compile(expr, diag)
}

/// `jaq` reports the *expected* token and hands back what it found. An empty found-slice
/// means end of input, and saying so is much clearer than `found ""`.
fn found_suffix(found: &str) -> String {
    if found.is_empty() {
        ", but the expression ended".into()
    } else {
        format!(", found {found:?}")
    }
}

fn jq_compile(expr: &str, diag: Diag) -> Error {
    Error::new(ErrorKind::JqCompile {
        expr: expr.to_string(),
        message: diag.message,
        col: diag.col,
    })
}

// ----------------------------------------------------------------- value conversion

/// `serde_json::Value` → `jaq_json::Val`.
///
/// Hand-written rather than going through `jaq-json`'s optional `serde` feature, which is not
/// enabled in this workspace, and rather than serializing to text and reparsing, which would
/// cost a full JSON encode/decode per page of a `--paginate` run.
fn to_val(v: &Value) -> Val {
    match v {
        Value::Null => Val::Null,
        Value::Bool(b) => Val::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Val::Num(jaq_json::Num::from_integral(i))
            } else if let Some(u) = n.as_u64() {
                // Gitea has two `uint64` fields; they exceed `i64` only at absurd values,
                // but losing them to a float here would silently change the number.
                Val::Num(jaq_json::Num::from_integral(u))
            } else {
                Val::from(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => Val::from(s.clone()),
        Value::Array(items) => items.iter().map(to_val).collect(),
        Value::Object(map) => {
            let m: jaq_json::Map =
                map.iter().map(|(k, v)| (Val::from(k.clone()), to_val(v))).collect();
            Val::obj(m)
        }
    }
}

/// `jaq_json::Val` → `serde_json::Value`.
fn from_val(v: &Val) -> Value {
    match v {
        Val::Null => Value::Null,
        Val::Bool(b) => Value::Bool(*b),
        // `Num` is `Int(isize) | BigInt | Float | Dec`, and the accessors for the last three
        // are crate-private. Its `Display` is jq's own number formatting, so round-tripping
        // through it is both the shortest and the most faithful conversion available.
        Val::Num(n) => serde_json::from_str(&n.to_string()).unwrap_or(Value::Null),
        // `jaq-json` is a JSON *superset* with byte strings. A byte string cannot survive as
        // JSON, so it is decoded lossily rather than failing the whole pipeline — the only way
        // to see one is to call `@base64d` or `tobytes` explicitly.
        Val::BStr(b) | Val::TStr(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
        Val::Arr(items) => Value::Array(items.iter().map(from_val).collect()),
        Val::Obj(map) => {
            Value::Object(map.iter().map(|(k, v)| (val_key(k), from_val(v))).collect())
        }
    }
}

/// `jaq-json` allows non-string object keys; JSON does not. Stringify rather than drop.
fn val_key(v: &Val) -> String {
    match v {
        Val::BStr(b) | Val::TStr(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(expr: &str, input: Value) -> Vec<Value> {
        Filter::compile(expr).expect(expr).run(&input).expect(expr)
    }

    fn out(expr: &str, input: Value) -> String {
        let mut buf = Vec::new();
        write_results(&run(expr, input), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn prs() -> Value {
        json!([
            {"number": 1, "title": "add a thing", "state": "open",
             "head": {"ref": "feat/a"}, "labels": [{"name": "bug"}, {"name": "ui"}],
             "user": {"login": "alice"}, "draft": false, "created_at": "2024-01-02T03:04:05Z"},
            {"number": 2, "title": "fix a thing", "state": "closed",
             "head": {"ref": "fix/b"}, "labels": [],
             "user": {"login": "bob"}, "draft": true, "created_at": "2024-03-04T05:06:07Z"}
        ])
    }

    /// ~20 golden expressions over one realistic document. These are the shapes users actually
    /// type, and they exist to catch a `jaq` upgrade that changes any of them.
    ///
    /// Bug this prevents: a `jaq` major bump silently changing `length`, string interpolation,
    /// object construction, or number formatting under us.
    #[test]
    fn jq_goldens() {
        let d = prs();
        let mut report = String::new();
        for expr in [
            ".",
            "length",
            ".[0].number",
            ".[].number",
            ".[].title",
            ".[0].head.ref",
            ".[] | .user.login",
            ".[] | select(.state == \"open\") | .number",
            "map(.number)",
            "[.[].state] | unique",
            ".[0] | keys",
            ".[0] | {n: .number, t: .title}",
            ".[] | [.number, .title] | @text",
            ".[].labels | map(.name) | join(\",\")",
            ".[0].labels[0].name",
            ".[] | .number, .state",
            "map(select(.draft)) | length",
            ".[0].created_at",
            ".[] | \"#\\(.number) \\(.title)\"",
            "group_by(.state) | map(length)",
            ".[0].missing_field",
            "to_entries | length",
            ".[] | .number * 10",
            "[limit(1; .[].number)]",
        ] {
            report.push_str(&format!("{expr}\n{}\n", out(expr, d.clone())));
        }
        insta::assert_snapshot!(report);
    }

    /// Bug this prevents: printing `"add a thing"` with quotes. jq prints a top-level string
    /// result raw, and every `--jq '.title' | read -r` pipeline depends on it.
    #[test]
    fn string_results_are_raw_and_unquoted() {
        assert_eq!(out(".[0].title", prs()), "add a thing\n");
        // ...but a string *inside* a structure keeps its quotes.
        assert_eq!(out("[.[0].title]", prs()), "[\"add a thing\"]\n");
    }

    /// Bug this prevents: joining multiple results onto one line, or emitting them as a JSON
    /// array, either of which breaks `--jq '.[].number' | while read n`.
    #[test]
    fn multiple_results_are_one_per_line() {
        assert_eq!(out(".[].number", prs()), "1\n2\n");
        assert_eq!(out("empty", prs()), "");
    }

    /// Documents a real gap rather than leaving it to be discovered in the field: `jaq` 3
    /// implements `@text`, `@sh`, `@html`, `@uri`, and `@base64`, but **not `@csv` or `@tsv`**.
    /// `jq` users reach for `@tsv` constantly, so the failure has to be a clear "not defined"
    /// message — and if a future `jaq` adds them, this test starts failing and tells us to
    /// document the improvement.
    #[test]
    fn jaq_does_not_implement_csv_or_tsv() {
        for expr in ["[1,2] | @tsv", "[1,2] | @csv"] {
            let err = Filter::compile(expr).unwrap_err();
            let ErrorKind::JqCompile { message, .. } = &*err.kind else { panic!("{expr}") };
            assert!(message.contains("not defined"), "{expr}: {message}");
        }
        // The documented workaround, which does work.
        assert_eq!(out(r#".[] | join("\t")"#, json!([[1, "a"]])), "1\ta\n");
    }

    /// Bug this prevents: reporting a compile error without a position, so the user rereads a
    /// 200-character expression by eye looking for the missing paren.
    #[test]
    fn compile_error_carries_a_column_and_renders_a_caret() {
        let err = Filter::compile(".items[] | select(.n == 1").unwrap_err();
        let ErrorKind::JqCompile { expr, message, col } = &*err.kind else {
            panic!("wrong kind: {:?}", err.kind);
        };
        assert!(col.is_some(), "no column in {message}");
        insta::assert_snapshot!(render_compile_error(expr, message, *col), @r#"
          .items[] | select(.n == 1
                                   ^ expected closing parenthesis, but the expression ended
        "#);
        assert_eq!(err.exit_code(), 2);
    }

    /// Bug this prevents: an undefined filter being reported as a parse error with no name,
    /// which is the single most common `--jq` mistake (a typo'd builtin).
    #[test]
    fn undefined_filter_names_the_symbol() {
        let err = Filter::compile(".a | lenght").unwrap_err();
        let ErrorKind::JqCompile { expr, message, col } = &*err.kind else {
            panic!("wrong kind");
        };
        assert!(message.contains("lenght"), "{message}");
        insta::assert_snapshot!(render_compile_error(expr, message, *col), @r#"
          .a | lenght
               ^ filter "lenght" is not defined
        "#);
    }

    /// Bug this prevents: placing the caret by byte offset, so a CJK string literal earlier in
    /// the expression shifts the marker several columns to the right.
    #[test]
    fn caret_is_placed_by_display_width() {
        let rendered =
            render_compile_error("\"日本語\" | nope", "filter \"nope\" is not defined", Some(9));
        // 3 CJK chars = 6 columns, plus two quotes and " | " => the caret sits under `nope`.
        let caret_line = rendered.lines().nth(1).unwrap();
        assert_eq!(caret_line.find('^'), Some(2 + 11), "{rendered}");
    }

    /// Bug this prevents: a multi-line `--jq` expression from a script file getting its caret
    /// under line 1 for an error on a later line.
    #[test]
    fn caret_follows_the_offending_line() {
        let expr = ".a\n| .b\n| nope";
        let col = ".a\n| .b\n| ".chars().count() + 1;
        insta::assert_snapshot!(render_compile_error(expr, "not defined", Some(col)), @r"
          | nope
            ^ not defined
        ");
    }

    /// Bug this prevents: a runtime type error aborting with a `JqCompile` error and a caret
    /// pointing at an expression that compiled perfectly well.
    #[test]
    fn runtime_errors_are_not_compile_errors() {
        let f = Filter::compile(". + 1").unwrap();
        let err = f.run(&json!("not a number")).unwrap_err();
        assert!(matches!(&*err.kind, ErrorKind::Usage(m) if m.contains("failed while running")));
    }

    /// Bug this prevents: recompiling per page of `--paginate`. Also proves the compiled filter
    /// does not borrow the source text, which is what makes reuse possible at all.
    #[test]
    fn one_filter_runs_over_many_pages() {
        let f = Filter::compile(".[].number").unwrap();
        let page1 = json!([{"number": 1}, {"number": 2}]);
        let page2 = json!([{"number": 3}]);
        assert_eq!(f.run(&page1).unwrap(), vec![json!(1), json!(2)]);
        assert_eq!(f.run(&page2).unwrap(), vec![json!(3)]);
    }

    /// Bug this prevents: object key order being lost in the round trip through `jaq`, so
    /// `--json number,title --jq .` reorders the user's columns.
    #[test]
    fn object_key_order_round_trips() {
        let v = json!({"z": 1, "a": 2, "m": 3});
        assert_eq!(out(".", v), r#"{"z":1,"a":2,"m":3}"#.to_string() + "\n");
    }

    /// Bug this prevents: integers coming back as `1.0` because the conversion routed
    /// everything through `f64`.
    #[test]
    fn integers_stay_integers() {
        assert_eq!(out(".", json!(1)), "1\n");
        assert_eq!(out(".", json!(-1)), "-1\n");
        assert_eq!(out(".", json!(1.5)), "1.5\n");
        assert_eq!(out(".", json!(i64::MAX)), format!("{}\n", i64::MAX));
        assert_eq!(out(". + 1", json!(1)), "2\n");
    }
}

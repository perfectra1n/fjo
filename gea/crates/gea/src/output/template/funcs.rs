//! The `--template` function library: `gh`'s helper set, and nothing more.

use gitea_core::Error;
use gitea_core::types::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

use super::eval::{Ctx, to_text};
use super::lex::template_err;
use crate::output::color;

/// Every function name, in the order the "unknown function" error lists them.
pub const FUNCTIONS: &[&str] = &[
    "autocolor",
    "color",
    "hyperlink",
    "join",
    "pluck",
    "printf",
    "tablerender",
    "tablerow",
    "timeago",
    "timefmt",
    "truncate",
];

pub fn call(ctx: &mut Ctx, name: &str, args: &[Value], line: usize) -> Result<Value, Error> {
    match name {
        // `tablerow` evaluates to the empty string so that it can be used in the middle of a
        // template without printing anything, exactly like `gh`.
        "tablerow" => {
            arity(name, args, 1.., line)?;
            ctx.table.row(args.iter().map(to_text));
            Ok(Value::String(String::new()))
        }
        "tablerender" => {
            arity(name, args, 0..=0, line)?;
            Ok(Value::String(ctx.flush_table()))
        }
        "timeago" => {
            arity(name, args, 1..=1, line)?;
            Ok(Value::String(timeago(&to_text(&args[0]))))
        }
        "timefmt" => {
            arity(name, args, 2..=2, line)?;
            let layout = to_text(&args[0]);
            timefmt_in(&layout, &to_text(&args[1]), &TimeZone::system(), line).map(Value::String)
        }
        "truncate" => {
            arity(name, args, 2..=2, line)?;
            let max = as_usize(&args[0], name, line)?;
            Ok(Value::String(color::truncate_visible(&to_text(&args[1]), max)))
        }
        "color" => {
            arity(name, args, 2..=2, line)?;
            let spec = to_text(&args[0]);
            let style = color::style_by_name(&spec).ok_or_else(|| {
                template_err(
                    line,
                    format!("color {spec:?} is not one of: {}", color::STYLE_NAMES.join(", ")),
                )
            })?;
            Ok(Value::String(color::paint(ctx.term, style, &to_text(&args[1]))))
        }
        // Two arities. One argument is `gea`'s own form: look the value up in the state →
        // color map. Two arguments is `gh`'s form (`autocolor "green" .x`), accepted so a
        // template copied from `gh` keeps working rather than failing on an arity check.
        "autocolor" => {
            arity(name, args, 1..=2, line)?;
            if args.len() == 1 {
                Ok(Value::String(color::autocolor(ctx.term, &to_text(&args[0]))))
            } else {
                let spec = to_text(&args[0]);
                let text = to_text(&args[1]);
                match color::style_by_name(&spec) {
                    Some(style) => Ok(Value::String(color::paint(ctx.term, style, &text))),
                    None => Ok(Value::String(text)),
                }
            }
        }
        "join" => {
            arity(name, args, 2..=2, line)?;
            let sep = to_text(&args[0]);
            Ok(Value::String(items(&args[1]).iter().map(to_text).collect::<Vec<_>>().join(&sep)))
        }
        "pluck" => {
            arity(name, args, 2..=2, line)?;
            let field = to_text(&args[0]);
            Ok(Value::Array(
                items(&args[1])
                    .iter()
                    .map(|v| v.get(&field).cloned().unwrap_or(Value::Null))
                    .collect(),
            ))
        }
        "printf" => {
            arity(name, args, 1.., line)?;
            printf(&to_text(&args[0]), &args[1..], line).map(Value::String)
        }
        "hyperlink" => {
            arity(name, args, 1..=2, line)?;
            let url = to_text(&args[0]);
            let text = args.get(1).map_or_else(|| url.clone(), to_text);
            Ok(Value::String(color::hyperlink(ctx.term, &url, &text)))
        }
        other => Err(template_err(
            line,
            format!("unknown function {other:?}; available: {}", FUNCTIONS.join(", ")),
        )),
    }
}

/// Treat a non-array as a one-element list.
///
/// `{{.labels | pluck "name" | join ","}}` must not break when the API returns a single object
/// instead of an array, and an absent field (which projection turns into `null`) should join to
/// the empty string rather than the text `null`.
fn items(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn as_usize(v: &Value, func: &str, line: usize) -> Result<usize, Error> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| template_err(line, format!("{func} needs a non-negative number, got {v}")))
}

fn arity(
    func: &str,
    args: &[Value],
    range: impl std::ops::RangeBounds<usize>,
    line: usize,
) -> Result<(), Error> {
    use std::ops::Bound;
    let n = args.len();
    let ok = range.contains(&n);
    if ok {
        return Ok(());
    }
    let expected = match (range.start_bound(), range.end_bound()) {
        (Bound::Included(a), Bound::Included(b)) if a == b => format!("{a}"),
        (Bound::Included(a), Bound::Included(b)) => format!("{a} to {b}"),
        (Bound::Included(a), Bound::Unbounded) => format!("at least {a}"),
        _ => "a different number of".into(),
    };
    Err(template_err(line, format!("{func} takes {expected} argument(s), got {n}")))
}

// ---------------------------------------------------------------------------- printf

/// The verbs `printf` accepts, in the order the error message lists them.
///
/// This is a **documented subset**, like [`LAYOUTS`]. Width, precision, and flags
/// (`%-10s`, `%.2f`, `%+d`, `%08x`) are deliberately **not** supported: implementing one of them
/// invites all of them, each with Go-specific semantics (`%v` on a map, `%q` on a rune, the
/// interaction between `-` and `0`), for a feature whose template users want in order to write
/// `#%v`. Column padding is [`crate::output::table`]'s job and it does it with display widths,
/// which `%-10s` could not match anyway.
pub const PRINTF_VERBS: &[&str] = &["%v", "%s", "%d", "%f", "%q", "%%"];

/// One parsed piece of a format string.
enum Piece {
    Lit(String),
    Verb(char),
}

/// Go's `printf`, restricted to [`PRINTF_VERBS`].
///
/// `%v` and `%s` coincide here, and that is not an oversight: Go distinguishes them by the
/// dynamic type of the argument, and over `serde_json::Value` the "default format" of every
/// scalar *is* its string form. Both are accepted because real `gh` templates use both, and
/// silently rejecting one would break a copied template for no reason.
///
/// An argument-count mismatch is an **error**, not Go's `%!v(MISSING)` placeholder. Templates
/// are authored interactively against live data, so a message naming both counts is
/// immediately actionable, whereas `%!v(MISSING)` embedded in row 40 of a table is noise that
/// people learn to ignore.
pub fn printf(format: &str, args: &[Value], line: usize) -> Result<String, Error> {
    let pieces = parse_format(format, line)?;
    let verbs = pieces.iter().filter(|p| matches!(p, Piece::Verb(_))).count();
    if verbs != args.len() {
        return Err(template_err(
            line,
            format!(
                "printf format {format:?} has {verbs} verb(s) but got {} argument(s)",
                args.len()
            ),
        ));
    }

    let mut out = String::new();
    let mut next = 0usize;
    for piece in &pieces {
        match piece {
            Piece::Lit(s) => out.push_str(s),
            Piece::Verb(verb) => {
                let arg = &args[next];
                next += 1;
                out.push_str(&format_verb(*verb, arg, line)?);
            }
        }
    }
    Ok(out)
}

fn parse_format(format: &str, line: usize) -> Result<Vec<Piece>, Error> {
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            lit.push(c);
            continue;
        }
        let Some(verb) = chars.next() else {
            return Err(template_err(
                line,
                format!(
                    "printf format {format:?} ends with a lone `%`; \
                     write `%%` for a literal percent sign"
                ),
            ));
        };
        if verb == '%' {
            lit.push('%');
            continue;
        }
        if !matches!(verb, 'v' | 's' | 'd' | 'f' | 'q') {
            // Width/precision/flags land here too, which is the point: `%-10s` reports
            // `%-` rather than silently printing `-10s`.
            return Err(template_err(
                line,
                format!(
                    "printf does not support `%{verb}`; supported verbs are {} \
                     (width, precision, and flags such as `%-10s` or `%.2f` are not supported)",
                    PRINTF_VERBS.join(", ")
                ),
            ));
        }
        if !lit.is_empty() {
            pieces.push(Piece::Lit(std::mem::take(&mut lit)));
        }
        pieces.push(Piece::Verb(verb));
    }
    if !lit.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    Ok(pieces)
}

fn format_verb(verb: char, arg: &Value, line: usize) -> Result<String, Error> {
    match verb {
        // Go's "default format". Over JSON: numbers bare, strings bare, bools as `true`/`false`,
        // `null` as nothing, composites as compact JSON.
        'v' | 's' => Ok(to_text(arg)),
        'd' => as_i64(arg)
            .map(|n| n.to_string())
            .ok_or_else(|| verb_mismatch(verb, "an integer", arg, line)),
        // Go's `%f` defaults to six decimal places; matching it means a copied template's
        // output does not change shape.
        'f' => as_f64(arg)
            .map(|f| format!("{f:.6}"))
            .ok_or_else(|| verb_mismatch(verb, "a number", arg, line)),
        // A double-quoted, escaped string. `serde_json`'s string encoder produces JSON escapes,
        // which agree with Go's `%q` for everything a Gitea response can contain. Applied to
        // the value's *text* form rather than to the raw value, because Go's `%q` on an integer
        // prints the character with that code point — a trap nobody wants in a template.
        'q' => Ok(serde_json::to_string(&Value::String(to_text(arg)))
            .expect("a string is always serializable")),
        other => Err(template_err(line, format!("printf does not support `%{other}`"))),
    }
}

fn verb_mismatch(verb: char, wanted: &str, arg: &Value, line: usize) -> Error {
    template_err(line, format!("printf `%{verb}` needs {wanted}, got {arg}"))
}

/// Accept a JSON integer, an integral float, or a numeric string. Gitea sends `int64` for
/// every id, but a projected or `--jq`-reshaped document can legitimately carry `"42"`.
fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

// ------------------------------------------------------------------------------ time

/// The Go layouts `timefmt` accepts, and their `jiff` `strftime` equivalents.
///
/// This is a **documented subset**, not an implementation of Go's reference-time layout
/// language. Full Go layout parsing means recognizing `2006`, `01`, `02`, `03`, `15`, `04`,
/// `05`, `PM`, `Mon`, `Jan`, `-0700`, `MST`, and their zero-padded/space-padded/abbreviated
/// variants in any order — a parser with its own ambiguities (`06` is a year, `6` is a month)
/// for a feature whose users overwhelmingly want one of these eight. Anything else is an error
/// that names the supported set, which is a much better outcome than a layout that
/// half-works.
pub const LAYOUTS: &[(&str, &str)] = &[
    ("2006-01-02", "%Y-%m-%d"),
    ("2006-01-02 15:04:05", "%Y-%m-%d %H:%M:%S"),
    ("15:04", "%H:%M"),
    ("RFC3339", "%Y-%m-%dT%H:%M:%S%:z"),
    ("RFC1123", "%a, %d %b %Y %H:%M:%S %Z"),
    ("Kitchen", "%-I:%M%p"),
    ("DateOnly", "%Y-%m-%d"),
    ("TimeOnly", "%H:%M:%S"),
];

/// Format an RFC 3339 timestamp with one of [`LAYOUTS`], rendered in `tz`.
///
/// Rendering in the **local** zone is deliberate. Gitea marshals timestamps in the
/// *server's* configured offset, so echoing the input offset back would show a human a wall
/// clock from a machine they have never seen. `RFC3339` still carries the offset, so nothing
/// is lost for machine consumers.
///
/// An unset or unparseable timestamp renders as the empty string. Go's zero time
/// (`0001-01-01T00:00:00Z`) is what Gitea sends for `merged_at` on an unmerged pull request;
/// formatting it would put a year-1 date in a table.
pub fn timefmt_in(layout: &str, value: &str, tz: &TimeZone, line: usize) -> Result<String, Error> {
    let fmt = LAYOUTS.iter().find(|(go, _)| *go == layout).map(|(_, f)| *f).ok_or_else(|| {
        template_err(
            line,
            format!(
                "timefmt layout {layout:?} is not supported; use one of: {}",
                LAYOUTS.iter().map(|(go, _)| *go).collect::<Vec<_>>().join(", ")
            ),
        )
    })?;
    let Some(ts) = parse_timestamp(value) else {
        return Ok(String::new());
    };
    Ok(ts.as_jiff().to_zoned(tz.clone()).strftime(fmt).to_string())
}

/// `gh`-style relative time. See [`timeago_between`].
pub fn timeago(value: &str) -> String {
    match parse_timestamp(value) {
        Some(ts) => timeago_between(jiff::Timestamp::now(), ts.as_jiff()),
        None => String::new(),
    }
}

/// The buckets, chosen to produce `gh`'s phrasings:
///
/// | age | output |
/// | --- | --- |
/// | < 1 minute | `less than a minute ago` |
/// | < 45 minutes | `N minutes ago` |
/// | < 90 minutes | `about 1 hour ago` |
/// | < 24 hours | `about N hours ago` |
/// | < 30 days | `N days ago` |
/// | < 365 days | `N months ago` |
/// | otherwise | `N years ago` |
///
/// Future instants are rendered as `in …` rather than as a negative age: Gitea can return a
/// timestamp slightly ahead of the client's clock, and `-1 minutes ago` looks like a bug.
///
/// `now` is a parameter rather than being read inside, so this is testable without a clock
/// abstraction and without a flaky boundary.
pub fn timeago_between(now: jiff::Timestamp, then: jiff::Timestamp) -> String {
    let seconds = (now - then).get_seconds();
    let future = seconds < 0;
    let s = seconds.unsigned_abs();
    let phrase = match s {
        0..60 => "less than a minute".to_string(),
        60..2700 => plural(s / 60, "minute"),
        2700..5400 => "about 1 hour".to_string(),
        5400..86_400 => format!("about {}", plural(s / 3600, "hour")),
        86_400..2_592_000 => plural(s / 86_400, "day"),
        2_592_000..31_536_000 => plural(s / 2_592_000, "month"),
        _ => plural(s / 31_536_000, "year"),
    };
    if future { format!("in {phrase}") } else { format!("{phrase} ago") }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
}

/// Parse an RFC 3339 timestamp, returning `None` for anything unusable.
///
/// "Unusable" includes Go's zero time and the unix epoch, both of which Gitea uses to mean
/// "unset" — the check is [`Timestamp::is_unset`], so templates and the typed models agree on
/// what counts as absent.
fn parse_timestamp(value: &str) -> Option<Timestamp> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let ts = Timestamp::from_jiff(trimmed.parse::<jiff::Timestamp>().ok()?);
    (!ts.is_unset()).then_some(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    /// Bug this prevents: reading the clock inside `timeago`, which makes every boundary test
    /// flaky, and getting the bucket phrasings wrong so output no longer matches `gh`.
    #[test]
    fn timeago_buckets() {
        let now = ts("2024-06-01T12:00:00Z");
        let cases = [
            ("2024-06-01T12:00:00Z", "less than a minute ago"),
            ("2024-06-01T11:59:30Z", "less than a minute ago"),
            ("2024-06-01T11:59:00Z", "1 minute ago"),
            ("2024-06-01T11:57:00Z", "3 minutes ago"),
            ("2024-06-01T11:16:00Z", "44 minutes ago"),
            ("2024-06-01T11:00:00Z", "about 1 hour ago"),
            ("2024-06-01T09:00:00Z", "about 3 hours ago"),
            ("2024-05-31T12:00:00Z", "1 day ago"),
            ("2024-05-29T12:00:00Z", "3 days ago"),
            ("2024-04-01T12:00:00Z", "2 months ago"),
            ("2023-05-01T12:00:00Z", "1 year ago"),
            ("2022-06-01T12:00:00Z", "2 years ago"),
            ("2019-06-01T12:00:00Z", "5 years ago"),
        ];
        for (then, expected) in cases {
            assert_eq!(timeago_between(now, ts(then)), expected, "{then}");
        }
    }

    /// Bug this prevents: a server clock a few seconds ahead of ours rendering as
    /// "-1 minutes ago".
    #[test]
    fn timeago_handles_the_future() {
        let now = ts("2024-06-01T12:00:00Z");
        assert_eq!(timeago_between(now, ts("2024-06-01T13:00:00Z")), "in about 1 hour");
    }

    /// Bug this prevents: Go's zero time rendering as "2025 years ago" — the exact failure the
    /// timestamp module exists to stop, reproduced here because templates see raw JSON and
    /// bypass the typed deserializer.
    #[test]
    fn go_zero_time_renders_as_nothing() {
        assert_eq!(timeago("0001-01-01T00:00:00Z"), "");
        assert_eq!(timeago(""), "");
        assert_eq!(timeago("not a date"), "");
        assert_eq!(
            timefmt_in("2006-01-02", "0001-01-01T00:00:00Z", &TimeZone::UTC, 1).unwrap(),
            ""
        );
    }

    /// Bug this prevents: silently accepting an unsupported Go layout and emitting it
    /// literally, so `{{timefmt "Jan 2" .x}}` prints `Jan 2` for every row.
    #[test]
    fn unsupported_layouts_name_the_supported_set() {
        let err = timefmt_in("Jan 2, 2006", "2024-06-01T12:00:00Z", &TimeZone::UTC, 3).unwrap_err();
        let gitea_core::ErrorKind::Template { message, line } = &*err.kind else { panic!() };
        assert_eq!(*line, 3);
        assert!(message.contains("RFC3339"), "{message}");
        assert!(message.contains("2006-01-02"), "{message}");
    }

    /// Bug this prevents: a layout in the table not actually being a valid `jiff` format
    /// string, which `strftime` would emit verbatim (`%-I` becoming literal text).
    #[test]
    fn every_layout_formats() {
        let input = "2024-06-01T15:04:05Z";
        let mut report = String::new();
        for (go, _) in LAYOUTS {
            let out = timefmt_in(go, input, &TimeZone::UTC, 1).unwrap();
            assert!(!out.contains('%'), "{go} left a literal specifier: {out}");
            report.push_str(&format!("{go:20} {out}\n"));
        }
        insta::assert_snapshot!(report, @r"
        2006-01-02           2024-06-01
        2006-01-02 15:04:05  2024-06-01 15:04:05
        15:04                15:04
        RFC3339              2024-06-01T15:04:05+00:00
        RFC1123              Sat, 01 Jun 2024 15:04:05 UTC
        Kitchen              3:04PM
        DateOnly             2024-06-01
        TimeOnly             15:04:05
        ");
    }

    /// Bug this prevents: rendering the server's offset instead of the user's zone, so a PR
    /// created at 09:00 local shows 17:00 because the instance runs in Asia/Tokyo.
    #[test]
    fn timestamps_render_in_the_requested_zone() {
        let iso = "2024-06-01T12:00:00+09:00";
        let utc = timefmt_in("2006-01-02 15:04:05", iso, &TimeZone::UTC, 1).unwrap();
        assert_eq!(utc, "2024-06-01 03:00:00");
        let ny =
            timefmt_in("2006-01-02 15:04:05", iso, &TimeZone::get("America/New_York").unwrap(), 1)
                .unwrap();
        assert_eq!(ny, "2024-05-31 23:00:00");
    }

    /// Bug this prevents: `%v` over JSON gaining Go's Go-specific spellings — a quoted string,
    /// `<nil>` for null, or `1.0` for the integer 1 — any of which would put noise into a table
    /// cell.
    #[test]
    fn printf_verbs() {
        let cases: &[(&str, &[Value], &str)] = &[
            ("#%v", &[Value::from(42)], "#42"),
            ("%v", &[Value::from("hi")], "hi"),
            ("%v", &[Value::Bool(true)], "true"),
            ("%v", &[Value::Null], ""),
            ("%v", &[serde_json::json!({"a": 1})], r#"{"a":1}"#),
            ("%s/%s", &[Value::from("o"), Value::from("r")], "o/r"),
            ("%d", &[Value::from(-7)], "-7"),
            ("%d", &[Value::from("42")], "42"),
            ("%f", &[Value::from(1.5)], "1.500000"),
            ("%q", &[Value::from("a\"b")], "\"a\\\"b\""),
            ("100%% done", &[], "100% done"),
            ("no verbs", &[], "no verbs"),
        ];
        for (fmt, args, want) in cases {
            assert_eq!(printf(fmt, args, 1).unwrap(), *want, "{fmt}");
        }
    }

    /// Bug this prevents: Go's `%!v(MISSING)` / `%!(EXTRA …)` placeholders leaking into output.
    /// In a hundred-row table those are invisible; an error naming both counts is not.
    #[test]
    fn printf_argument_count_mismatch_is_an_error() {
        for (fmt, args) in
            [("%v %v", vec![Value::from(1)]), ("%v", vec![Value::from(1), Value::from(2)])]
        {
            let err = printf(fmt, &args, 4).unwrap_err();
            let gitea_core::ErrorKind::Template { message, line } = &*err.kind else { panic!() };
            assert_eq!(*line, 4);
            assert!(message.contains("verb(s)") && message.contains("argument(s)"), "{message}");
        }
    }

    /// Bug this prevents: `%-10s` being read as the verb `-` followed by the literal `10s`, so a
    /// template that asked for padding silently prints `10s` instead. The unsupported-verb error
    /// has to name what *is* supported, the same way `timefmt`'s does.
    #[test]
    fn unsupported_verbs_and_flags_name_the_supported_set() {
        for fmt in ["%-10s", "%.2f", "%+d", "%08x", "%x", "%T"] {
            let err = printf(fmt, &[Value::from(1)], 1).unwrap_err();
            let gitea_core::ErrorKind::Template { message, .. } = &*err.kind else { panic!() };
            assert!(message.contains("%v, %s, %d, %f, %q, %%"), "{fmt}: {message}");
            assert!(message.contains("not supported"), "{fmt}: {message}");
        }
        // A lone trailing `%` is its own mistake, with its own remedy.
        let err = printf("50%", &[], 1).unwrap_err();
        let gitea_core::ErrorKind::Template { message, .. } = &*err.kind else { panic!() };
        assert!(message.contains("`%%`"), "{message}");
    }

    /// Bug this prevents: `%d` silently rendering `1.7` as `1` or a title as `0`. Go prints
    /// `%!d(string=x)`; we refuse, because a wrong number in a table is indistinguishable from a
    /// right one.
    #[test]
    fn printf_rejects_values_a_verb_cannot_represent() {
        for (fmt, arg) in
            [("%d", Value::from("nope")), ("%d", Value::from(1.7)), ("%f", Value::Bool(true))]
        {
            let err = printf(fmt, std::slice::from_ref(&arg), 1).unwrap_err();
            let gitea_core::ErrorKind::Template { message, .. } = &*err.kind else { panic!() };
            assert!(message.contains(fmt.trim_start_matches('%')), "{fmt} {arg}: {message}");
        }
    }

    /// Bug this prevents: `join`/`pluck` blowing up on a `null` that projection inserted for
    /// an absent field, which would fail the whole render for one missing label list.
    #[test]
    fn list_helpers_tolerate_null_and_scalars() {
        assert_eq!(items(&Value::Null).len(), 0);
        assert_eq!(items(&Value::from("x")).len(), 1);
    }
}

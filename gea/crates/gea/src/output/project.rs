//! `--json` field selection, projection, and discovery.
//!
//! # Field names are snake_case, verbatim from the API
//!
//! There is no casing translation anywhere in this module, and no `--json-case` flag. `gh`
//! prints camelCase because that is literally what GitHub's GraphQL API returns; its actual
//! principle is "the field names are the API's field names". Gitea's REST API is
//! snake_case, so applying `gh`'s principle here yields snake_case, and copying `gh`'s
//! *output* instead would be cargo-culting.
//!
//! Two concrete consequences make this load-bearing rather than cosmetic:
//!
//! * One `--jq` expression has to work across all three layers. `--jq '.[].head.ref'` is
//!   identical for `gea api`, `gea raw`, and `gea pr list`. Under camelCase, layer 1 would
//!   pass `head_repo` straight through while layers 2 and 3 said `headRepo` — a permanent
//!   trap, and every Gitea API doc snippet a user copies would be wrong.
//! * Any translation has to be *bijective* for `--json` to round-trip, and it is not:
//!   `html_url` maps to `htmlUrl` or `htmlURL` depending on the acronym rule, `id` and `ID`
//!   collide, and `ssh_url` vs `sshUrl` vs `sSHUrl` is a bug farm. Zero translation, zero
//!   bugs.
//!
//! The docs point users who want camelCase at `--jq 'with_entries(...)'`.

use std::io::{self, Write};

use gitea_core::{Error, ErrorKind};
use serde_json::{Map, Value};

use super::table::Table;
use super::tty::Term;

/// One selectable top-level field.
///
/// Field metadata is passed **by reference** rather than through a trait implemented on
/// generated types, so this module has no dependency on `gitea-model` or the generated
/// field tables. `xtask`'s `emit::fields` will later emit
/// `static FIELDS_PULL_REQUEST: &[FieldSpec]` tables matching this exact shape, plus a
/// sorted `OP_FIELDS` index for binary-search lookup; nothing here changes when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: &'static str,
    pub kind: FieldKind,
    pub doc: &'static str,
}

/// The shape of a field, used only to annotate the discovery listing.
///
/// Nested shapes are carried (`Object`, `Array`, `Map`) even though only top-level fields are
/// selectable, because the listing needs to tell a user that `head` is an object they should
/// reach into with `--jq` rather than a scalar they can put in a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    Int,
    Float,
    Str,
    DateTime,
    Enum(&'static [&'static str]),
    Object(&'static [FieldSpec]),
    Array(&'static FieldKind),
    Map(&'static FieldKind),
    /// A free-form object: `CreateHookOptionConfig`, `ForgeLike`, `ForgeOutbox`.
    Json,
}

impl FieldKind {
    /// Short label for the discovery listing. Deliberately not Rust type syntax — the reader
    /// is a shell user deciding what to put in `--json`, not a Rust programmer.
    pub fn label(&self) -> String {
        match self {
            Self::Bool => "bool".into(),
            Self::Int => "int".into(),
            Self::Float => "float".into(),
            Self::Str => "string".into(),
            Self::DateTime => "datetime".into(),
            Self::Enum(values) => format!("enum({})", values.join("|")),
            Self::Object(_) => "object".into(),
            Self::Array(inner) => format!("[{}]", inner.label()),
            Self::Map(inner) => format!("map[{}]", inner.label()),
            Self::Json => "json".into(),
        }
    }
}

/// What a `--json` argument asked for.
///
/// Wired in clap as `num_args = 0..=1, default_missing_value = ""`, so a bare `--json`
/// arrives as an empty string and becomes [`Selection::Discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Bare `--json`: list the field names and exit.
    Discover,
    /// `--json a,b,c`, validated against the field table.
    Fields(Vec<String>),
}

/// Parse and validate a `--json` value.
///
/// # This must run before any HTTP request
///
/// The command layer calls this **before** it touches the network, and prints the listing (or
/// the unknown-field error) without constructing a client. That is a contract, not an
/// optimization: `gea pr list --json` answers "what can I select?", and that question needs
/// neither credentials nor connectivity. A user exploring an API they are not yet
/// authenticated against must not be told to log in first, and a user on a plane must still
/// be able to read the field list.
pub fn resolve(raw: &str, available: &[FieldSpec]) -> Result<Selection, Error> {
    if raw.trim().is_empty() {
        return Ok(Selection::Discover);
    }
    let mut fields: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            // `--json title,,number` is a typo, not a request for an empty field.
            return Err(Error::new(ErrorKind::Usage(format!(
                "--json got an empty field name in {raw:?}; separate names with a single comma"
            ))));
        }
        if !available.iter().any(|f| f.name == name) {
            return Err(unknown_field(name, available));
        }
        // A repeated name would collapse into one JSON key anyway; dropping it keeps the
        // column count equal to the number of distinct names the user can see.
        if !fields.iter().any(|f| f == name) {
            fields.push(name.to_string());
        }
    }
    Ok(Selection::Fields(fields))
}

/// Build the [`ErrorKind::UnknownJsonField`] error, with a Levenshtein-1 suggestion.
fn unknown_field(given: &str, available: &[FieldSpec]) -> Error {
    let names: Vec<String> = available.iter().map(|f| f.name.to_string()).collect();
    Error::new(ErrorKind::UnknownJsonField {
        given: given.to_string(),
        suggest: suggest(given, &names),
        available: names,
    })
}

/// The closest field name within edit distance 1, comparing case-insensitively.
///
/// Case folding before measuring is what makes the most common real mistake reachable at
/// distance 1: a user coming from `gh` types `htmlUrl`, and `htmlurl` → `html_url` is a single
/// insertion. Without folding it is distance 2 and we would silently offer no suggestion at
/// the exact moment the user most needs one.
///
/// Ties break on the field-table order so the message is deterministic and snapshot-testable.
///
/// One limitation, accepted deliberately: plain Levenshtein counts a *transposition* as two
/// edits, so `titel` gets no suggestion for `title`. Damerau-Levenshtein would catch it, but
/// distance 1 is the specified rule and widening it starts suggesting `state` for `date`.
pub fn suggest(given: &str, available: &[String]) -> Option<String> {
    let g = given.to_ascii_lowercase();
    available
        .iter()
        .filter_map(|name| {
            let d = levenshtein(&g, &name.to_ascii_lowercase());
            (d <= 1).then_some((d, name))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, name)| name.clone())
}

/// Plain Wagner–Fischer edit distance over `char`s.
///
/// Field names are ASCII snake_case and there are at most a few dozen of them, so the O(nm)
/// table is free; a bounded-distance specialization would be more code for no measurable win.
fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b_chars.len()]
}

/// Print the field list.
///
/// **stdout, one name per line, exit 0** — a deliberate divergence from `gh`, which prints the
/// list to stderr and exits 1.
///
/// The justification: the user asked what fields exist and got a correct, complete answer, so
/// this is success, and calling it a failure is wrong on its face. The practical payoff is
/// that stdout + exit 0 makes the obvious pipeline work:
///
/// ```text
/// gea pr list --json | fzf --multi | paste -sd,
/// ```
///
/// With `gh`'s behavior that pipeline gets an empty stdin and a non-zero status, and a `set -e`
/// script aborts. The divergence is recorded in `docs/output.md`.
///
/// On a TTY the type and doc columns are added, through the same [`Table`] as everything else.
/// When piped it is names only, because the piped form is an input to another program and a
/// second column would have to be stripped.
pub fn write_field_list(fields: &[FieldSpec], term: &Term, out: &mut impl Write) -> io::Result<()> {
    if !term.tty {
        for f in fields {
            writeln!(out, "{}", f.name)?;
        }
        return Ok(());
    }
    let mut table = Table::new(term);
    for f in fields {
        table.row([f.name.to_string(), f.kind.label(), f.doc.to_string()]);
    }
    table.render(out)
}

/// Keep only the requested top-level keys.
///
/// * an **array** maps elementwise,
/// * an **object** is filtered,
/// * anything else is a [`ErrorKind::Usage`] error.
///
/// Nested access is `--jq`'s job. That is the same division of labour as `gh`, and it is why
/// `--json` takes a flat comma list rather than a path syntax: two overlapping ways to reach
/// into a document would each need their own escaping rules, error messages, and docs.
///
/// **Projection happens before `--jq`**, so `--json number,title --jq '.[].title'` sees only
/// the projected document. Same order as `gh`.
pub fn project(value: Value, fields: &[String]) -> Result<Value, Error> {
    match value {
        Value::Array(items) => {
            let projected: Result<Vec<Value>, Error> =
                items.into_iter().map(|item| project_object(item, fields)).collect();
            Ok(Value::Array(projected?))
        }
        v @ Value::Object(_) => project_object(v, fields),
        other => Err(Error::new(ErrorKind::Usage(format!(
            "--json needs an object or an array of objects to select fields from, but the \
             response was {}; use --jq to reach into it instead",
            type_name(&other)
        )))),
    }
}

fn project_object(value: Value, fields: &[String]) -> Result<Value, Error> {
    let Value::Object(map) = value else {
        return Err(Error::new(ErrorKind::Usage(format!(
            "--json selects fields from objects, but an array element was {}; \
             use --jq to reach into it instead",
            type_name(&value)
        ))));
    };
    let mut out = Map::new();
    for name in fields {
        // An absent key becomes `null` rather than being omitted. This keeps the key set
        // identical for every element, which is what makes the column count stable across
        // rows and keeps `--jq '.[].x'` from failing on the one object that lacks `x`.
        out.insert(name.clone(), map.get(name).cloned().unwrap_or(Value::Null));
    }
    Ok(Value::Object(out))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIELDS: &[FieldSpec] = &[
        FieldSpec { name: "number", kind: FieldKind::Int, doc: "Index within the repository" },
        FieldSpec { name: "title", kind: FieldKind::Str, doc: "Pull request title" },
        FieldSpec {
            name: "state",
            kind: FieldKind::Enum(&["open", "closed"]),
            doc: "Whether it is open",
        },
        FieldSpec { name: "html_url", kind: FieldKind::Str, doc: "Web URL" },
        FieldSpec { name: "created_at", kind: FieldKind::DateTime, doc: "Creation time" },
    ];

    fn names() -> Vec<String> {
        FIELDS.iter().map(|f| f.name.to_string()).collect()
    }

    /// Bug this prevents: a bare `--json` being treated as `--json ""` and projecting every
    /// object down to `{}`.
    #[test]
    fn bare_json_is_discovery() {
        assert_eq!(resolve("", FIELDS).unwrap(), Selection::Discover);
        assert_eq!(resolve("   ", FIELDS).unwrap(), Selection::Discover);
    }

    /// Bug this prevents: the piped listing growing a second column, which every
    /// `--json | fzf | paste -sd,` pipeline would then have to strip.
    #[test]
    fn field_list_piped_is_names_only() {
        let mut out = Vec::new();
        write_field_list(FIELDS, &Term::piped(), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "number\ntitle\nstate\nhtml_url\ncreated_at\n");
    }

    /// Bug this prevents: printing the same bare list on a TTY, where a human reading it has
    /// no way to tell that `state` is an enum or what `html_url` means.
    #[test]
    fn field_list_tty_has_aligned_type_and_doc_columns() {
        let mut out = Vec::new();
        write_field_list(FIELDS, &Term::tty(100), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        insta::assert_snapshot!(out, @r"
        number      int                Index within the repository
        title       string             Pull request title
        state       enum(open|closed)  Whether it is open
        html_url    string             Web URL
        created_at  datetime           Creation time
        ");
    }

    /// Bug this prevents: an unknown field producing a bare "unknown field" with no list and
    /// no suggestion, leaving the user to guess.
    #[test]
    fn unknown_field_suggests_and_lists() {
        let err = resolve("titl", FIELDS).unwrap_err();
        match &*err.kind {
            ErrorKind::UnknownJsonField { given, suggest, available } => {
                assert_eq!(given, "titl");
                assert_eq!(suggest.as_deref(), Some("title"));
                assert_eq!(available.len(), FIELDS.len());
            }
            other => panic!("wrong kind: {other:?}"),
        }
        // Exit code 2 (usage), like every other local error.
        assert_eq!(err.exit_code(), 2);
    }

    /// Bug this prevents: measuring edit distance case-sensitively, which makes the single
    /// most likely mistake — a `gh` user typing camelCase — fall outside distance 1 and get no
    /// suggestion at all.
    #[test]
    fn camel_case_from_gh_habits_is_suggested() {
        assert_eq!(suggest("htmlUrl", &names()).as_deref(), Some("html_url"));
        assert_eq!(suggest("Title", &names()).as_deref(), Some("title"));
        assert_eq!(suggest("createdAt", &names()).as_deref(), Some("created_at"));
    }

    /// Bug this prevents: suggesting something absurd for a name that resembles nothing,
    /// which reads as a bug in the tool.
    #[test]
    fn no_suggestion_when_nothing_is_close() {
        assert_eq!(suggest("kubernetes", &names()), None);
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("abc", "ab"), 1);
        assert_eq!(levenshtein("ab", "abc"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    /// Bug this prevents: projecting in the API's key order instead of the user's, so
    /// `--json title,number` produces columns in the order `number,title`.
    #[test]
    fn projection_uses_the_requested_order() {
        let v = json!({"number": 1, "title": "hi", "state": "open"});
        let got = project(v, &["title".into(), "number".into()]).unwrap();
        assert_eq!(serde_json::to_string(&got).unwrap(), r#"{"title":"hi","number":1}"#);
    }

    /// Bug this prevents: omitting an absent key, so one element of an array has two keys and
    /// the rest have three, and every downstream table or `--jq` breaks on that one row.
    #[test]
    fn absent_keys_become_null_not_missing() {
        let v = json!([{"number": 1, "title": "a"}, {"number": 2}]);
        let got = project(v, &["number".into(), "title".into()]).unwrap();
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            r#"[{"number":1,"title":"a"},{"number":2,"title":null}]"#
        );
    }

    /// Bug this prevents: silently returning `{}` (or the untouched scalar) when `--json` is
    /// applied to something that has no fields, so the user sees empty output and no reason.
    #[test]
    fn projection_of_a_scalar_is_a_usage_error() {
        let err = project(json!(42), &["title".into()]).unwrap_err();
        assert!(matches!(&*err.kind, ErrorKind::Usage(m) if m.contains("--jq")));
        let err = project(json!([1, 2]), &["title".into()]).unwrap_err();
        assert!(matches!(&*err.kind, ErrorKind::Usage(_)));
    }

    /// Bug this prevents: `--json title,,number` silently selecting a field named `""`.
    #[test]
    fn empty_field_name_in_a_list_is_rejected() {
        assert!(matches!(
            &*resolve("title,,number", FIELDS).unwrap_err().kind,
            ErrorKind::Usage(_)
        ));
    }

    #[test]
    fn duplicate_names_collapse() {
        assert_eq!(
            resolve("title,title", FIELDS).unwrap(),
            Selection::Fields(vec!["title".into()])
        );
    }

    #[test]
    fn field_kind_labels() {
        const INNER: FieldKind = FieldKind::Str;
        assert_eq!(FieldKind::Array(&INNER).label(), "[string]");
        assert_eq!(FieldKind::Map(&INNER).label(), "map[string]");
        assert_eq!(FieldKind::Object(FIELDS).label(), "object");
        assert_eq!(FieldKind::Json.label(), "json");
    }
}

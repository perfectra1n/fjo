//! Every generated `gea raw` operation, composed into a request and checked against the
//! metadata it was generated from.
//!
//! # What this is for
//!
//! `cli.rs` proves all 506 operations *build* a clap tree and render help. That catches the
//! crash-shaped failures (a duplicate long name is a clap `panic!`) and nothing else: a command
//! can build perfectly, parse perfectly, and still send `?per-page=50` instead of `?limit=50`,
//! or `contents/src%2Fmain.rs` instead of `contents/src/main.rs`. Those are silent wrong-request
//! bugs, they arrive with a **specification bump** rather than with a code change, and no amount
//! of `--help` rendering sees them.
//!
//! So this file goes one layer further: for each operation it synthesises an invocation from
//! that operation's own `OpMeta`, parses it through the real command tree, binds it, and asserts
//! the composed request is the one the metadata describes — method, rendered path, per-parameter
//! encoding, query keys, body JSON types, content type.
//!
//! # Why it can afford to do that 506 times
//!
//! [`gea_raw::bind`] is pure: `ArgMatches` in, `PlannedRequest` out, no socket, no filesystem,
//! no subprocess. The whole loop is string work, which is what makes exhaustive coverage cost
//! milliseconds instead of a container per operation.
//!
//! # Why the values are ugly
//!
//! Every synthesised value names where it came from — `p0-owner/s`, `v-labels-2`,
//! `{"sentinel":"config"}`. A test that passes `"v"` for everything cannot tell a value that
//! landed in the right slot from one that landed in the wrong slot and happened to match. These
//! sentinels make cross-wiring visible in the failure message.
//!
//! Path values deliberately contain a `/`, because that single character is the difference
//! between the two encoding rules and the 404 that motivated them: a `Segment` parameter must
//! come out `%2F`, a `PathLike` parameter must keep the slash. Carrying a `/` in every path
//! value means all 947 path parameters exercise their own rule rather than only the ten that
//! are `PathLike`.
//!
//! # No silent skips
//!
//! An operation that cannot be synthesised is a **failure** listing the operation, not a
//! `continue`. [`UNSYNTHESIZABLE`] is the escape hatch, and it is a reviewed constant with a
//! reason per entry — an entry that is no longer needed fails the test too, so the list cannot
//! rot into a permanent exemption.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::ArgAction;
use gea_raw::build::{body_id, form_id, query_id};
use gitea_client::meta::OPS;
use gitea_client::meta_types::{OpMeta, ParamMeta, PathEncoding, ValueTy};
use serde_json::Value;

/// Operations whose invocation cannot be synthesised from metadata alone, with the reason.
///
/// Empty, and the assertions below keep it honest in both directions: an operation missing from
/// the table fails, and an operation listed here that actually *works* fails as a stale
/// exemption. The point of the list is the reviewable diff — adding a line is a decision someone
/// signs off on, exactly like `spec/live-coverage.toml` for the live plane.
const UNSYNTHESIZABLE: &[(&str, &str)] = &[];

/// The verbs `gitea_core::http` knows how to send. A generated `"get"` or `"Get"` would build,
/// parse, bind, and then fail at the transport — long after the point where a test can name it.
const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

// ------------------------------------------------------------------ value synthesis

/// A name reduced to characters that percent-encoding leaves alone.
///
/// Everything outside RFC 3986's *unreserved* set is encoded by both encoders, so a sentinel
/// built from this plus `/` has exactly one character whose fate distinguishes `Segment` from
/// `PathLike`. Any other punctuation would make an expectation about encoding ambiguous.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

/// FNV-1a. Not for security — for a number that differs per parameter name and is identical
/// between runs, so a failure message is reproducible and two integers are never confusable.
fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A distinctive integer for `name`, well inside `i64` and far from any value a real caller
/// would type, so an integer landing under the wrong key is obvious in the diff.
fn sentinel_number(name: &str, nth: usize) -> i64 {
    7_000_000 + (hash(name) % 1_000_000) as i64 + nth as i64
}

/// RFC 3339, made distinctive through the fractional-second field.
///
/// The fraction is the only part that can vary freely without risking an invalid instant (no
/// month-length or leap-second arithmetic), and
/// [`every_synthesized_timestamp_is_real_rfc3339`] proves the result actually parses — otherwise
/// "the DateTime sentinel is a valid timestamp" would be a claim this file merely asserts about
/// itself.
fn sentinel_timestamp(name: &str, nth: usize) -> String {
    format!("2019-08-07T06:05:04.{:03}Z", (hash(name) as usize + nth) % 1000)
}

/// One flag value, as the text clap will see.
///
/// `nth` distinguishes the values of a repeatable flag so that
/// [`gea_raw::bind`] preserving *order* is observable: `--labels=v-labels-1 --labels=v-labels-2`
/// must arrive as exactly that sequence, not as a set.
fn scalar_text(ty: ValueTy, name: &str, nth: usize, upload: &Path) -> String {
    match ty {
        // Two values, and no way to make either distinctive. A bool derived from the name's
        // hash at least means the table is not uniformly `true`, so a binding that ignored the
        // value and hard-coded one would fail on roughly half the fields.
        ValueTy::Bool => (if hash(name).is_multiple_of(2) { "true" } else { "false" }).to_owned(),
        ValueTy::Int => sentinel_number(name, nth).to_string(),
        ValueTy::Float => format!("{}.{nth}", sentinel_number(name, nth)),
        ValueTy::DateTime => sentinel_timestamp(name, nth),
        // A `Json` field is the one case where the flag's text is itself structured. Passing it
        // inline rather than through `--body-file` is deliberate: it exercises `bind`'s
        // `serde_json::from_str` branch, which is where a body `int` becoming a quoted string
        // would show up, and a body file would bypass it entirely.
        ValueTy::Json => format!("{{\"sentinel\":\"{}-{nth}\"}}", slug(name)),
        ValueTy::File => upload.display().to_string(),
        ValueTy::List | ValueTy::Str => format!("v-{}-{nth}", slug(name)),
    }
}

/// The JSON a body field must hold once bound.
///
/// This is the half of the contract that a stringly-typed binding gets wrong: the API rejects
/// `{"index": "7000123"}` for a field it declared as an integer, and nothing on the client side
/// notices until a 422 comes back.
fn expected_json(ty: ValueTy, texts: &[String]) -> Value {
    match ty {
        ValueTy::Bool => Value::from(texts[0] == "true"),
        ValueTy::Int => Value::from(texts[0].parse::<i64>().expect("the sentinel is an integer")),
        ValueTy::Float => Value::from(texts[0].parse::<f64>().expect("the sentinel is a float")),
        ValueTy::List => Value::Array(texts.iter().cloned().map(Value::from).collect()),
        ValueTy::Json => serde_json::from_str(&texts[0]).expect("the sentinel is an object"),
        ValueTy::Str | ValueTy::DateTime | ValueTy::File => Value::from(texts[0].clone()),
    }
}

/// A path parameter's value: its position, its name, and a slash.
///
/// The position is in there so that a renderer that substituted parameters in declaration order
/// rather than path order would produce a visibly scrambled URL instead of a plausible one.
fn path_sentinel(index: usize, wire: &str) -> String {
    format!("p{index}-{}/s", slug(wire))
}

// ------------------------------------------------------------------ the synthesised case

/// What the built command says about one argument.
///
/// Read off the real tree rather than recomputed, because the long name is **not** the metadata's
/// `flag`: `build::unique_long` renames a generated flag that collides with an engine flag, which
/// is how the API's per-page `limit` becomes `--per-page`. Recomputing that rule here would make
/// this test agree with a copy of the logic instead of with the command the user types.
struct Shape {
    long: String,
    /// `ArgAction::Append`, i.e. clap will collect repeats rather than overwrite.
    append: bool,
}

/// One operation's synthesised invocation, and everything the resulting plan owes it.
struct Case {
    argv: Vec<String>,
    /// The rendered path, computed by plain `str::replace` rather than by the template walker
    /// under test — an independent expectation, not a second copy of `render_path`.
    path: String,
    path_values: Vec<(&'static ParamMeta, String)>,
    query: Vec<(String, String)>,
    /// JSON pointer to expected value, for every body field that was supplied.
    body: BTreeMap<&'static str, Value>,
    form: Vec<(&'static str, String)>,
    uploads: Vec<(&'static str, PathBuf)>,
    /// Every flag whose long name differs from the wire name behind it, paired with that wire
    /// name. Both halves of the gap matter: `--per-page` carries `limit`, and `--login-name`
    /// carries `login_name`. Neither spelling may reach the request.
    renamed: Vec<(String, &'static str)>,
    problems: Vec<String>,
}

fn arg_shapes(root: &clap::Command, op: &OpMeta) -> Option<BTreeMap<String, Shape>> {
    let leaf =
        root.find_subcommand("raw")?.find_subcommand(op.group)?.find_subcommand(op.command)?;
    Some(
        leaf.get_arguments()
            .filter_map(|a| {
                let long = a.get_long()?;
                let shape = Shape {
                    long: long.to_owned(),
                    append: matches!(a.get_action(), ArgAction::Append),
                };
                Some((a.get_id().to_string(), shape))
            })
            .collect(),
    )
}

/// Build an argv that fills in every parameter and body field the operation declares.
///
/// Path parameters go in **positionally**. They are accepted either way, but a path parameter
/// whose long name collided with an engine flag has been renamed, and some surrender `--repo` to
/// the `-R` global outright; a positional has no name to lose, so this route reaches every
/// operation by the same rule.
fn synthesize(op: &'static OpMeta, shapes: &BTreeMap<String, Shape>, upload: &Path) -> Case {
    let mut case = Case {
        argv: vec!["gea".to_owned(), "raw".to_owned(), op.group.to_owned(), op.command.to_owned()],
        path: op.path.to_owned(),
        path_values: Vec::new(),
        query: Vec::new(),
        body: BTreeMap::new(),
        form: Vec::new(),
        uploads: Vec::new(),
        renamed: Vec::new(),
        problems: Vec::new(),
    };

    for (i, p) in op.path_params().enumerate() {
        let value = path_sentinel(i, p.wire);
        case.argv.push(value.clone());
        let placeholder = format!("{{{}}}", p.wire);
        if !op.path.contains(&placeholder) {
            // A declared path parameter the template never mentions is a parameter the user is
            // forced to type into a void — a generator bug that produces a request missing the
            // value and no diagnostic anywhere.
            case.problems.push(format!(
                "{}: declares path parameter {:?} but {:?} has no {placeholder}",
                op.op_id, p.wire, op.path
            ));
        }
        let encoded = match p.encoding {
            PathEncoding::Segment => value.replace('/', "%2F"),
            PathEncoding::PathLike => value.clone(),
        };
        case.path = case.path.replace(&placeholder, &encoded);
        case.path_values.push((p, value));
    }

    for p in op.query_params() {
        for v in push_flag(&mut case, op, shapes, &query_id(p.flag), p.ty, p.wire, upload) {
            case.query.push((p.wire.to_owned(), v));
        }
    }

    for p in op.form_params() {
        for v in push_flag(&mut case, op, shapes, &form_id(p.flag), p.ty, p.wire, upload) {
            match p.ty {
                ValueTy::File => case.uploads.push((p.wire, PathBuf::from(v))),
                _ => case.form.push((p.wire, v)),
            }
        }
    }

    if let Some(b) = op.body {
        for f in b.fields {
            let texts = push_flag(&mut case, op, shapes, &body_id(f.flag), f.ty, f.flag, upload);
            if !texts.is_empty() {
                case.body.insert(f.pointer, expected_json(f.ty, &texts));
            }
        }
    }

    case
}

/// Append one generated flag to the argv, with as many values as clap will accept for it.
///
/// Always `--long=value`, never `--long value`. A `Bool` argument is built with
/// `require_equals(true)` precisely so that a bare `--draft` cannot swallow the next token — and
/// the next token here is a path parameter, so the separated form would silently move a value
/// out of the URL and into a flag.
fn push_flag(
    case: &mut Case,
    op: &OpMeta,
    shapes: &BTreeMap<String, Shape>,
    id: &str,
    ty: ValueTy,
    name: &'static str,
    upload: &Path,
) -> Vec<String> {
    let Some(shape) = shapes.get(id) else {
        case.problems.push(format!(
            "{}: metadata declares {id} but the built command has no flag for it",
            op.op_id
        ));
        return Vec::new();
    };
    if shape.long != name {
        case.renamed.push((shape.long.clone(), name));
    }
    let count = if shape.append { 2 } else { 1 };
    (1..=count)
        .map(|nth| {
            let text = scalar_text(ty, name, nth, upload);
            case.argv.push(format!("--{}={text}", shape.long));
            text
        })
        .collect()
}

// ------------------------------------------------------------------------ the checks

/// Everything one operation's plan must satisfy. Returns the problems found, empty on success.
fn check(op: &'static OpMeta, upload: &Path) -> Vec<String> {
    let root = gea::raw::root(Some(op.group), Some(op.command));
    let Some(shapes) = arg_shapes(&root, op) else {
        return vec![format!(
            "{}: no `raw {} {}` leaf in the tree",
            op.op_id, op.group, op.command
        )];
    };

    let case = synthesize(op, &shapes, upload);
    let mut problems = case.problems.clone();

    let matches = match root.try_get_matches_from(&case.argv) {
        Ok(m) => m,
        Err(e) => {
            problems.push(format!("{}: {:?} did not parse: {e}", op.op_id, case.argv));
            return problems;
        }
    };
    let Some(leaf) = matches
        .subcommand_matches("raw")
        .and_then(|m| m.subcommand_matches(op.group))
        .and_then(|m| m.subcommand_matches(op.command))
    else {
        problems.push(format!("{}: parsed, but the leaf matches are missing", op.op_id));
        return problems;
    };

    // `ctx` is `None` on purpose: every path parameter was supplied explicitly, so repository
    // context cannot mask a parameter this test failed to pass.
    let plan = match gea_raw::bind(op, leaf, None, &mut std::io::empty()) {
        Ok(p) => p,
        Err(e) => {
            problems.push(format!("{}: bind failed: {e}", op.op_id));
            return problems;
        }
    };

    check_method(op, &plan, &mut problems);
    check_path(op, &case, &plan, &mut problems);
    check_query(op, &case, &plan, &mut problems);
    check_body(op, &case, &plan, &mut problems);

    if plan.form != case.form {
        problems.push(format!("{}: form {:?}, expected {:?}", op.op_id, plan.form, case.form));
    }
    if plan.uploads != case.uploads {
        problems
            .push(format!("{}: uploads {:?}, expected {:?}", op.op_id, plan.uploads, case.uploads));
    }

    // `--dry-run` is the only window a user has onto the composed request, so its first line has
    // to be the request that would actually be sent rather than a separately assembled string.
    let head = format!("{} {}\n", op.method, plan.path_and_query());
    let described = plan.describe();
    if !described.starts_with(&head) {
        problems.push(format!("{}: describe() does not start with {head:?}", op.op_id));
    }

    problems
}

fn check_method(op: &OpMeta, plan: &gea_raw::PlannedRequest, problems: &mut Vec<String>) {
    if plan.method() != op.method {
        problems.push(format!("{}: method {} != {}", op.op_id, plan.method(), op.method));
    }
    if !METHODS.contains(&op.method) {
        problems.push(format!("{}: {:?} is not an HTTP method we can send", op.op_id, op.method));
    }
}

fn check_path(
    op: &OpMeta,
    case: &Case,
    plan: &gea_raw::PlannedRequest,
    problems: &mut Vec<String>,
) {
    if plan.path != case.path {
        problems.push(format!("{}: path {:?}, expected {:?}", op.op_id, plan.path, case.path));
    }
    // A surviving brace means a placeholder was never substituted and the request would go to a
    // URL containing a literal `{owner}`.
    if plan.path.contains('{') || plan.path.contains('}') {
        problems.push(format!("{}: unsubstituted placeholder in {:?}", op.op_id, plan.path));
    }

    for (p, value) in &case.path_values {
        match p.encoding {
            // The `get-contents` 404: encoding the `/` in a `filepath` asks for a file literally
            // named `src/main.rs` in the repository root, i.e. a miss for every nested file in
            // every repository.
            PathEncoding::PathLike => {
                if !plan.path.contains(value) {
                    problems.push(format!(
                        "{}: PathLike {:?} lost its slashes — {:?} does not contain {value:?}",
                        op.op_id, p.wire, plan.path
                    ));
                }
            }
            // The mirror-image bug: an unencoded `/` in a single-segment value adds a path
            // segment, so the request matches a different route entirely.
            PathEncoding::Segment => {
                let encoded = value.replace('/', "%2F");
                if !plan.path.contains(&encoded) {
                    problems.push(format!(
                        "{}: Segment {:?} is not in {:?} as {encoded:?}",
                        op.op_id, p.wire, plan.path
                    ));
                }
                if plan.path.contains(value) {
                    problems.push(format!(
                        "{}: Segment {:?} reached {:?} with its slash unencoded",
                        op.op_id, p.wire, plan.path
                    ));
                }
            }
        }
    }
}

fn check_query(
    op: &OpMeta,
    case: &Case,
    plan: &gea_raw::PlannedRequest,
    problems: &mut Vec<String>,
) {
    // Exact and ordered, which is three assertions in one: every query parameter is present,
    // each is keyed by its **wire** name rather than by the flag the user typed (`--per-page`
    // sends `limit`), and a repeated flag keeps both values in the order they were given —
    // `labels=bug&labels=ci` is legal and a map would have kept one of them.
    if plan.query != case.query {
        problems.push(format!("{}: query {:?}, expected {:?}", op.op_id, plan.query, case.query));
    }

    let rendered = plan.path_and_query();
    for (long, wire) in &case.renamed {
        for anchor in ['?', '&'] {
            if rendered.contains(&format!("{anchor}{long}=")) {
                problems.push(format!(
                    "{}: {rendered:?} is keyed by the flag name {long:?}; it must be {wire:?}",
                    op.op_id
                ));
            }
        }
    }
}

fn check_body(
    op: &OpMeta,
    case: &Case,
    plan: &gea_raw::PlannedRequest,
    problems: &mut Vec<String>,
) {
    let Some(b) = op.body else {
        // An operation with no request body must not acquire one, and must not announce a
        // content type for a body it will never send.
        if plan.content_type().is_some() {
            problems.push(format!(
                "{}: no body, yet content-type {:?}",
                op.op_id,
                plan.content_type()
            ));
        }
        if plan.body.is_some() {
            problems
                .push(format!("{}: no body declared, yet {:?} was planned", op.op_id, plan.body));
        }
        if plan.describe().contains("content-type:") {
            problems.push(format!("{}: describe() announces a content type", op.op_id));
        }
        return;
    };

    if plan.content_type() != Some(b.content_type) {
        problems.push(format!(
            "{}: content-type {:?}, expected {:?}",
            op.op_id,
            plan.content_type(),
            b.content_type
        ));
    }

    // Two operations declare a body with no flattenable fields at all. The optional one plans no
    // body (there is nothing to put in it) while still reporting a content type; the required one
    // sends `{}` so the server answers with a field-level 422 naming what is missing.
    if !b.required && case.body.is_empty() {
        if plan.body.is_some() {
            problems.push(format!(
                "{}: optional body with nothing to supply, yet {:?} was planned",
                op.op_id, plan.body
            ));
        }
        return;
    }

    let Some(body) = &plan.body else {
        problems.push(format!("{}: {} fields supplied, yet no body", op.op_id, case.body.len()));
        return;
    };

    for (pointer, want) in &case.body {
        match body.pointer(pointer) {
            Some(got) if got == want => {}
            got => {
                problems.push(format!("{}: body {pointer} is {got:?}, expected {want:?}", op.op_id))
            }
        }
    }
    // Stated separately from the loop above even though the values overlap, because "a required
    // field reached the body" is the property, and it must not become vacuous if the synthesis
    // above ever stops supplying optional fields.
    for f in b.fields.iter().filter(|f| f.required) {
        if body.pointer(f.pointer).is_none() {
            problems.push(format!("{}: required body field {} is absent", op.op_id, f.pointer));
        }
    }
    if !plan.describe().contains(&format!("content-type: {}", b.content_type)) {
        problems.push(format!("{}: describe() omits content-type {}", op.op_id, b.content_type));
    }
}

// ------------------------------------------------------------------------ the tests

/// A real file on disk, because `ValueTy::File` names something the binary will later open and
/// stream; a path to nowhere would let a binding that silently dropped the upload pass.
fn upload_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("attachment-sentinel.bin");
    std::fs::write(&path, b"gea raw contract upload fixture").expect("a writable temp dir");
    path
}

/// Bug this prevents: an operation that builds and parses perfectly while composing the wrong
/// request — a query key under its flag name instead of its wire name, a `PathLike` parameter
/// percent-encoded into a 404, a body integer sent as a quoted string, a content type announced
/// for a body that will never be sent. None of those is visible to a `--help` smoke test, and
/// all of them arrive with a specification bump rather than with a code change.
#[test]
fn every_operation_composes_the_request_its_metadata_describes() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let upload = upload_fixture(dir.path());

    let excused: BTreeMap<&str, &str> = UNSYNTHESIZABLE.iter().copied().collect();
    let mut failures: Vec<String> = Vec::new();
    let mut stale: Vec<&str> = Vec::new();
    let mut exercised: Vec<&'static str> = Vec::new();

    for op in OPS {
        let problems = check(op, &upload);
        match (problems.is_empty(), excused.get(op.op_id)) {
            (true, None) => exercised.push(op.op_id),
            (true, Some(_)) => stale.push(op.op_id),
            (false, Some(_)) => {}
            (false, None) => failures.extend(problems),
        }
    }

    // Written before the assertions: the ratchet must see what actually ran, and it treats any
    // gap as a hard failure of its own.
    emit_journal(&exercised);

    assert!(
        stale.is_empty(),
        "these operations are listed in UNSYNTHESIZABLE but pass — delete their entries: {stale:?}"
    );
    assert!(
        failures.is_empty(),
        "{} of {} operations composed the wrong request:\n{}",
        failures.len(),
        OPS.len(),
        failures.join("\n")
    );
    assert_eq!(
        exercised.len(),
        OPS.len() - UNSYNTHESIZABLE.len(),
        "every operation must be exercised, not skipped"
    );
}

/// Bug this prevents: the table losing a shape this file knows how to check, so the matching
/// assertion keeps passing over nothing at all.
///
/// A vacuous assertion is worse than a missing one — it reads as coverage. If a spec bump ever
/// removes the last `PathLike` parameter or the last `Json` body field, that is a fact worth
/// noticing deliberately rather than discovering when the bug it guarded comes back.
#[test]
fn the_table_still_contains_every_shape_this_file_knows_how_to_check() {
    let mut path_like = 0usize;
    let mut uploads = 0usize;
    let mut repeatable_query = 0usize;
    let mut renamed_query = 0usize;
    let mut query_types: BTreeSet<&str> = BTreeSet::new();
    let mut body_types: BTreeSet<&str> = BTreeSet::new();
    let mut required_body_fields = 0usize;
    let mut bodyless = 0usize;

    for op in OPS {
        for p in op.path_params() {
            if p.encoding == PathEncoding::PathLike {
                path_like += 1;
            }
        }
        for p in op.query_params() {
            query_types.insert(ty_name(p.ty));
            if p.repeatable {
                repeatable_query += 1;
            }
            // `limit` is the collision that really happens: the engine reserves `--limit` for a
            // total item cap on paginated operations, so the API's per-page one becomes
            // `--per-page` while still travelling as `limit`.
            if p.wire == "limit" {
                renamed_query += 1;
            }
        }
        for p in op.form_params() {
            if p.ty == ValueTy::File {
                uploads += 1;
            }
        }
        match op.body {
            None => bodyless += 1,
            Some(b) => {
                for f in b.fields {
                    body_types.insert(ty_name(f.ty));
                    if f.required {
                        required_body_fields += 1;
                    }
                }
            }
        }
    }

    assert!(path_like >= 1, "no PathLike path parameter left; the 404 assertion proves nothing");
    assert!(uploads >= 1, "no File form parameter left; the upload assertion proves nothing");
    assert!(repeatable_query >= 1, "no repeatable query parameter; order proves nothing");
    assert!(renamed_query >= 1, "no per-page/limit rename left; the wire-name check is vacuous");
    assert!(required_body_fields >= 1, "no required body field left to assert on");
    assert!(bodyless >= 1, "no body-free operation left; the no-content-type check is vacuous");
    for ty in ["string", "int", "bool", "datetime"] {
        assert!(query_types.contains(ty), "no {ty} query parameter left: {query_types:?}");
    }
    for ty in ["string", "int", "bool", "list", "json"] {
        assert!(body_types.contains(ty), "no {ty} body field left: {body_types:?}");
    }
}

fn ty_name(ty: ValueTy) -> &'static str {
    match ty {
        ValueTy::Bool => "bool",
        ValueTy::Int => "int",
        ValueTy::Float => "float",
        ValueTy::Str => "string",
        ValueTy::DateTime => "datetime",
        ValueTy::List => "list",
        ValueTy::Json => "json",
        ValueTy::File => "file",
    }
}

/// Bug this prevents: the contract test feeding `2019-08-07T06:05:04.999Z`-shaped nonsense into
/// every `DateTime` parameter and proving only that `bind` copies strings.
///
/// The API rejects a malformed timestamp, so a sentinel that is not a real RFC 3339 instant would
/// make all 41 `DateTime` fields covered on paper and untested in fact.
#[test]
fn every_synthesized_timestamp_is_real_rfc3339() {
    let mut checked = 0usize;
    for op in OPS {
        let datetimes =
            op.params.iter().filter(|p| p.ty == ValueTy::DateTime).map(|p| p.wire).chain(
                op.body
                    .iter()
                    .flat_map(|b| b.fields)
                    .filter(|f| f.ty == ValueTy::DateTime)
                    .map(|f| f.flag),
            );
        for name in datetimes {
            for nth in 1..=2 {
                let text = sentinel_timestamp(name, nth);
                assert!(
                    text.parse::<jiff::Timestamp>().is_ok(),
                    "{name}: {text:?} is not a timestamp a Gitea would accept"
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 2, "no DateTime field left in the table to validate against");
}

/// Records which operations the contract plane actually exercised, and only when asked to.
///
/// `cargo xtask coverage-check` treats a gap here as a hard failure rather than a budget, since
/// this plane is a loop over `OPS` and can only be incomplete through a bug in this file. With
/// `GEA_COVERAGE_DIR` unset — the ordinary `mise run test` run — nothing is written at all: an
/// unconditional write would fail on a read-only checkout, and a partial journal read by the
/// ratchet would understate coverage, which is the one direction a ratchet must never move.
fn emit_journal(exercised: &[&str]) {
    let Some(dir) = std::env::var_os("GEA_COVERAGE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Per-pid, because `cargo nextest` gives every test its own process and a shared journal
    // would interleave writes. The ratchet unions them.
    let path = dir.join(format!("contract-{}.jsonl", std::process::id()));
    let site = format!("{}:{}", file!(), line!());
    let mut buf = String::with_capacity(exercised.len() * 96);
    for id in exercised {
        let line = serde_json::json!({ "kind": "raw", "id": id, "site": site });
        buf.push_str(&line.to_string());
        buf.push('\n');
    }
    let _ = std::fs::write(&path, buf);
}

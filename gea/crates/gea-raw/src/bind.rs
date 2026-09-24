//! [`clap::ArgMatches`] to a described request.
//!
//! This module performs **no HTTP**. It returns a [`PlannedRequest`] — method, rendered path,
//! ordered query pairs, body JSON — and the binary executes it. That split is what makes
//! layer 2 unit-testable without a transport, and it is what lets `--dry-run` be a `return`
//! rather than a special case threaded through the client.
//!
//! Two decisions here are load-bearing:
//!
//! * **Path parameters merge with precedence flag > positional > context > error.** The flag
//!   wins because it is the more explicit form; a positional is easy to get in the wrong
//!   order. If both are given and *disagree*, that is a mistake worth naming — clap's native
//!   answer would be a baffling "unexpected argument".
//! * **Path rendering honours per-parameter encoding.** `Segment` encodes `/`, `PathLike` does
//!   not. Blanket-encoding turns `get-contents o r src/main.rs` into a request for a file
//!   literally named `src/main.rs` in the repository root, i.e. a 404 for every nested file in
//!   every repository.

use std::any::Any;
use std::io::Read;
use std::path::PathBuf;

use clap::ArgMatches;
use gitea_client::meta_types::{CtxFill, OpMeta, ParamMeta, PathEncoding, ValueTy};
use gitea_core::error::{Result, usage};
use gitea_core::http::encode;
use gitea_core::types::RepoSlug;
use serde_json::{Map, Value};

use crate::build::{
    ID_BODY_FILE, ID_DRY_RUN, ID_LIMIT, ID_PAGINATE, body_id, form_id, path_flag_id, path_pos_id,
    query_id,
};

/// A request, fully described and not yet sent.
///
/// `op` is carried whole rather than copied field by field, so the binary can reach `produces`,
/// `scope`, and `pagination` for output handling and error classification without a second
/// lookup.
#[derive(Debug, Clone)]
pub struct PlannedRequest {
    pub op: &'static OpMeta,
    /// Path with every `{param}` substituted and percent-encoded, relative to the API base
    /// path. Guaranteed to contain no `{`.
    pub path: String,
    /// **Ordered**, not a map: repeated keys are legal in this API (`labels=bug&labels=ci`),
    /// and a map would silently drop all but one.
    pub query: Vec<(String, String)>,
    /// The JSON request body, if this operation takes one and anything was supplied.
    pub body: Option<Value>,
    /// `multipart/form-data` scalar fields, for the three upload operations.
    pub form: Vec<(&'static str, String)>,
    /// `multipart/form-data` file fields: the wire name and the local path to stream.
    pub uploads: Vec<(&'static str, PathBuf)>,
    pub paginate: bool,
    /// `--limit`: a cap on total items across pages, never a per-page size.
    pub limit: Option<u64>,
    pub dry_run: bool,
}

impl PlannedRequest {
    pub fn method(&self) -> &'static str {
        self.op.method
    }

    pub fn content_type(&self) -> Option<&'static str> {
        self.op.body.map(|b| b.content_type)
    }

    /// `path?query`, or just `path` when there is no query.
    pub fn path_and_query(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        format!("{}?{}", self.path, encode::query_string(self.query.iter().map(|(k, v)| (k, v))))
    }

    /// What `--dry-run` prints: the assembled method, URL, and body.
    pub fn describe(&self) -> String {
        let mut s = format!("{} {}\n", self.method(), self.path_and_query());
        for (name, path) in &self.uploads {
            s.push_str(&format!("upload: {name}={}\n", path.display()));
        }
        for (name, value) in &self.form {
            s.push_str(&format!("form: {name}={value}\n"));
        }
        if let Some(b) = &self.body {
            if let Some(ct) = self.content_type() {
                s.push_str(&format!("content-type: {ct}\n"));
            }
            s.push_str(&serde_json::to_string_pretty(b).unwrap_or_else(|_| b.to_string()));
            s.push('\n');
        }
        s
    }
}

/// Turn one leaf command's matches into a request description.
///
/// `ctx` is the *already resolved* repository, passed in rather than resolved here: resolution
/// shells out to git and reads config, which this crate deliberately cannot do. `stdin` is
/// injected for the same reason — it makes `--body-file -` testable.
pub fn bind(
    op: &'static OpMeta,
    m: &ArgMatches,
    ctx: Option<&RepoSlug>,
    stdin: &mut dyn Read,
) -> Result<PlannedRequest> {
    let path = render_path(op, m, ctx)?;

    let mut query = Vec::new();
    for p in op.query_params() {
        for v in matched_strings(m, &query_id(p.flag), p.ty) {
            query.push((p.wire.to_owned(), v));
        }
    }

    let mut form = Vec::new();
    let mut uploads = Vec::new();
    for p in op.form_params() {
        let id = form_id(p.flag);
        if p.ty == ValueTy::File {
            if let Some(v) = matched_one::<String>(m, &id) {
                uploads.push((p.wire, PathBuf::from(v)));
            }
        } else {
            for v in matched_strings(m, &id, p.ty) {
                form.push((p.wire, v));
            }
        }
    }

    let body = build_body(op, m, stdin)?;

    Ok(PlannedRequest {
        op,
        path,
        query,
        body,
        form,
        uploads,
        paginate: matched_one::<bool>(m, ID_PAGINATE).unwrap_or(false),
        limit: matched_one::<u64>(m, ID_LIMIT),
        dry_run: matched_one::<bool>(m, ID_DRY_RUN).unwrap_or(false),
    })
}

// ------------------------------------------------------------------- path rendering

/// Substitute and encode every `{param}` in the path template.
///
/// The template is walked rather than split on `/`, which is what makes the two dotted paths
/// (`/pulls/{index}.{diffType}`) work: the `.` is ordinary literal text between two
/// placeholders, so `diffType` is never encoded into a filename.
fn render_path(op: &'static OpMeta, m: &ArgMatches, ctx: Option<&RepoSlug>) -> Result<String> {
    let mut out = String::with_capacity(op.path.len() + 16);
    let mut rest = op.path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| usage(format!("{}: malformed path template {:?}", op.op_id, op.path)))?
            + open;
        let wire = &rest[open + 1..close];
        let p = op.path_params().find(|p| p.wire == wire).ok_or_else(|| {
            usage(format!(
                "{}: path needs {{{wire}}} but the operation declares no such parameter",
                op.op_id
            ))
        })?;
        let value = resolve_path_param(op, p, m, ctx)?;
        out.push_str(&match p.encoding {
            PathEncoding::Segment => encode::seg(&value),
            PathEncoding::PathLike => encode::path_like(&value),
        });
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    // A leftover brace means a placeholder was never substituted, and the request would go to
    // a URL containing a literal `{owner}`. Both encoders escape `{`, so a *value* cannot put
    // one here; only a rendering bug can.
    assert!(!out.contains('{'), "unsubstituted placeholder in rendered path {out:?}");
    Ok(out)
}

/// Merge precedence: flag > positional > context > error.
fn resolve_path_param(
    op: &'static OpMeta,
    p: &'static ParamMeta,
    m: &ArgMatches,
    ctx: Option<&RepoSlug>,
) -> Result<String> {
    let flag = matched_one::<String>(m, &path_flag_id(p.flag));
    let positional = matched_one::<String>(m, &path_pos_id(p.flag));
    match (flag, positional) {
        (Some(f), Some(pos)) if f != pos => Err(usage(format!(
            "{} was given twice with different values: {:?} positionally but {:?} as --{} — pass it only one way",
            p.wire, pos, f, p.flag
        ))),
        (Some(f), _) => Ok(f),
        (None, Some(pos)) => Ok(pos),
        (None, None) => match (p.ctx_fill, ctx) {
            (Some(CtxFill::Owner), Some(slug)) => Ok(slug.owner.clone()),
            (Some(CtxFill::Repo), Some(slug)) => Ok(slug.name.clone()),
            _ => Err(usage(missing_path_param(op, p))),
        },
    }
}

fn missing_path_param(op: &'static OpMeta, p: &'static ParamMeta) -> String {
    let how = if p.ctx_fill.is_some() {
        ", or -R owner/repo, or run inside a clone of the repository"
    } else {
        ""
    };
    let name = crate::build::value_name(p.wire);
    format!(
        "gea raw {} {} needs {}: pass it as <{name}> or --{} <{name}>{how}",
        op.group, op.command, p.wire, p.flag,
    )
}

// ------------------------------------------------------------------------- the body

/// `--body-file` supplies the base object; body-field flags override it by JSON pointer.
fn build_body(op: &'static OpMeta, m: &ArgMatches, stdin: &mut dyn Read) -> Result<Option<Value>> {
    let mut overrides: Vec<(&'static str, Value)> = Vec::new();
    if let Some(b) = op.body {
        for f in b.fields {
            if let Some(v) = matched_json(m, &body_id(f.flag), f.ty, f.flag)? {
                overrides.push((f.pointer, v));
            }
        }
    }
    let body_file = matched_one::<String>(m, ID_BODY_FILE);
    let required = op.body.is_some_and(|b| b.required);

    match (&body_file, overrides.is_empty()) {
        // Nothing supplied. A required body still has to be *something*, and `{}` gets a
        // field-level 422 from the server naming what is missing — a better error than any we
        // could synthesise from a spec that only marks 39 definitions `required`.
        (None, true) => Ok(required.then(|| Value::Object(Map::new()))),
        _ => {
            let base = match &body_file {
                Some(p) => crate::bodyfile::load(p, stdin)?,
                None => Value::Object(Map::new()),
            };
            Ok(Some(crate::bodyfile::merge(base, &overrides)?))
        }
    }
}

// --------------------------------------------------------------- reading the matches

/// `try_get_*` rather than `get_*` throughout: `get_one` panics when an id is absent from the
/// command, and the binary can hand us matches from a subtree we did not build.
fn matched_one<T>(m: &ArgMatches, id: &str) -> Option<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    m.try_get_one::<T>(id).ok().flatten().cloned()
}

fn matched_many<T>(m: &ArgMatches, id: &str) -> Vec<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    m.try_get_many::<T>(id).ok().flatten().map(|vs| vs.cloned().collect()).unwrap_or_default()
}

/// Read values back as strings for the query string.
///
/// The `ValueTy` → `T` mapping must match [`crate::build`]'s value parsers exactly, because
/// clap panics on a type mismatch. Both live in one small function each for that reason.
fn matched_strings(m: &ArgMatches, id: &str, ty: ValueTy) -> Vec<String> {
    match ty {
        ValueTy::Bool => matched_many::<bool>(m, id).iter().map(bool::to_string).collect(),
        ValueTy::Int => matched_many::<i64>(m, id).iter().map(i64::to_string).collect(),
        ValueTy::Float => matched_many::<f64>(m, id).iter().map(f64::to_string).collect(),
        _ => matched_many::<String>(m, id),
    }
}

/// Read one body field back as JSON, preserving its type: a body `int` must serialize as a
/// number, not a quoted string, or the server rejects it.
fn matched_json(m: &ArgMatches, id: &str, ty: ValueTy, flag: &str) -> Result<Option<Value>> {
    Ok(match ty {
        ValueTy::Bool => matched_one::<bool>(m, id).map(Value::from),
        ValueTy::Int => matched_one::<i64>(m, id).map(Value::from),
        ValueTy::Float => matched_one::<f64>(m, id).map(Value::from),
        ValueTy::List => {
            let vs = matched_many::<String>(m, id);
            (!vs.is_empty()).then(|| Value::Array(vs.into_iter().map(Value::from).collect()))
        }
        ValueTy::Json => match matched_one::<String>(m, id) {
            Some(s) => Some(
                serde_json::from_str(&s)
                    .map_err(|e| usage(format!("--{flag} expects JSON: {e}")))?,
            ),
            None => None,
        },
        _ => matched_one::<String>(m, id).map(Value::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, op};
    use std::io::Cursor;

    fn plan(group: &str, command: &str, words: &[&str]) -> Result<PlannedRequest> {
        plan_with(group, command, words, None, "")
    }

    fn plan_with(
        group: &str,
        command: &str,
        words: &[&str],
        ctx: Option<&RepoSlug>,
        stdin: &str,
    ) -> Result<PlannedRequest> {
        let m = fixtures::matches(group, command, words);
        let mut input = Cursor::new(stdin.as_bytes().to_vec());
        bind(op(group, command), &m, ctx, &mut input)
    }

    // ----------------------------------------------------- the five merge shapes

    #[test]
    fn merge_positional_only() {
        let r = plan("repo", "get", &["o", "r"]).unwrap();
        assert_eq!(r.path, "/repos/o/r");
    }

    #[test]
    fn merge_flag_only() {
        let r = plan("repo", "get", &["--owner", "o", "--repo", "r"]).unwrap();
        assert_eq!(r.path, "/repos/o/r");
    }

    #[test]
    fn merge_both_agreeing_is_fine() {
        let r = plan("repo", "get", &["o", "r", "--owner", "o", "--repo", "r"]).unwrap();
        assert_eq!(r.path, "/repos/o/r");
    }

    /// clap would call this "unexpected argument", which tells the user nothing. Name both
    /// values and say to pick one.
    #[test]
    fn merge_both_disagreeing_names_both_values() {
        let e = plan("repo", "get", &["a", "r", "--owner", "b"]).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("owner was given twice"), "{msg}");
        assert!(msg.contains("\"a\""), "{msg}");
        assert!(msg.contains("\"b\""), "{msg}");
        assert!(msg.contains("--owner"), "{msg}");
    }

    #[test]
    fn merge_neither_falls_back_to_repository_context() {
        let slug = RepoSlug::new("ctxowner", "ctxrepo");
        let r = plan_with("repo", "get", &[], Some(&slug), "").unwrap();
        assert_eq!(r.path, "/repos/ctxowner/ctxrepo");
    }

    /// The flag is the explicit form, so it beats a positional that may simply be in the
    /// wrong place, and it beats context.
    #[test]
    fn a_flag_beats_context() {
        let slug = RepoSlug::new("ctxowner", "ctxrepo");
        let r = plan_with("repo", "get", &["--owner", "flagowner"], Some(&slug), "").unwrap();
        assert_eq!(r.path, "/repos/flagowner/ctxrepo");
    }

    #[test]
    fn merge_neither_and_no_context_explains_all_the_ways_to_supply_it() {
        let e = plan("repo", "get", &[]).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("needs owner"), "{msg}");
        assert!(msg.contains("--owner"), "{msg}");
        assert!(msg.contains("-R owner/repo"), "{msg}");
    }

    /// A parameter with no `ctx_fill` cannot be conjured from context, and the message must
    /// not pretend otherwise.
    #[test]
    fn a_non_context_param_is_not_offered_a_repo_flag_remedy() {
        let slug = RepoSlug::new("o", "r");
        let e = plan_with("issue", "create-comment", &[], Some(&slug), "").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("needs index"), "{msg}");
        assert!(!msg.contains("-R owner/repo"), "{msg}");
    }

    // ------------------------------------------------------------- path encoding

    /// The `get-contents` 404: a `PathLike` parameter must keep its slashes.
    #[test]
    fn path_like_params_keep_their_slashes() {
        let r = plan("repo", "get-contents", &["o", "r", "src/main.rs"]).unwrap();
        assert_eq!(r.path, "/repos/o/r/contents/src/main.rs");
    }

    /// The mirror-image bug: a `Segment` parameter containing `/` would add a path segment and
    /// match a different route.
    #[test]
    fn segment_params_encode_their_slashes() {
        let r = plan("repo", "get", &["a/b", "r"]).unwrap();
        assert_eq!(r.path, "/repos/a%2Fb/r");
    }

    /// `/pulls/{index}.{diffType}` is the path a naive `split('/')` renderer breaks: the `.`
    /// is literal text between two placeholders.
    #[test]
    fn the_dotted_path_renders_both_placeholders_around_the_literal_dot() {
        let r = plan("repo", "download-pull-diff-or-patch", &["o", "r", "1", "diff"]).unwrap();
        assert_eq!(r.path, "/repos/o/r/pulls/1.diff");
    }

    /// If a placeholder ever survives rendering we would request a URL containing a literal
    /// `{owner}`. Nothing a user types can cause it — both encoders escape `{` — and this
    /// asserts that.
    #[test]
    fn a_rendered_path_never_contains_a_brace() {
        for (group, command, words) in fixtures::EVERY_OP_INVOCATION {
            let r = plan(group, command, words).unwrap();
            assert!(!r.path.contains('{'), "{}: {}", command, r.path);
            assert!(!r.path.contains('}'), "{}: {}", command, r.path);
        }
        // Even when a value is itself brace-shaped.
        let r = plan("repo", "get", &["{owner}", "r"]).unwrap();
        assert_eq!(r.path, "/repos/%7Bowner%7D/r");
    }

    // -------------------------------------------------------------------- query

    /// Repeated keys are legal, so the query is an ordered `Vec`. A map would keep one label.
    #[test]
    fn repeatable_query_params_keep_every_value_in_order() {
        let r = plan(
            "issue",
            "list",
            &["o", "r", "--labels", "bug", "--labels", "ci", "--state", "closed"],
        )
        .unwrap();
        assert_eq!(
            r.query,
            vec![
                ("state".to_owned(), "closed".to_owned()),
                ("labels".to_owned(), "bug".to_owned()),
                ("labels".to_owned(), "ci".to_owned()),
            ]
        );
        assert_eq!(r.path_and_query(), "/repos/o/r/issues?state=closed&labels=bug&labels=ci");
    }

    #[test]
    fn the_per_page_flag_still_sends_the_wire_name_limit() {
        let r = plan("issue", "list", &["o", "r", "--per-page", "50", "--paginate"]).unwrap();
        assert_eq!(r.query, vec![("limit".to_owned(), "50".to_owned())]);
        assert!(r.paginate);
        assert_eq!(r.limit, None);
    }

    #[test]
    fn the_limit_flag_is_a_total_cap_not_a_query_param() {
        let r = plan("issue", "list", &["o", "r", "--limit", "300"]).unwrap();
        assert_eq!(r.limit, Some(300));
        assert!(r.query.is_empty());
    }

    #[test]
    fn an_optional_value_bool_flag_defaults_to_true_and_accepts_false() {
        let r = plan("issue", "list", &["o", "r", "--mine"]).unwrap();
        assert_eq!(r.query, vec![("mine".to_owned(), "true".to_owned())]);
        let r = plan("issue", "list", &["o", "r", "--mine=false"]).unwrap();
        assert_eq!(r.query, vec![("mine".to_owned(), "false".to_owned())]);
    }

    // --------------------------------------------------------------------- body

    #[test]
    fn body_flags_are_typed_not_stringified() {
        let r = plan(
            "repo",
            "create-pull-request",
            &["o", "r", "--title", "hi", "--draft", "--assignees", "a", "--assignees", "b"],
        )
        .unwrap();
        let body = r.body.unwrap();
        assert_eq!(body["title"], Value::from("hi"));
        assert_eq!(body["draft"], Value::Bool(true));
        assert_eq!(body["assignees"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn an_operation_with_no_body_and_no_flags_plans_no_body() {
        let r = plan("repo", "get", &["o", "r"]).unwrap();
        assert!(r.body.is_none());
    }

    /// A required body with nothing supplied still sends `{}`, so the server answers with a
    /// field-level 422 that names what is missing.
    #[test]
    fn a_required_body_with_nothing_supplied_is_an_empty_object() {
        let r = plan("repo", "create-pull-request", &["o", "r"]).unwrap();
        assert_eq!(r.body, Some(serde_json::json!({})));
    }

    /// The composition that makes nested bodies usable: pipe in a body you already have, and
    /// change one field on the command line.
    #[test]
    fn a_body_file_from_stdin_is_overridden_field_by_field_by_flags() {
        let r = plan_with(
            "repo",
            "create-pull-request",
            &["o", "r", "--body-file", "-", "--title", "from the flag"],
            None,
            r#"{"title": "from the file", "base": "main", "labels": [1, 2]}"#,
        )
        .unwrap();
        assert_eq!(
            r.body.unwrap(),
            serde_json::json!({"title": "from the flag", "base": "main", "labels": [1, 2]})
        );
    }

    #[test]
    fn a_non_object_body_file_plus_a_flag_says_which_flag_has_nowhere_to_go() {
        let e = plan_with(
            "repo",
            "create-pull-request",
            &["o", "r", "--body-file", "-", "--title", "x"],
            None,
            "[1, 2]",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("an array"), "{e}");
        assert!(e.contains("--title"), "{e}");
    }

    #[test]
    fn dry_run_describes_the_whole_request() {
        let r =
            plan("repo", "create-pull-request", &["o", "r", "--title", "hi", "--dry-run"]).unwrap();
        assert!(r.dry_run);
        let d = r.describe();
        assert!(d.starts_with("POST /repos/o/r/pulls\n"), "{d}");
        assert!(d.contains("\"title\": \"hi\""), "{d}");
        assert!(d.contains("content-type: application/json"), "{d}");
    }

    // ------------------------------------------------------------------ uploads

    #[test]
    fn a_file_form_param_becomes_an_upload_not_a_body_field() {
        let r = plan(
            "repo",
            "create-release-attachment",
            &["o", "r", "7", "--attachment", "./dist/x.zip", "--name", "x.zip", "--checksum", "ab"],
        )
        .unwrap();
        assert_eq!(r.path, "/repos/o/r/releases/7/assets");
        assert_eq!(r.uploads, vec![("attachment", PathBuf::from("./dist/x.zip"))]);
        // `name` is a query parameter here, not a form field: form and query are separate
        // destinations even on a multipart operation.
        assert_eq!(r.query, vec![("name".to_owned(), "x.zip".to_owned())]);
        assert_eq!(r.form, vec![("checksum", "ab".to_owned())]);
        assert!(r.body.is_none());
    }
}

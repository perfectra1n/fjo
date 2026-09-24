//! `cargo xtask spec-diff --tag v1.27.3` / `--branch main` / `--file path.json`.
//!
//! A *semantic* diff between the vendored spec and an upstream one, rendered as Markdown.
//!
//! The left-hand side defaults to the vendored spec and can be moved with `--baseline-tag`,
//! `--baseline-branch` or `--baseline-file`. That is what answers the question the vendored
//! comparison cannot: `--baseline-tag v1.27.3 --branch main` reports what is on upstream's
//! development branch **and in no release yet**. A change in that report is a preview; a
//! change in `--tag v1.27.3` is already shipped and a bump will bring it in. Same report,
//! opposite meanings, and the spec-drift workflow labels the issue with both.
//! `git diff` on two 850 KB JSON files answers "did anything change"; this answers the
//! question a maintainer actually has — *which operations changed, and did a response shape
//! move* — because a changed response is what turns into a deserialization failure or a
//! silently-dropped field in `gitea-model`.
//!
//! The upstream document goes through exactly the same [`crate::update_spec::canonicalize`]
//! step as the vendored one, with the vendored version substituted, so `info.version` and
//! key order never show up as differences. Everything else is compared on the canonical JSON
//! rather than on [`crate::swagger::Spec`]: the typed model deliberately drops keys it does
//! not understand, and a diff that only sees what the model sees would miss precisely the
//! upstream additions that need a human to look at them.
//!
//! The report is consumed two ways, and both matter for the shape of the output:
//!
//! - `.github/workflows/spec-drift.yaml` pastes it into an issue or a pull-request body, so it
//!   is Markdown with stable headings a reader can scan;
//! - the same workflow branches on the exit code, so [`Report::has_drift`] maps to a distinct
//!   exit status ([`DRIFT_EXIT_CODE`]) rather than being folded into "error".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Result;

/// Exit status when the two specs differ. Not 1, so a workflow can tell "drift" from
/// "curl failed" or "the spec is not something we can parse" without grepping output.
pub const DRIFT_EXIT_CODE: u8 = 3;

/// Where the upstream document comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A Gitea release tag, e.g. `v1.27.3`.
    Tag(String),
    /// A Gitea branch, e.g. `gitea` (the development branch) or `v16.0/gitea`.
    Branch(String),
    /// An already-fetched `v1_json.tmpl` (or canonical JSON) on disk.
    File(PathBuf),
}

/// One side of the comparison, as the report describes it.
#[derive(Debug, Clone)]
pub struct Side {
    /// The first column of the header table: `vendored`, `baseline` or `upstream`. With a
    /// `--baseline-*` the left-hand side is no longer the vendored spec, and the table has to
    /// say so or the report reads as a claim about `spec/`.
    pub role: String,
    pub label: String,
    pub sha256: String,
}

/// Everything the diff found, grouped the way the Markdown renders it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// `METHOD /path (operationId)` for operations only the upstream spec has.
    pub ops_added: Vec<String>,
    pub ops_removed: Vec<String>,
    /// Operation → human-readable lines describing what moved.
    pub ops_changed: BTreeMap<String, Vec<String>>,
    pub defs_added: Vec<String>,
    pub defs_removed: Vec<String>,
    pub defs_changed: BTreeMap<String, Vec<String>>,
    /// Shared `#/responses/...` entries. A change here is reported once, with the operations
    /// that reference it, rather than once per operation.
    pub responses_added: Vec<String>,
    pub responses_removed: Vec<String>,
    pub responses_changed: BTreeMap<String, Vec<String>>,
    /// Problems the generator would have with the upstream spec, e.g. a construct the
    /// Swagger model does not handle. Drift, but the kind that needs code, not a bump.
    pub warnings: Vec<String>,
}

impl Report {
    pub fn has_drift(&self) -> bool {
        *self != Report::default()
    }

    /// The Markdown document. `old` and `new` name the two sides in the header.
    pub fn render(&self, old: &Side, new: &Side) -> String {
        let mut md = String::from("# Gitea API spec drift\n\n");
        md.push_str("| side | source | sha256 (canonical) |\n|---|---|---|\n");
        md.push_str(&format!("| {} | {} | `{}` |\n", old.role, old.label, old.sha256));
        md.push_str(&format!("| {} | {} | `{}` |\n\n", new.role, new.label, new.sha256));

        if !self.has_drift() {
            md.push_str("No differences.\n");
            return md;
        }

        md.push_str(&format!("**Summary:** {}.\n\n", self.summary()));
        if !self.warnings.is_empty() {
            md.push_str("## Generator warnings\n\n");
            for w in &self.warnings {
                md.push_str(&format!("- {w}\n"));
            }
            md.push('\n');
        }
        section(
            &mut md,
            "Operations",
            &self.ops_added,
            &self.ops_removed,
            &self.ops_changed,
            op_md,
        );
        section(
            &mut md,
            "Shared responses",
            &self.responses_added,
            &self.responses_removed,
            &self.responses_changed,
            code_md,
        );
        section(
            &mut md,
            "Definitions",
            &self.defs_added,
            &self.defs_removed,
            &self.defs_changed,
            code_md,
        );
        md
    }

    /// `1 operation added, 2 changed; 3 definitions changed` — zero counts are left out.
    fn summary(&self) -> String {
        let mut groups = Vec::new();
        for (noun, added, removed, changed) in [
            ("operation", &self.ops_added, &self.ops_removed, &self.ops_changed),
            (
                "shared response",
                &self.responses_added,
                &self.responses_removed,
                &self.responses_changed,
            ),
            ("definition", &self.defs_added, &self.defs_removed, &self.defs_changed),
        ] {
            let mut parts = Vec::new();
            for (n, verb) in
                [(added.len(), "added"), (removed.len(), "removed"), (changed.len(), "changed")]
            {
                let with_noun = parts.is_empty();
                push_count(&mut parts, noun, n, verb, with_noun);
            }
            if !parts.is_empty() {
                groups.push(parts.join(", "));
            }
        }
        if !self.warnings.is_empty() {
            groups.push(plural(self.warnings.len(), "generator warning"));
        }
        groups.join("; ")
    }
}

/// `1 definition added` for the first part of a group, then just `2 removed`.
fn push_count(parts: &mut Vec<String>, noun: &str, n: usize, verb: &str, with_noun: bool) {
    if n == 0 {
        return;
    }
    if with_noun {
        parts.push(format!("{} {verb}", plural(n, noun)));
    } else {
        parts.push(format!("{n} {verb}"));
    }
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// One `## …` section with `### Added` / `### Removed` / `### Changed` beneath it, each
/// present only when non-empty. `name_md` formats the item name for Markdown.
fn section(
    md: &mut String,
    title: &str,
    added: &[String],
    removed: &[String],
    changed: &BTreeMap<String, Vec<String>>,
    name_md: fn(&str) -> String,
) {
    if added.is_empty() && removed.is_empty() && changed.is_empty() {
        return;
    }
    md.push_str(&format!("## {title}\n\n"));
    for (heading, items) in [("Added", added), ("Removed", removed)] {
        if items.is_empty() {
            continue;
        }
        md.push_str(&format!("### {heading}\n\n"));
        for item in items {
            md.push_str(&format!("- {}\n", name_md(item)));
        }
        md.push('\n');
    }
    if !changed.is_empty() {
        md.push_str("### Changed\n\n");
        for (name, lines) in changed {
            md.push_str(&format!("- {}\n", name_md(name)));
            for line in lines {
                md.push_str(&format!("  - {line}\n"));
            }
        }
        md.push('\n');
    }
}

/// `GET /x (id)` → `` `GET /x` (`id`) ``.
fn op_md(name: &str) -> String {
    match name.rsplit_once(" (") {
        Some((op, id)) => format!("`{op}` (`{}`)", id.trim_end_matches(')')),
        None => format!("`{name}`"),
    }
}

fn code_md(name: &str) -> String {
    format!("`{name}`")
}

impl Source {
    /// The upstream URL, or `None` for a local file.
    pub fn url(&self) -> Option<String> {
        match self {
            Source::Tag(tag) => Some(crate::spec::source_url(tag)),
            Source::Branch(branch) => Some(crate::spec::branch_url(branch)),
            Source::File(_) => None,
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::Tag(tag) => format!("tag `{tag}`"),
            Source::Branch(branch) => format!("branch `{branch}`"),
            Source::File(path) => format!("`{}`", path.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// The diff itself. Everything below works on canonical JSON `Value`s.
// ---------------------------------------------------------------------------

const METHODS: [&str; 7] = ["get", "post", "put", "patch", "delete", "head", "options"];

/// Object entries in key order; empty for anything that is not an object.
fn entries(v: &Value) -> BTreeMap<&str, &Value> {
    v.as_object().map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect()).unwrap_or_default()
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `#/definitions/X`, `string`, `integer/int64`, `array of …`, or `(none)`.
fn describe_schema(schema: &Value) -> String {
    if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
        return r.to_owned();
    }
    let ty = str_of(schema, "type");
    if ty == "array" {
        let items = schema.get("items").unwrap_or(&Value::Null);
        return format!("array of {}", describe_schema(items));
    }
    let format = str_of(schema, "format");
    match (ty.is_empty(), format.is_empty()) {
        (true, _) => "(none)".to_owned(),
        (false, true) => ty.to_owned(),
        (false, false) => format!("{ty}/{format}"),
    }
}

/// A response is either a `$ref` into `#/responses/…` or an inline `schema`.
fn describe_response(response: &Value) -> String {
    match response.get("$ref").and_then(Value::as_str) {
        Some(r) => r.to_owned(),
        None => describe_schema(response.get("schema").unwrap_or(&Value::Null)),
    }
}

/// Non-body parameters carry `type` inline; body parameters carry a `schema`.
fn describe_param(param: &Value) -> String {
    match param.get("schema") {
        Some(schema) => describe_schema(schema),
        None => describe_schema(param),
    }
}

/// Every operation in the document, keyed by `METHOD /path` with its display name
/// (`METHOD /path (operationId)`) alongside. Keyed without the id so an `operationId` rename
/// shows as a change to one operation rather than a removal plus an addition.
fn operations(spec: &Value) -> BTreeMap<String, (String, &Value)> {
    let mut out = BTreeMap::new();
    for (path, item) in entries(spec.get("paths").unwrap_or(&Value::Null)) {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                let key = format!("{} {path}", method.to_ascii_uppercase());
                let display = format!("{key} ({})", str_of(op, "operationId"));
                out.insert(key, (display, op));
            }
        }
    }
    out
}

/// Compares two canonical spec documents.
pub fn diff(old: &Value, new: &Value) -> Report {
    let mut r = Report::default();
    diff_operations(old, new, &mut r);

    let old_responses = entries(old.get("responses").unwrap_or(&Value::Null));
    let new_responses = entries(new.get("responses").unwrap_or(&Value::Null));
    let users = response_users(old, new);
    diff_named(
        &old_responses,
        &new_responses,
        &mut r.responses_added,
        &mut r.responses_removed,
        &mut r.responses_changed,
        |name, o, n| {
            let mut lines = diff_shared_response(o, n);
            if let Some(ops) = users.get(name) {
                let ops: Vec<String> = ops.iter().map(|op| op_md(op)).collect();
                lines.push(format!("used by: {}", ops.join(", ")));
            }
            lines
        },
    );

    let old_defs = entries(old.get("definitions").unwrap_or(&Value::Null));
    let new_defs = entries(new.get("definitions").unwrap_or(&Value::Null));
    diff_named(
        &old_defs,
        &new_defs,
        &mut r.defs_added,
        &mut r.defs_removed,
        &mut r.defs_changed,
        |_, o, n| diff_definition(o, n),
    );

    r.warnings = generator_warnings(new);
    r
}

/// Shared-response name → display names of every operation (in either document) whose
/// responses `$ref` it.
fn response_users(old: &Value, new: &Value) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, (display, op)) in operations(old).into_iter().chain(operations(new)) {
        for (_, response) in entries(op.get("responses").unwrap_or(&Value::Null)) {
            if let Some(name) = str_of(response, "$ref").strip_prefix("#/responses/") {
                let users = out.entry(name.to_owned()).or_default();
                if !users.contains(&display) {
                    users.push(display.clone());
                }
            }
        }
    }
    out
}

/// Walks two name-keyed maps together: names only on one side go to `added`/`removed`,
/// names on both sides whose values differ go through `describe` and, if it has anything
/// to say, into `changed`.
fn diff_named<'a>(
    old: &BTreeMap<&'a str, &'a Value>,
    new: &BTreeMap<&'a str, &'a Value>,
    added: &mut Vec<String>,
    removed: &mut Vec<String>,
    changed: &mut BTreeMap<String, Vec<String>>,
    describe: impl Fn(&str, &Value, &Value) -> Vec<String>,
) {
    for (name, o) in old {
        match new.get(name) {
            None => removed.push((*name).to_owned()),
            Some(n) if o != n => {
                let lines = describe(name, o, n);
                if !lines.is_empty() {
                    changed.insert((*name).to_owned(), lines);
                }
            }
            Some(_) => {}
        }
    }
    for name in new.keys() {
        if !old.contains_key(name) {
            added.push((*name).to_owned());
        }
    }
}

fn diff_operations(old: &Value, new: &Value, r: &mut Report) {
    let old_ops = operations(old);
    let new_ops = operations(new);
    for (key, (display, o)) in &old_ops {
        match new_ops.get(key) {
            None => r.ops_removed.push(display.clone()),
            Some((new_display, n)) if o != n => {
                let lines = diff_operation(o, n);
                if !lines.is_empty() {
                    r.ops_changed.insert(new_display.clone(), lines);
                }
            }
            Some(_) => {}
        }
    }
    for (key, (display, _)) in &new_ops {
        if !old_ops.contains_key(key) {
            r.ops_added.push(display.clone());
        }
    }
}

fn diff_operation(old: &Value, new: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let (old_id, new_id) = (str_of(old, "operationId"), str_of(new, "operationId"));
    if old_id != new_id {
        lines.push(format!("operationId: `{old_id}` → `{new_id}`"));
    }
    lines.extend(diff_params(old, new));
    lines.extend(diff_responses(old, new));
    if lines.is_empty() {
        lines.push("changed (description or metadata only)".to_owned());
    }
    lines
}

/// Parameters keyed by `name (in)` — the pair is what identifies one to a caller.
fn params_by_key(op: &Value) -> BTreeMap<String, &Value> {
    op.get("parameters")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter().map(|p| (format!("{} ({})", str_of(p, "name"), str_of(p, "in")), p)).collect()
        })
        .unwrap_or_default()
}

fn diff_params(old: &Value, new: &Value) -> Vec<String> {
    let (old_params, new_params) = (params_by_key(old), params_by_key(new));
    let mut lines = Vec::new();
    let keys: std::collections::BTreeSet<&String> =
        old_params.keys().chain(new_params.keys()).collect();
    for key in keys {
        let label = param_label(key);
        match (old_params.get(key), new_params.get(key)) {
            (Some(_), None) => lines.push(format!("parameter {label} removed")),
            (None, Some(n)) => {
                lines.push(format!("parameter {label} added: {}", describe_param(n)))
            }
            (Some(o), Some(n)) if o != n => {
                lines.push(format!("parameter {label} changed: {}", param_change(o, n)));
            }
            _ => {}
        }
    }
    lines
}

/// `name (in)` → `` `name` (in) ``.
fn param_label(key: &str) -> String {
    match key.split_once(" (") {
        Some((name, rest)) => format!("`{name}` ({rest}"),
        None => format!("`{key}`"),
    }
}

fn param_change(old: &Value, new: &Value) -> String {
    let mut parts = Vec::new();
    let (o_req, n_req) = (is_required(old), is_required(new));
    if o_req != n_req {
        parts.push(format!("required {o_req} → {n_req}"));
    }
    let (o_ty, n_ty) = (describe_param(old), describe_param(new));
    if o_ty != n_ty {
        parts.push(format!("type {o_ty} → {n_ty}"));
    }
    if parts.is_empty() {
        parts.push("description or metadata only".to_owned());
    }
    parts.join(", ")
}

fn is_required(param: &Value) -> bool {
    param.get("required").and_then(Value::as_bool).unwrap_or(false)
}

fn diff_responses(old: &Value, new: &Value) -> Vec<String> {
    let old_resp = entries(old.get("responses").unwrap_or(&Value::Null));
    let new_resp = entries(new.get("responses").unwrap_or(&Value::Null));
    let mut lines = Vec::new();
    let statuses: std::collections::BTreeSet<&str> =
        old_resp.keys().chain(new_resp.keys()).copied().collect();
    for status in statuses {
        match (old_resp.get(status), new_resp.get(status)) {
            (Some(o), None) => {
                lines.push(format!("response `{status}` removed: `{}`", describe_response(o)))
            }
            (None, Some(n)) => {
                lines.push(format!("response `{status}` added: `{}`", describe_response(n)))
            }
            (Some(o), Some(n)) if o != n => {
                let (od, nd) = (describe_response(o), describe_response(n));
                if od != nd {
                    lines.push(format!("response `{status}` schema: `{od}` → `{nd}`"));
                } else {
                    lines.push(format!("response `{status}` changed (description or headers)"));
                }
            }
            _ => {}
        }
    }
    lines
}

fn diff_shared_response(old: &Value, new: &Value) -> Vec<String> {
    let (od, nd) = (describe_response(old), describe_response(new));
    if od != nd {
        vec![format!("schema: `{od}` → `{nd}`")]
    } else {
        vec!["changed (description or headers)".to_owned()]
    }
}

fn diff_definition(old: &Value, new: &Value) -> Vec<String> {
    let old_props = entries(old.get("properties").unwrap_or(&Value::Null));
    let new_props = entries(new.get("properties").unwrap_or(&Value::Null));
    let mut lines = Vec::new();
    let names: std::collections::BTreeSet<&str> =
        old_props.keys().chain(new_props.keys()).copied().collect();
    for name in names {
        match (old_props.get(name), new_props.get(name)) {
            (Some(_), None) => lines.push(format!("property `{name}` removed")),
            (None, Some(n)) => {
                lines.push(format!("property `{name}` added: {}", describe_schema(n)))
            }
            (Some(o), Some(n)) if o != n => {
                let (od, nd) = (describe_schema(o), describe_schema(n));
                if od != nd {
                    lines.push(format!("property `{name}` changed: {od} → {nd}"));
                } else {
                    lines.push(format!("property `{name}` changed (description or metadata only)"));
                }
            }
            _ => {}
        }
    }
    let (o_req, n_req) = (required_list(old), required_list(new));
    if o_req != n_req {
        lines.push(format!("required: [{}] → [{}]", o_req.join(", "), n_req.join(", ")));
    }
    if lines.is_empty() {
        let (od, nd) = (describe_schema(old), describe_schema(new));
        if od != nd {
            lines.push(format!("changed: {od} → {nd}"));
        } else {
            lines.push("changed (description or metadata only)".to_owned());
        }
    }
    lines
}

fn required_list(def: &Value) -> Vec<&str> {
    def.get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Would `cargo xtask codegen` be able to consume this spec? A polymorphism key is the
/// documented dealbreaker (see [`crate::swagger::POLYMORPHISM_KEYS`]); a document the typed
/// model cannot deserialize at all is the other.
fn generator_warnings(new: &Value) -> Vec<String> {
    match serde_json::from_value::<crate::swagger::Spec>(new.clone()) {
        Err(e) => vec![format!("the swagger model cannot load this spec: {e}")],
        Ok(spec) => {
            let stats = crate::stats::Stats::compute(&spec);
            if stats.polymorphism == 0 {
                Vec::new()
            } else {
                vec![format!(
                    "{} schema node(s) use {}, which the generator does not model — \
                     codegen will refuse this spec until the lowering handles them",
                    stats.polymorphism,
                    crate::swagger::POLYMORPHISM_KEYS.join("/"),
                )]
            }
        }
    }
}

/// Fetches (or reads) the upstream document, canonicalizes it against the vendored version,
/// diffs, and writes the report to `out` (or stdout). Returns the report so `main` can pick
/// the exit code.
/// Fetches (or reads) one side of the comparison and canonicalizes it.
///
/// `version` is the *vendored* version for both sides, always. [`canonicalize`] writes it into
/// `info.version`, so two upstream refs compared against each other do not report the version
/// string itself as a difference.
///
/// [`canonicalize`]: crate::update_spec::canonicalize
fn fetch_side(source: &Source, role: &str, version: &str) -> Result<(String, Side)> {
    let text = match (source, source.url()) {
        (Source::File(path), _) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        (_, Some(url)) => {
            eprintln!("fetching {url}");
            crate::update_spec::fetch(&url)?
        }
        (_, None) => bail!("{source:?} has neither a URL nor a path"),
    };

    // Either the raw Go template or an already-canonical JSON document is accepted, so a
    // report can be reproduced offline from a saved file. Only the template carries
    // placeholders, and only then does the placeholder guard apply.
    if !crate::update_spec::find_placeholders(&text).is_empty() {
        crate::update_spec::check_placeholders(&text)?;
    }
    let json = crate::update_spec::canonicalize(&text, version)?;
    let sha256 = crate::spec::sha256_hex(json.as_bytes());
    Ok((json, Side { role: role.to_owned(), label: source.describe(), sha256 }))
}

/// Diffs `source` against `baseline`, or against the vendored spec when `baseline` is `None`.
///
/// A baseline is what makes the three-way question answerable: `--baseline-tag <latest>
/// --branch main` reports exactly what exists on the development branch and in no release
/// yet, which is the difference between "wait" and "there is a bump to take".
pub fn run(
    root: &Path,
    baseline: Option<&Source>,
    source: &Source,
    out: Option<&Path>,
) -> Result<Report> {
    let loaded = crate::spec::load(root)?;

    let (old_json, old_side) = match baseline {
        None => (
            loaded.json.clone(),
            Side {
                role: "vendored".to_owned(),
                label: format!("`{}` (`spec/{}`)", loaded.lock.tag, loaded.lock.canonical),
                sha256: loaded.lock.canonical_sha256.clone(),
            },
        ),
        Some(b) => fetch_side(b, "baseline", &loaded.lock.version)?,
    };
    let (new_json, new_side) = fetch_side(source, "upstream", &loaded.lock.version)?;

    let old: Value = serde_json::from_str(&old_json)?;
    let new: Value = serde_json::from_str(&new_json)?;
    let report = diff(&old, &new);
    let md = report.render(&old_side, &new_side);

    match out {
        Some(path) => {
            std::fs::write(path, &md)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        None => print!("{md}"),
    }

    let against = match baseline {
        None => source.describe(),
        Some(b) => format!("{} (baseline {})", source.describe(), b.describe()),
    };
    if report.has_drift() {
        eprintln!("drift: {}", report.summary());
    } else {
        eprintln!("no drift against {against}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn side(label: &str) -> Side {
        Side { role: "vendored".to_owned(), label: label.to_owned(), sha256: "0".repeat(64) }
    }

    /// A minimal canonical spec: one operation with a `200` response pointing at a
    /// definition, one shared response, one definition.
    fn base() -> Value {
        json!({
            "swagger": "2.0",
            "info": {"title": "t", "version": "1.27.2"},
            "basePath": "/api/v1",
            "paths": {
                "/repos/{owner}/{repo}": {
                    "get": {
                        "operationId": "repoGet",
                        "parameters": [
                            {"name": "owner", "in": "path", "required": true, "type": "string"},
                            {"name": "repo", "in": "path", "required": true, "type": "string"}
                        ],
                        "responses": {
                            "200": {"$ref": "#/responses/Repository"},
                            "404": {"$ref": "#/responses/notFound"}
                        }
                    }
                }
            },
            "responses": {
                "Repository": {
                    "description": "Repository",
                    "schema": {"$ref": "#/definitions/Repository"}
                },
                "notFound": {"description": "APINotFound is a not found empty response"}
            },
            "definitions": {
                "Repository": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer", "format": "int64"},
                        "name": {"type": "string"}
                    },
                    "required": ["id"]
                }
            }
        })
    }

    #[test]
    fn identical_specs_have_no_drift_and_say_so() {
        let r = diff(&base(), &base());
        assert!(!r.has_drift());
        assert_eq!(r, Report::default());
        let md = r.render(&side("vendored v1.27.2"), &side("gitea@abc"));
        assert!(md.contains("No differences"), "{md}");
        assert!(md.contains("vendored v1.27.2"), "{md}");
        assert!(md.contains("gitea@abc"), "{md}");
    }

    #[test]
    fn an_added_operation_is_listed_with_method_path_and_id() {
        let mut new = base();
        new["paths"]["/repos/{owner}/{repo}"]["delete"] = json!({"operationId": "repoDelete", "responses": {"204": {"$ref": "#/responses/empty"}}});
        let r = diff(&base(), &new);
        assert!(r.has_drift());
        assert_eq!(r.ops_added, vec!["DELETE /repos/{owner}/{repo} (repoDelete)"]);
        assert!(r.ops_removed.is_empty());
        assert!(r.ops_changed.is_empty());
        let md = r.render(&side("a"), &side("b"));
        assert!(md.contains("### Added"), "{md}");
        assert!(md.contains("`DELETE /repos/{owner}/{repo}` (`repoDelete`)"), "{md}");
    }

    #[test]
    fn a_removed_operation_is_listed() {
        let mut new = base();
        new["paths"] = json!({});
        let r = diff(&base(), &new);
        assert_eq!(r.ops_removed, vec!["GET /repos/{owner}/{repo} (repoGet)"]);
        assert!(r.ops_added.is_empty());
    }

    #[test]
    fn a_response_schema_change_names_the_status_and_both_schemas() {
        let mut new = base();
        new["paths"]["/repos/{owner}/{repo}"]["get"]["responses"]["200"] =
            json!({"description": "ok", "schema": {"$ref": "#/definitions/RepositoryV2"}});
        let r = diff(&base(), &new);
        let lines = &r.ops_changed["GET /repos/{owner}/{repo} (repoGet)"];
        assert_eq!(
            lines,
            &vec![
                "response `200` schema: `#/responses/Repository` → `#/definitions/RepositoryV2`"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn added_and_removed_response_statuses_are_reported() {
        let mut new = base();
        let responses = &mut new["paths"]["/repos/{owner}/{repo}"]["get"]["responses"];
        responses["201"] = json!({"$ref": "#/responses/Repository"});
        responses.as_object_mut().unwrap().remove("404");
        let r = diff(&base(), &new);
        let lines = &r.ops_changed["GET /repos/{owner}/{repo} (repoGet)"];
        assert_eq!(
            lines,
            &vec![
                "response `201` added: `#/responses/Repository`".to_owned(),
                "response `404` removed: `#/responses/notFound`".to_owned(),
            ]
        );
    }

    #[test]
    fn parameter_changes_are_reported_by_name_and_location() {
        let mut new = base();
        let params = &mut new["paths"]["/repos/{owner}/{repo}"]["get"]["parameters"];
        // `repo` becomes optional, `owner` disappears, a query param appears.
        *params = json!([
            {"name": "repo", "in": "path", "required": false, "type": "string"},
            {"name": "verbose", "in": "query", "type": "boolean"}
        ]);
        let r = diff(&base(), &new);
        let lines = &r.ops_changed["GET /repos/{owner}/{repo} (repoGet)"];
        assert_eq!(
            lines,
            &vec![
                "parameter `owner` (path) removed".to_owned(),
                "parameter `repo` (path) changed: required true → false".to_owned(),
                "parameter `verbose` (query) added: boolean".to_owned(),
            ]
        );
    }

    #[test]
    fn a_parameter_type_change_is_reported() {
        let mut new = base();
        new["paths"]["/repos/{owner}/{repo}"]["get"]["parameters"][1]["type"] = json!("integer");
        let r = diff(&base(), &new);
        let lines = &r.ops_changed["GET /repos/{owner}/{repo} (repoGet)"];
        assert_eq!(
            lines,
            &vec!["parameter `repo` (path) changed: type string → integer".to_owned()]
        );
    }

    #[test]
    fn definition_property_changes_are_reported() {
        let mut new = base();
        let def = &mut new["definitions"]["Repository"];
        def["properties"]["name"] = json!({"type": "integer", "format": "int64"});
        def["properties"]["archived"] = json!({"type": "boolean"});
        def["properties"].as_object_mut().unwrap().remove("id");
        def["required"] = json!(["name"]);
        let r = diff(&base(), &new);
        assert!(r.ops_changed.is_empty(), "{:?}", r.ops_changed);
        assert_eq!(
            r.defs_changed["Repository"],
            vec![
                "property `archived` added: boolean".to_owned(),
                "property `id` removed".to_owned(),
                "property `name` changed: string → integer/int64".to_owned(),
                "required: [id] → [name]".to_owned(),
            ]
        );
    }

    #[test]
    fn added_and_removed_definitions_are_listed() {
        let mut new = base();
        new["definitions"]["Team"] = json!({"type": "object"});
        new["definitions"].as_object_mut().unwrap().remove("Repository");
        let r = diff(&base(), &new);
        assert_eq!(r.defs_added, vec!["Team"]);
        assert_eq!(r.defs_removed, vec!["Repository"]);
    }

    #[test]
    fn a_shared_response_change_is_reported_once_with_its_users() {
        let mut new = base();
        new["responses"]["Repository"]["schema"] = json!({"$ref": "#/definitions/Repo"});
        let r = diff(&base(), &new);
        // Not attributed to the operation: the operation's own `$ref` did not move.
        assert!(r.ops_changed.is_empty(), "{:?}", r.ops_changed);
        assert_eq!(
            r.responses_changed["Repository"],
            vec![
                "schema: `#/definitions/Repository` → `#/definitions/Repo`".to_owned(),
                "used by: `GET /repos/{owner}/{repo}` (`repoGet`)".to_owned(),
            ]
        );
    }

    #[test]
    fn schemas_are_described_by_ref_type_or_array_items() {
        assert_eq!(describe_schema(&json!({"$ref": "#/definitions/X"})), "#/definitions/X");
        assert_eq!(describe_schema(&json!({"type": "string"})), "string");
        assert_eq!(
            describe_schema(&json!({"type": "integer", "format": "int64"})),
            "integer/int64"
        );
        assert_eq!(
            describe_schema(&json!({"type": "array", "items": {"$ref": "#/definitions/X"}})),
            "array of #/definitions/X"
        );
        assert_eq!(describe_schema(&json!({})), "(none)");
    }

    #[test]
    fn a_spec_the_generator_cannot_load_is_a_warning_not_a_crash() {
        let mut new = base();
        new["definitions"]["Repository"]["allOf"] = json!([{"$ref": "#/definitions/Base"}]);
        let r = diff(&base(), &new);
        assert!(r.has_drift());
        assert!(
            r.warnings.iter().any(|w| w.contains("allOf")),
            "expected a polymorphism warning, got {:?}",
            r.warnings
        );
    }

    #[test]
    fn render_has_a_summary_line_and_sections_only_for_what_changed() {
        let mut new = base();
        new["definitions"]["Team"] = json!({"type": "object"});
        let r = diff(&base(), &new);
        let md = r.render(&side("old"), &side("new"));
        assert!(md.contains("1 definition added"), "{md}");
        assert!(md.contains("## Definitions"), "{md}");
        assert!(!md.contains("## Operations"), "{md}");
        assert!(!md.contains("No differences"), "{md}");
    }

    #[test]
    fn source_urls_point_at_tag_or_branch() {
        assert_eq!(
            Source::Tag("v1.27.3".into()).url().unwrap(),
            "https://raw.githubusercontent.com/go-gitea/gitea/refs/tags/v1.27.3/templates/swagger/v1_json.tmpl"
        );
        assert_eq!(
            Source::Branch("main".into()).url().unwrap(),
            "https://raw.githubusercontent.com/go-gitea/gitea/refs/heads/main/templates/swagger/v1_json.tmpl"
        );
        assert!(Source::File("x".into()).url().is_none());
    }

    #[test]
    fn the_header_table_labels_each_side_by_its_role() {
        // Without this the release-vs-branch report would announce a release tag as the
        // "vendored" spec, which is the exact confusion the baseline exists to remove.
        let mut old = side("tag `v1.27.3`");
        old.role = "baseline".to_owned();
        let mut new = side("branch `gitea`");
        new.role = "upstream".to_owned();
        let md = Report::default().render(&old, &new);
        assert!(md.contains("| baseline | tag `v1.27.3` |"), "{md}");
        assert!(md.contains("| upstream | branch `gitea` |"), "{md}");
        assert!(!md.contains("| vendored |"), "{md}");
    }

    #[test]
    fn a_baseline_replaces_the_vendored_side() {
        // The vendored template as *both* sides. It has to canonicalize to the committed JSON
        // byte-for-byte, so this is a no-drift run that never touches the network — and it
        // proves the baseline goes through the same canonicalize step the vendored side does,
        // which is what stops `info.version` showing up as a difference.
        let root = crate::workspace_root();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("report.md");
        let tmpl = Source::File(crate::spec::tmpl_path(&root));
        let r = run(&root, Some(&tmpl), &tmpl, Some(&out)).unwrap();
        assert!(!r.has_drift(), "{r:?}");
        let md = std::fs::read_to_string(&out).unwrap();
        assert!(md.contains("| baseline |"), "{md}");
        assert!(!md.contains("| vendored |"), "{md}");
    }

    #[test]
    fn run_against_the_vendored_template_itself_reports_no_drift() {
        // The vendored `spec/v1_json.tmpl` is exactly what `update-spec` fetched, so feeding
        // it back through `--file` must canonicalize to the committed JSON byte-for-byte.
        let root = crate::workspace_root();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("report.md");
        let src = Source::File(crate::spec::tmpl_path(&root));
        let r = run(&root, None, &src, Some(&out)).unwrap();
        assert!(!r.has_drift(), "{r:?}");
        let md = std::fs::read_to_string(&out).unwrap();
        assert!(md.contains("No differences"), "{md}");
    }
}

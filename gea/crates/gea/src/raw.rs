//! Layer 2's adapter: a [`PlannedRequest`] onto a `Client`, and its response onto
//! [`crate::output`].
//!
//! `gea-raw` deliberately does no I/O — it turns [`clap::ArgMatches`] into a described request
//! and stops. Everything that touches the network, the filesystem, or stdout is here, which is
//! what makes 506 operations testable without a transport and `--dry-run` a `return` rather than
//! a flag threaded through the client.
//!
//! The interesting decisions:
//!
//! * **`--json` field discovery short-circuits before anything else**, including before the
//!   runtime is built. Asking "what can I select?" must work with no token, no network, and no
//!   configured host — see `docs/output.md`, divergence 2.
//! * **`OpMeta::produces` picks the exit**, not the response. A `Produces::Bytes` operation is
//!   streamed to `--output` and refused to a terminal; a `Produces::Empty` one is drained.
//!   Guessing from `Content-Type` instead would put a zip file on someone's terminal the first
//!   time an instance mislabelled one.

use std::io::Write;
use std::path::Path;

use clap::ArgMatches;
use futures::StreamExt;
use gea_raw::PlannedRequest;
use gitea_client::meta_types::{self, OpMeta, Produces};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::{Accept, Body, Part, Request};
use gitea_core::types::RepoSlug;
use serde_json::Value;

use crate::api::paginate;
use crate::global::GlobalOpts;
use crate::output::{self, Filter, Pipeline, Selection, Template, project};
use crate::runtime::Runtime;

/// The layer-2 root: `gea` with the global flags and **only** the named subtree.
///
/// Lives here rather than in `main` so that the 506-operation smoke test can build the exact
/// command tree the binary builds. That test is not decoration: clap answers a duplicate long
/// name with a *panic*, so a generated flag colliding with a global would be a crash reachable
/// from an ordinary command line, and the collision would arrive with a spec bump rather than
/// with a code change.
pub fn root(group: Option<&str>, leaf: Option<&str>) -> clap::Command {
    let raw =
        gea_raw::raw_command(gitea_client::meta::OPS, gitea_client::meta::GROUPS, group, leaf);
    let taken = crate::global::taken(&raw);
    crate::global::augment(
        clap::Command::new("gea")
            .about(crate::ABOUT)
            .long_about(crate::LONG_ABOUT)
            .version(crate::version_string())
            .subcommand_required(true)
            .arg_required_else_help(true),
        &taken,
    )
    .subcommand(raw)
}

/// Run one layer-2 operation.
pub fn run(globals: &GlobalOpts, op: &'static OpMeta, m: &ArgMatches) -> Result<()> {
    // Field discovery first: no runtime, no credential, no network. A fresh install with no
    // configured host must still be able to answer this.
    let selected = match globals.json.as_deref() {
        None => None,
        Some(raw) => match resolve_fields(op, raw)? {
            Selection::Discover => {
                let fields = fields_for(op);
                let term = output::Term::detect();
                let mut out = std::io::stdout().lock();
                project::write_field_list(&fields, &term, &mut out)?;
                out.flush()?;
                return Ok(());
            }
            Selection::Fields(f) => Some(f),
        },
    };

    // `--dry-run` sends nothing, so it must not demand a configured host either: "show me what
    // this would send" is exactly the question someone asks *before* running `auth login`, and
    // answering it with `no Gitea host is set up yet` would be perverse.
    if m.try_get_one::<bool>(gea_raw::build::ID_DRY_RUN).ok().flatten().copied().unwrap_or(false) {
        let plan =
            gea_raw::bind(op, m, best_effort_slug(globals, op).as_ref(), &mut std::io::stdin())?;
        let mut out = std::io::stdout().lock();
        out.write_all(plan.describe().as_bytes())?;
        out.flush()?;
        return Ok(());
    }

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;

        // `bind` wants the repository up front, and only operations with a `ctx_fill` parameter
        // can use one. A resolution failure is swallowed on purpose: `bind`'s own message names
        // every way to supply the value ("pass it as <OWNER> or --owner, or -R owner/repo, or
        // run inside a clone"), which is more useful here than the resolver's list of attempts.
        let slug: Option<RepoSlug> =
            if needs_context(op) { rt.repo(globals).ok().map(|c| c.slug.clone()) } else { None };

        let plan = gea_raw::bind(op, m, slug.as_ref(), &mut std::io::stdin())?;
        execute(&rt, globals, &plan, selected.as_deref()).await
    })
}

fn needs_context(op: &'static OpMeta) -> bool {
    op.params.iter().any(|p| p.ctx_fill.is_some())
}

/// The repository for a `--dry-run`, without insisting on a working configuration.
///
/// `-R owner/name` needs nothing at all. Falling back to full resolution needs config and `git`,
/// so it is attempted and discarded on failure — `bind` then reports the missing parameter, and
/// its message already names every way to supply one.
fn best_effort_slug(globals: &GlobalOpts, op: &'static OpMeta) -> Option<RepoSlug> {
    if let Some(r) = &globals.repo {
        return Some(r.slug.clone());
    }
    if !needs_context(op) {
        return None;
    }
    let rt = Runtime::new(globals).ok()?;
    rt.repo(globals).ok().map(|c| c.slug.clone())
}

async fn execute(
    rt: &Runtime,
    globals: &GlobalOpts,
    plan: &PlannedRequest,
    fields: Option<&[String]>,
) -> Result<()> {
    let req = build_request(plan)?;
    rt.trace(&format!("{} {}", plan.method(), plan.path_and_query()));

    let filter = globals.jq.as_deref().map(Filter::compile).transpose()?;
    let template = globals.template.as_deref().map(Template::parse).transpose()?;
    let pipeline = Pipeline::new().fields(fields).jq(filter.as_ref()).template(template.as_ref());

    match plan.op.produces {
        Produces::Json | Produces::LdJson => {
            // `--paginate` on the leaf (paginated operations only) or the global one, so a
            // non-paginated operation still refuses politely rather than silently ignoring it.
            let paginate_wanted = plan.paginate || globals.paginate;
            if paginate_wanted && plan.op.pagination == meta_types::Pagination::Paged {
                let limit = plan.limit.map(|n| n as usize).or(globals.limit);
                let walked = paginate::walk(rt.client(), &req, limit).await?;
                rt.trace(&format!(
                    "paginate: {} item(s) over {} page(s); stopped because {}",
                    walked.items,
                    walked.pages.len(),
                    walked.stopped
                ));
                // One merged array rather than a document per page: a layer-2 command has a
                // known response shape, so the caller asked for "the collection", and
                // `--json`/`--jq` should see it as one list the way `gea pr list` will.
                let merged = paginate::flatten(walked.pages);
                return render(rt, globals, &pipeline, merged);
            }
            if paginate_wanted {
                return Err(usage(format!(
                    "gea raw {} {} does not support pagination; remove --paginate",
                    plan.op.group, plan.op.command
                )));
            }
            let value = rt.client().value(req).await?;
            render(rt, globals, &pipeline, value)
        }

        Produces::Text | Produces::Html => {
            if pipeline.is_explicit() {
                return Err(usage(format!(
                    "gea raw {} {} answers with {}, not JSON, so --json/--jq/--template have \
                     nothing to work on",
                    plan.op.group,
                    plan.op.command,
                    if plan.op.produces == Produces::Html { "HTML" } else { "plain text" }
                )));
            }
            let (_, mut body) = rt.client().bytes(req).await?;
            let dest = output::dest_for(globals.output.as_deref());
            let mut out = output::open_dest(&dest)?;
            while let Some(chunk) = body.next().await {
                out.write_all(&chunk?)?;
            }
            out.flush()?;
            Ok(())
        }

        Produces::Bytes => {
            let (mime, mut body) = rt.client().bytes(req).await?;
            let dest = output::dest_for(globals.output.as_deref());
            // Before a single byte is written: a few kilobytes of a zip file interpreted as
            // terminal input can leave the session needing `reset`.
            output::guard_binary(mime.as_str(), rt.term(), &dest, globals.force)?;
            let mut out = output::open_dest(&dest)?;
            let mut total = 0u64;
            // Streamed, not buffered: this is the path a multi-gigabyte release asset takes.
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                total += chunk.len() as u64;
                out.write_all(&chunk)?;
            }
            out.flush()?;
            rt.trace(&format!("wrote {total} byte(s) of {mime}"));
            Ok(())
        }

        Produces::Empty => {
            rt.client().empty(req).await?;
            Ok(())
        }
    }
}

fn render(rt: &Runtime, globals: &GlobalOpts, pipeline: &Pipeline<'_>, value: Value) -> Result<()> {
    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;
    pipeline.render(value, rt.term(), &mut out)?;
    out.flush()?;
    Ok(())
}

/// A [`PlannedRequest`] as a wire [`Request`].
pub fn build_request(plan: &PlannedRequest) -> Result<Request> {
    let mut req = Request::get(plan.path.clone()).accept(accept_for(plan.op.produces));
    crate::api::set_method(&mut req, plan.method())?;
    // Layer 2 knows exactly which operation this is, so a 403 here names the scope codegen
    // recorded rather than the one `infer_scope` guesses from the path — the two disagree on
    // every pull-request route. Assigned rather than built with `Request::scope` because
    // `OpMeta::scope` is already an `Option`.
    req.scope = plan.op.scope;
    for (key, value) in &plan.query {
        req = req.query(key.clone(), value.clone());
    }

    if !plan.uploads.is_empty() || !plan.form.is_empty() {
        let mut parts = Vec::with_capacity(plan.uploads.len() + plan.form.len());
        for (name, value) in &plan.form {
            parts.push(Part::bytes(*name, value.as_bytes().to_vec()));
        }
        for (name, path) in &plan.uploads {
            parts.push(if path == Path::new("-") {
                Part::stdin(*name, "stdin")
            } else {
                Part::file(*name, path.clone())
            });
        }
        req.body = Body::Multipart(parts);
        return Ok(req);
    }

    if let Some(body) = &plan.body {
        req = req.json_body(body)?;
    }
    Ok(req)
}

/// `produces` decides what we are willing to accept back.
fn accept_for(produces: Produces) -> Accept {
    match produces {
        Produces::Json => Accept::Json,
        Produces::LdJson => Accept::Other(std::borrow::Cow::Borrowed("application/ld+json")),
        Produces::Text => Accept::Text,
        Produces::Html => Accept::Html,
        Produces::Bytes => Accept::Octets,
        // A 204 endpoint declares no media type at all, and demanding JSON from one is how you
        // get a 406 from a strict proxy.
        Produces::Empty => Accept::Any,
    }
}

// ------------------------------------------------------------------- `--json` field tables

/// Validate `--json` against the operation's generated field table.
fn resolve_fields(op: &'static OpMeta, raw: &str) -> Result<Selection> {
    let fields = fields_for(op);
    if fields.is_empty() {
        return Err(usage(format!(
            "gea raw {} {} has no selectable fields: it answers with {}, not a typed object.\n\
             use --jq to reach into the response instead",
            op.group,
            op.command,
            match op.produces {
                Produces::Empty => "nothing",
                Produces::Text => "plain text",
                Produces::Html => "HTML",
                Produces::Bytes => "a byte stream",
                _ => "an untyped value",
            }
        )));
    }
    project::resolve(raw, &fields)
}

/// The generated field table for an operation, in the output module's vocabulary.
///
/// `gitea_client::meta_types::FieldSpec` and [`crate::output::project::FieldSpec`] are
/// structurally identical and deliberately distinct types: `gea::output` is written against no
/// generated code at all, so that it can be developed and tested before the emitters exist.
/// The price is this adapter, which is the whole cost of that independence.
fn fields_for(op: &'static OpMeta) -> Vec<project::FieldSpec> {
    gitea_client::fields::OP_FIELDS
        .binary_search_by(|(id, _)| (*id).cmp(op.op_id))
        .map(|i| gitea_client::fields::OP_FIELDS[i].1)
        .unwrap_or(&[])
        .iter()
        .map(|f| project::FieldSpec { name: f.name, kind: kind_of(&f.kind), doc: f.doc })
        .collect()
}

fn kind_of(kind: &'static meta_types::FieldKind) -> project::FieldKind {
    use meta_types::FieldKind as G;
    use project::FieldKind as O;
    match kind {
        G::Bool => O::Bool,
        G::Int => O::Int,
        G::Float => O::Float,
        G::Str => O::Str,
        G::DateTime => O::DateTime,
        G::Enum(values) => O::Enum(values),
        // Only top-level fields are selectable, so a nested table would never be read; the
        // listing needs the *label* ("object"), which does not depend on it.
        G::Object(_) => O::Object(&[]),
        G::Array(inner) => O::Array(inner_of(inner)),
        G::Map(inner) => O::Map(inner_of(inner)),
        G::Json => O::Json,
    }
}

/// The element kind of an array or map, for a label like `[string]`.
///
/// A composite element collapses to `json`: `[object]` would suggest the elements can be
/// selected, and they cannot — that is `--jq`'s job.
fn inner_of(kind: &'static meta_types::FieldKind) -> &'static project::FieldKind {
    use meta_types::FieldKind as G;
    use project::FieldKind as O;
    match kind {
        G::Bool => &O::Bool,
        G::Int => &O::Int,
        G::Float => &O::Float,
        G::Str => &O::Str,
        G::DateTime => &O::DateTime,
        _ => &O::Json,
    }
}

fn usage(msg: String) -> Error {
    Error::new(ErrorKind::Usage(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_client::meta::{GROUPS, OPS};
    use gitea_client::meta_types::lookup;

    fn plan_of(group: &str, command: &str, words: &[&str]) -> PlannedRequest {
        let op =
            lookup::op(OPS, group, command).unwrap_or_else(|| panic!("no op {group} {command}"));
        let cmd = gea_raw::build(OPS, GROUPS, Some(group), Some(command));
        let mut argv: Vec<String> = vec!["gea".into(), "raw".into(), group.into(), command.into()];
        argv.extend(words.iter().map(|w| (*w).to_owned()));
        let m = cmd.try_get_matches_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let leaf = m
            .subcommand_matches("raw")
            .and_then(|m| m.subcommand_matches(group))
            .and_then(|m| m.subcommand_matches(command))
            .expect("leaf matches");
        gea_raw::bind(op, leaf, None, &mut std::io::empty()).unwrap()
    }

    #[test]
    fn a_json_body_operation_becomes_a_json_request() {
        let plan = plan_of("repo", "create-pull-request", &["o", "r", "--title", "hi"]);
        let req = build_request(&plan).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/repos/o/r/pulls");
        assert!(matches!(req.body, Body::Json(_)));
        assert_eq!(req.accept, Accept::Json);
    }

    /// Bug this prevents: a multipart operation's file being serialised into the JSON body,
    /// which uploads the *path* rather than the file.
    #[test]
    fn an_upload_operation_becomes_multipart_and_not_a_json_body() {
        let plan = plan_of(
            "repo",
            "create-release-attachment",
            &["o", "r", "7", "--attachment", "/tmp/x.zip"],
        );
        let req = build_request(&plan).unwrap();
        match &req.body {
            Body::Multipart(parts) => assert_eq!(parts.len(), 1),
            other => panic!("expected multipart, got {other:?}"),
        }
    }

    /// The whole set of `produces` values must map to an `Accept`, and a byte endpoint must not
    /// ask for JSON — a strict proxy answers that with a 406.
    #[test]
    fn every_produces_maps_to_an_accept() {
        assert_eq!(accept_for(Produces::Json), Accept::Json);
        assert_eq!(accept_for(Produces::Text), Accept::Text);
        assert_eq!(accept_for(Produces::Bytes), Accept::Octets);
        assert_eq!(accept_for(Produces::Empty), Accept::Any);
        assert_eq!(accept_for(Produces::LdJson).header(), "application/ld+json");
    }

    /// The `--json` short-circuit has to find a real table for a real operation, or discovery
    /// silently claims every operation is untyped.
    #[test]
    fn a_real_operation_has_a_real_field_table() {
        let op = lookup::op(OPS, "repo", "get").unwrap();
        let fields = fields_for(op);
        assert!(fields.iter().any(|f| f.name == "full_name"), "{fields:?}");
        assert!(matches!(resolve_fields(op, ""), Ok(Selection::Discover)));
        assert!(matches!(resolve_fields(op, "full_name"), Ok(Selection::Fields(_))));
        // An unknown field is a usage error carrying a suggestion, not a silent empty column.
        let e = resolve_fields(op, "fullname").unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }

    /// An operation with nothing to select must say so rather than offering an empty list.
    #[test]
    fn an_untyped_operation_refuses_json_with_an_alternative() {
        let op = OPS
            .iter()
            .find(|o| {
                gitea_client::fields::OP_FIELDS
                    .binary_search_by(|(id, _)| (*id).cmp(o.op_id))
                    .is_err()
            })
            .expect("some operation returns no typed object");
        let e = resolve_fields(op, "").unwrap_err();
        assert!(e.to_string().contains("--jq"), "{e}");
    }

    /// Every field kind must produce a label; a panic here would be a crash driven by table
    /// contents on a `--json` that only asked what exists.
    #[test]
    fn every_generated_field_kind_converts() {
        for (_, table) in gitea_client::fields::OP_FIELDS {
            for f in *table {
                assert!(!kind_of(&f.kind).label().is_empty(), "{}", f.name);
            }
        }
    }
}

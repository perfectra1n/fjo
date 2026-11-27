//! `gea workflow` — Actions workflow files: list, inspect, dispatch, enable, disable.
//!
//! # What the API does and does not give us
//!
//! Gitea 1.27 has workflow routes — list, get, enable, disable, dispatch — but the objects they
//! return know only a workflow's file name, display name and `state`. They say nothing about what
//! the file *does*, and what it does is what this group is for. So:
//!
//! * **`list` and `view` are built from the contents API.** The workflow directories
//!   (`.gitea/workflows` and `.github/workflows` — Gitea reads both) are listed, and each file is
//!   read and scanned by [`yaml`]. That is what makes the `DISPATCH` and `RUNS-ON` columns
//!   possible, and `RUNS-ON` is the column that matters on Gitea, because a label no runner
//!   carries means the run waits forever (see [`crate::cmd::run`]). The Actions listing is read
//!   alongside, for the one thing only it knows: whether the workflow is enabled.
//! * **`enable`, `disable` and `run` use the Actions routes directly.**
//!
//! # Dispatch takes a *filename*, not an id
//!
//! `{workflow_id}` is `ci.yml`, not a number — unlike GitHub, where the same command takes
//! either. `gea workflow run ci.yml` and `gea workflow run .gitea/workflows/ci.yml` both
//! work: the path form is resolved against the discovered files and only the base name is sent.
//!
//! # `-f`, `-F`, and where `gh`'s `--json` went
//!
//! Inputs use `crate::api::fields`, the *same* parser as `gea api`, so `-f` (always a string)
//! and `-F` (typed, `@file`, `@-` for stdin) cannot mean two different things at two layers.
//! `gh workflow run --json` reads an input object from stdin; here that is **`--input -`**,
//! spelled like `gea api --input`, because `--json` is a global output selector in `gea` and
//! one flag cannot be both. Note that Actions inputs are strings on the wire — `-F count=3`
//! sends `"3"` — which is Gitea's schema, not a conversion of ours.

pub(crate) mod yaml;

use std::collections::BTreeMap;

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::ContentsResponse;
use serde_json::Value;

use crate::api::fields::{self, Typing};
use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::project::{FieldKind, FieldSpec};
use crate::runtime::Runtime;

/// Where Gitea looks for workflow files, in the order it prefers them.
///
/// Both are read: `.gitea/workflows` is Gitea's own, and `.github/workflows` is what a
/// repository mirrored from GitHub has. A tool that only looked in one of them would report "no
/// workflows" for a repository whose Actions work. (`.forgejo/workflows` is Forgejo's, and Gitea
/// does not read it.)
///
/// The usual repository has **one** of the two, so the other read exists only to be discarded as
/// a 404 — see [`discover`], which therefore sends both at once rather than paying a serial round
/// trip for an answer it is going to throw away.
const WORKFLOW_DIRS: [&str; 2] = [".gitea/workflows", ".github/workflows"];

/// How many workflow files [`list`] reads at once.
///
/// Bounded rather than "all of them at once": an unbounded burst against a self-hosted instance
/// behind a reverse proxy trips its rate limit, and `gitea_core::http`'s retry layer then spends
/// back in backoff everything the concurrency bought. Six is enough to hide the round trip of the
/// handful of workflows a repository has.
const READ_CONCURRENCY: usize = 6;

/// The shape `list` and `view` emit.
///
/// Ten keys, and none of them invented: `path`, `name`, `sha` and `html_url` are
/// `ContentsResponse`'s own; `state` is `ActionWorkflow`'s; `workflow_name`, `dispatch`,
/// `events`, `runs_on` and `jobs` are named after the keys in the workflow file itself (`name`,
/// `on.workflow_dispatch`, `on`, `runs-on`, `jobs`).
const WORKFLOW_FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "path", kind: FieldKind::Str, doc: "path of the file in the repository" },
    FieldSpec { name: "name", kind: FieldKind::Str, doc: "the file's name" },
    FieldSpec { name: "workflow_name", kind: FieldKind::Str, doc: "the workflow's `name:`" },
    FieldSpec { name: "dispatch", kind: FieldKind::Bool, doc: "declares workflow_dispatch" },
    FieldSpec { name: "events", kind: FieldKind::Json, doc: "the events under `on:`" },
    FieldSpec { name: "runs_on", kind: FieldKind::Json, doc: "runner labels the jobs ask for" },
    FieldSpec { name: "jobs", kind: FieldKind::Json, doc: "job ids in the file" },
    FieldSpec { name: "sha", kind: FieldKind::Str, doc: "blob sha of the file" },
    FieldSpec { name: "html_url", kind: FieldKind::Str, doc: "web URL of the file" },
    FieldSpec { name: "state", kind: FieldKind::Str, doc: "active, or why it is not" },
];

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the repository's workflow files, with their triggers and runner labels
    List(ListArgs),
    /// Show one workflow: its triggers, dispatch inputs, jobs and labels
    View(ViewArgs),
    /// Start a workflow that declares `workflow_dispatch`
    Run(RunArgs),
    /// Enable a workflow, so its triggers start runs again
    Enable(ToggleArgs),
    /// Disable a workflow: its triggers stop starting runs until it is enabled
    Disable(ToggleArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Read the workflows at this ref instead of the default branch
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// Only workflows that can be started with `gea workflow run`
    #[arg(long)]
    pub dispatchable: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    /// The workflow file: `ci.yml`, or its full path
    #[arg(value_name = "FILE")]
    pub file: String,
    /// Print the file itself instead of a summary
    #[arg(long)]
    pub yaml: bool,
    /// Read the workflow at this ref instead of the default branch
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct RunArgs {
    /// The workflow file: `ci.yml`, or its full path
    #[arg(value_name = "FILE")]
    pub file: String,
    /// The ref to run on; defaults to the checked-out branch, else the default branch
    #[arg(short = 'r', long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// An input as a string: `-f environment=staging`
    #[arg(short = 'f', long = "raw-field", value_name = "KEY=VALUE")]
    pub raw_field: Vec<String>,
    /// A typed input: `-F verbose=true`, `-F notes=@file`, `-F notes=@-`
    #[arg(short = 'F', long = "field", value_name = "KEY=VALUE")]
    pub field: Vec<String>,
    /// Read all inputs as one JSON object from a file; `-` means stdin
    #[arg(long, value_name = "FILE")]
    pub input: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ToggleArgs {
    /// The workflow file: `ci.yml`, or its full path
    #[arg(value_name = "FILE")]
    pub file: String,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, globals, &api, a).await,
            Cmd::View(a) => view(&rt, globals, &api, a).await,
            Cmd::Run(a) => dispatch(&rt, globals, &api, a).await,
            Cmd::Enable(a) => toggle(&rt, globals, &api, a, true).await,
            Cmd::Disable(a) => toggle(&rt, globals, &api, a, false).await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Custom(WORKFLOW_FIELDS),
        Cmd::View(a) if a.yaml => Fields::None,
        Cmd::View(_) => Fields::Custom(WORKFLOW_FIELDS),
        Cmd::Run(_) => Fields::Op("ActionsDispatchWorkflow"),
        Cmd::Enable(_) | Cmd::Disable(_) => Fields::None,
    }
}

// ------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    let files = discover(api, &slug, args.git_ref.as_deref()).await?;
    let limit = support::limit(None, globals);

    // `take(limit)` before any fetching, so `--limit` still caps the number of requests rather
    // than only the number of rows printed.
    let entries: Vec<ContentsResponse> = files.into_iter().take(limit).collect();
    // One request per file. That is the price of the columns that matter: without reading the
    // file there is no way to know whether it can be dispatched or which labels it needs, and
    // a repository has a handful of workflows, not hundreds. They are read [`READ_CONCURRENCY`]
    // at a time rather than one after another: nothing is printed until `emit::list` below, so
    // the serial version was N round trips for a table that could not appear until the last one
    // came back anyway.
    let (parsed, states) =
        futures::join!(read_all(api, &slug, &entries, args.git_ref.as_deref()), states(api, &slug));
    let parsed = parsed?;

    let rows: Vec<(ContentsResponse, yaml::Parsed)> = entries
        .into_iter()
        .zip(parsed)
        .filter(|(_, p)| !args.dispatchable || p.dispatchable())
        .collect();

    let state_of = |e: &ContentsResponse| states.get(&e.name).cloned().unwrap_or_default();
    let value = Value::Array(rows.iter().map(|(e, p)| as_json(e, p, &state_of(e))).collect());
    let listing = Listing {
        fields: Fields::Custom(WORKFLOW_FIELDS),
        value,
        count: rows.len(),
        total: None,
        noun: "workflows",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["NAME", "FILE", "STATE", "DISPATCH", "RUNS-ON", "EVENTS", "JOBS"]);
        for (entry, parsed) in &rows {
            t.row([
                parsed.display_name(&entry.path),
                entry.name.clone(),
                state_of(entry),
                if parsed.dispatchable() { "yes".to_owned() } else { String::new() },
                join(&parsed.runs_on),
                join(&parsed.events),
                join(&parsed.jobs),
            ]);
        }
    })?;
    if rows.is_empty() {
        support::note(
            rt.term(),
            &format!("no workflow files in {}; Gitea reads {}", slug, WORKFLOW_DIRS.join(", ")),
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------- view

async fn view(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ViewArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    let entry = resolve_file(api, &slug, &args.file, args.git_ref.as_deref()).await?;
    let source = read_source(api, &slug, &entry.path, args.git_ref.as_deref()).await?;

    if args.yaml {
        return emit::text(rt, globals, &source);
    }

    let parsed = yaml::parse(&source);
    let state = states(api, &slug).await.remove(&entry.name).unwrap_or_default();
    let mut rows = vec![
        ("name".to_owned(), parsed.display_name(&entry.path)),
        ("path".to_owned(), entry.path.clone()),
        ("state".to_owned(), state.clone()),
        ("events".to_owned(), join(&parsed.events)),
        ("runs-on".to_owned(), join(&parsed.runs_on)),
        ("jobs".to_owned(), join(&parsed.jobs)),
        (
            "dispatch".to_owned(),
            if parsed.dispatchable() {
                format!("yes: `gea workflow run {}`", entry.name)
            } else {
                "no (the file declares no workflow_dispatch trigger)".to_owned()
            },
        ),
    ];
    // The inputs are the reason to run `view` before `run`: they are the only place the legal
    // values of a `choice` input are written down.
    for input in parsed.dispatch.iter().flatten() {
        let mut described = input.kind_or_default().to_owned();
        if !input.options.is_empty() {
            described.push_str(&format!(" ({})", input.options.join("|")));
        }
        if input.required {
            described.push_str(" [required]");
        }
        if !input.default.is_empty() {
            described.push_str(&format!(" default {}", input.default));
        }
        rows.push((format!("input {}", input.name), described));
    }
    rows.push(("url".to_owned(), entry.html_url.clone()));

    emit::detail(
        rt,
        globals,
        Fields::Custom(WORKFLOW_FIELDS),
        as_json(&entry, &parsed, &state),
        rows,
    )
}

// --------------------------------------------------------------------------------- dispatch

async fn dispatch(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &RunArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    let inputs = collect_inputs(args)?;

    // The file name, not a path and not an id: that is what the endpoint takes.
    let entry = resolve_file(api, &slug, &args.file, None).await.ok();
    let filename = match &entry {
        Some(e) => e.name.clone(),
        None => file_name(&args.file).to_owned(),
    };

    // Validate against the file when we can read it. Doing this before the request turns
    // "the run started and did nothing" into a message naming the input.
    if let Some(e) = &entry
        && let Ok(source) = read_source(api, &slug, &e.path, None).await
    {
        let parsed = yaml::parse(&source);
        check_inputs(rt.term(), &parsed, &inputs, &filename)?;
    }

    let git_ref = match &args.git_ref {
        Some(r) => r.clone(),
        None => default_ref(rt, api, &slug).await?,
    };

    let body = gitea_model::CreateActionWorkflowDispatch {
        inputs: Some(inputs.clone()),
        r#ref: git_ref.clone(),
    };
    let started = send_dispatch(api, &slug, &filename, &body).await?;

    emit::detail(
        rt,
        globals,
        Fields::Op("ActionsDispatchWorkflow"),
        serde_json::to_value(&started).map_err(encode_failed)?,
        vec![
            ("workflow".to_owned(), filename.clone()),
            ("ref".to_owned(), git_ref),
            ("run".to_owned(), started.workflow_run_id.to_string()),
            ("url".to_owned(), started.html_url.clone()),
            ("next".to_owned(), format!("gea run watch {}", started.workflow_run_id)),
        ],
    )
}

/// The dispatch request itself, split out so a `FakeTransport` test can read what was sent.
///
/// `return_run_details` is always asked for, for two reasons: the caller wants the run id it just
/// created, and the generated method decodes a `RunDetails` — without it the server answers 204
/// with an empty body and the decode fails on a request that in fact succeeded.
async fn send_dispatch(
    api: &Api,
    slug: &RepoSlug,
    filename: &str,
    body: &gitea_model::CreateActionWorkflowDispatch,
) -> Result<gitea_model::RunDetails> {
    let q = query::ActionsDispatchWorkflowQuery {
        return_run_details: Some(true),
        ..Default::default()
    };
    api.workflow().dispatch(&slug.owner, &slug.name, filename, body, &q).await
}

/// `-f`, `-F` and `--input` into the one `inputs` map the endpoint accepts.
///
/// Everything becomes a string, because that is what `CreateActionWorkflowDispatch.inputs` is: a
/// `map[string]string`. `-F verbose=true` therefore sends `"true"`, not `true` — the same value
/// the web UI's checkbox sends. A nested object or array is refused rather than serialised into
/// a string that no workflow could read.
fn collect_inputs(args: &RunArgs) -> Result<BTreeMap<String, String>> {
    let mut specs: Vec<(Typing, String)> = Vec::new();
    specs.extend(args.raw_field.iter().map(|v| (Typing::Raw, v.clone())));
    specs.extend(args.field.iter().map(|v| (Typing::Typed, v.clone())));

    let mut out = BTreeMap::new();
    if let Some(source) = &args.input {
        let bytes = read_input(source)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            support::usage(format!("--input {source}: that is not valid JSON ({e})"))
        })?;
        let Value::Object(map) = value else {
            return Err(support::usage(format!(
                "--input {source}: workflow inputs are a JSON object of key/value pairs, \
                 e.g. {{\"environment\":\"staging\"}}"
            )));
        };
        for (k, v) in map {
            out.insert(k.clone(), scalar(&k, &v)?);
        }
    }

    let parsed = fields::parse(&specs, &mut std::io::stdin())?;
    for field in parsed {
        if field.key.ends_with("[]") || field.key.contains('.') {
            return Err(support::usage(format!(
                "{:?}: a workflow input is a flat name; arrays and nested keys have no meaning \
                 here because Gitea's inputs are a map of strings",
                field.key
            )));
        }
        let value = scalar(&field.key, &field.value)?;
        // Later wins, so `--input` can be overridden one field at a time.
        out.insert(field.key, value);
    }
    Ok(out)
}

/// A JSON scalar as the string the wire wants.
fn scalar(key: &str, value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        // `null` would arrive at the workflow as the string "null"; refusing is the honest
        // reading of "this input has no value".
        Value::Null => Err(support::usage(format!(
            "input {key:?} has no value; workflow inputs are strings, so pass one (or leave the \
             input out to use the workflow's default)"
        ))),
        Value::Array(_) | Value::Object(_) => Err(support::usage(format!(
            "input {key:?} is a {}; Gitea's workflow inputs are strings, so pass a single value",
            if value.is_array() { "list" } else { "object" }
        ))),
    }
}

/// Compare the given inputs with the ones the file declares.
///
/// A missing **required** input is an error before the request: Gitea would either refuse it
/// with a less specific message or start a run that immediately does the wrong thing. An
/// *unknown* input is only a warning, because [`yaml`] is a shallow scanner and being wrong here
/// must not block a dispatch that would have worked.
fn check_inputs(
    term: &Term,
    parsed: &yaml::Parsed,
    given: &BTreeMap<String, String>,
    filename: &str,
) -> Result<()> {
    let Some(declared) = &parsed.dispatch else {
        return Err(support::usage(format!(
            "{filename} declares no workflow_dispatch trigger, so it cannot be started from the \
             API; add `on: workflow_dispatch:` to the file, or run `gea workflow list \
             --dispatchable` to see which workflows can"
        )));
    };
    if declared.is_empty() {
        return Ok(());
    }

    let names: Vec<&str> = declared.iter().map(|i| i.name.as_str()).collect();

    // **Every** missing required input, not just the first. This was a `for` loop whose body
    // returned unconditionally, so it reported one input and stopped: a workflow with three
    // required inputs and none given took three dispatches to find out what it wanted, each one
    // a round trip that ended in the same refusal. Reporting before the request is the whole
    // point of this function, and it is only worth anything if it reports all of it at once.
    let missing: Vec<&yaml::Input> =
        declared.iter().filter(|i| i.required && !given.contains_key(&i.name)).collect();
    match missing.as_slice() {
        [] => {}
        [only] => {
            return Err(support::usage(format!(
                "{filename} requires the input {:?}; pass it with {}",
                only.name,
                flag_for(only)
            )));
        }
        many => {
            return Err(support::usage(format!(
                "{filename} requires {} inputs that were not given; pass them with:\n{}",
                many.len(),
                many.iter().map(|i| format!("  {}", flag_for(i))).collect::<Vec<_>>().join("\n")
            )));
        }
    }
    for key in given.keys() {
        match declared.iter().find(|i| &i.name == key) {
            None => support::note(
                term,
                &format!(
                    "warning: {filename} declares no input {key:?} (it declares {}); sending it \
                     anyway",
                    names.join(", ")
                ),
            ),
            Some(input) if !input.options.is_empty() => {
                let value = &given[key];
                if !input.options.contains(value) {
                    support::note(
                        term,
                        &format!(
                            "warning: {key} is a choice input and {value:?} is not one of {}",
                            input.options.join(", ")
                        ),
                    );
                }
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// `-f name=<value>`, with the choices spelled out when the input declares any.
fn flag_for(input: &yaml::Input) -> String {
    format!(
        "-f {}=<value>{}",
        input.name,
        if input.options.is_empty() {
            String::new()
        } else {
            format!(" (one of {})", input.options.join(", "))
        }
    )
}

// -------------------------------------------------------------------------- enable / disable

/// `PUT …/actions/workflows/{file}/{enable,disable}`.
async fn toggle(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &ToggleArgs,
    enable: bool,
) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    let entry = resolve_file(api, &slug, &args.file, None).await.ok();
    let filename = entry.map(|e| e.name).unwrap_or_else(|| file_name(&args.file).to_owned());
    let workflows = api.workflow();
    if enable {
        workflows.enable(&slug.owner, &slug.name, &filename).await?;
    } else {
        workflows.disable(&slug.owner, &slug.name, &filename).await?;
    }
    let verb = if enable { "enabled" } else { "disabled" };
    support::note(rt.term(), &format!("{verb} {filename}"));
    Ok(())
}

// ---------------------------------------------------------------------------------- plumbing

/// Every workflow file in the repository, across both directories Gitea reads.
///
/// # Both at once, and `join!` is not `try_join!`
///
/// A repository normally has exactly one of the two directories, so one of these reads is a 404
/// that the arm below deliberately discards — and read in series that is a round trip spent on an
/// answer nobody wants, charged to every `gea workflow` subcommand because [`resolve_file`] comes
/// through here too. [`futures::join!`] drives both to completion; [`futures::try_join!`] would
/// cancel the other the moment one returned, and **a 404 here is the normal case**, so the missing
/// directory would take the present one down with it.
///
/// The results are then triaged in `WORKFLOW_DIRS` order rather than in the order they arrived,
/// so `first_error` still means "the first directory's error" and the message the user sees does
/// not depend on which request the network happened to answer first.
async fn discover(
    api: &Api,
    slug: &RepoSlug,
    git_ref: Option<&str>,
) -> Result<Vec<ContentsResponse>> {
    // Destructured rather than indexed: adding a fourth directory to `WORKFLOW_DIRS` then fails
    // to compile here instead of quietly becoming a directory that is never read.
    let [gitea, github] = WORKFLOW_DIRS;
    let (a, b) =
        futures::join!(list_dir(api, slug, gitea, git_ref), list_dir(api, slug, github, git_ref));

    let mut out: Vec<ContentsResponse> = Vec::new();
    let mut first_error: Option<Error> = None;

    for result in [a, b] {
        match result {
            Ok(entries) => out.extend(
                entries.into_iter().filter(|e| e.r#type == "file" && is_workflow_file(&e.name)),
            ),
            // A repository without `.github/workflows` is the normal case, not a failure.
            Err(e) if is_not_found(&e) => {}
            Err(e) => first_error = first_error.or(Some(e)),
        }
    }
    // Every directory failing for a reason other than absence — a missing repository, a token
    // without `read:repository` — must surface, not read as "no workflows".
    if out.is_empty()
        && let Some(e) = first_error
    {
        return Err(e);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// List one directory.
///
/// `get_contents` returns a `ContentsResponseOrList` because the route answers with a single
/// entry for a file and an array for a directory, which the specification does not say — see
/// `[one_or_many]` in the generator's `overrides.toml`. Here the path is always a directory, so
/// `into_vec` is the whole of the difference.
async fn list_dir(
    api: &Api,
    slug: &RepoSlug,
    dir: &str,
    git_ref: Option<&str>,
) -> Result<Vec<ContentsResponse>> {
    let mut query = query::RepoGetContentsQuery::default();
    if let Some(r) = git_ref {
        query = query.with_ref(r);
    }
    let entries = api.repo().get_contents(&slug.owner, &slug.name, dir, &query).await?;
    Ok(entries.into_vec())
}

/// Turn what the user typed into the file we will act on.
async fn resolve_file(
    api: &Api,
    slug: &RepoSlug,
    given: &str,
    git_ref: Option<&str>,
) -> Result<ContentsResponse> {
    let files = discover(api, slug, git_ref).await?;
    let wanted = file_name(given);
    if let Some(hit) = files.iter().find(|e| e.path == given || e.name == wanted) {
        return Ok(hit.clone());
    }
    Err(Error::new(ErrorKind::ResourceNotFound {
        kind: "workflow",
        id: given.to_owned(),
        slug: Some(slug.to_string()),
        // Discovered locally: there was no server reply to quote.
        server_message: None,
    }))
}

/// Read a workflow file's text.
///
/// `raw/{filepath}` rather than `contents/{filepath}`: the contents endpoint answers with
/// base64, and `gea` has no base64 decoder (nor a reason to grow one for this).
async fn read_source(
    api: &Api,
    slug: &RepoSlug,
    path: &str,
    git_ref: Option<&str>,
) -> Result<String> {
    let q = query::RepoGetRawFileQuery { r#ref: git_ref.map(str::to_owned) };
    let (_, mut body) = api.repo().get_raw_file(&slug.owner, &slug.name, path, &q).await?;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    // Lossy: a workflow file is text, and a stray byte should not fail the command that was
    // going to tell you which labels it needs.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn read_workflow(
    api: &Api,
    slug: &RepoSlug,
    path: &str,
    git_ref: Option<&str>,
) -> Result<yaml::Parsed> {
    Ok(yaml::parse(&read_source(api, slug, path, git_ref).await?))
}

/// Read every discovered file, [`READ_CONCURRENCY`] at a time, **in `entries` order**.
///
/// `buffered` rather than `buffer_unordered`: the returned vector is zipped straight back onto
/// `entries`, which [`discover`] has already sorted by path, and an unordered stream would
/// silently pair each row's columns with a different file's body. It also means the error this
/// returns is the first *in path order*, not the first to arrive — the same error the serial
/// loop this replaced used to report.
///
/// Split out from [`list`] so a `FakeTransport` test can count the requests without a
/// [`Runtime`].
async fn read_all(
    api: &Api,
    slug: &RepoSlug,
    entries: &[ContentsResponse],
    git_ref: Option<&str>,
) -> Result<Vec<yaml::Parsed>> {
    futures::stream::iter(
        entries.iter().map(|entry| read_workflow(api, slug, &entry.path, git_ref)),
    )
    .buffered(READ_CONCURRENCY)
    .try_collect()
    .await
}

/// The ref to dispatch on: the checked-out branch, else the repository's default branch.
async fn default_ref(rt: &Runtime, api: &Api, slug: &RepoSlug) -> Result<String> {
    if let Ok(Some(branch)) = rt.git().current_branch() {
        return Ok(branch);
    }
    Ok(api.repo().get(&slug.owner, &slug.name).await?.default_branch)
}

/// Each workflow's `state` from the Actions listing, keyed by file name (which is what Gitea uses
/// as a workflow's id).
///
/// Best-effort: it is one column, and a token or instance that cannot answer it should cost that
/// column, not the command. An empty map leaves the column blank.
async fn states(api: &Api, slug: &RepoSlug) -> BTreeMap<String, String> {
    match api.workflow().list(&slug.owner, &slug.name).await {
        Ok(r) => r.workflows.into_iter().map(|w| (w.id, w.state)).collect(),
        Err(_) => BTreeMap::new(),
    }
}

fn as_json(entry: &ContentsResponse, parsed: &yaml::Parsed, state: &str) -> Value {
    serde_json::json!({
        "path": entry.path,
        "name": entry.name,
        "workflow_name": parsed.display_name(&entry.path),
        "dispatch": parsed.dispatchable(),
        "events": parsed.events,
        "runs_on": parsed.runs_on,
        "jobs": parsed.jobs,
        "sha": entry.sha,
        "html_url": entry.html_url,
        "state": state,
    })
}

fn is_workflow_file(name: &str) -> bool {
    name.ends_with(".yml") || name.ends_with(".yaml")
}

fn file_name(given: &str) -> &str {
    given.rsplit('/').next().unwrap_or(given)
}

fn join(values: &[String]) -> String {
    if values.is_empty() { "-".to_owned() } else { values.join(", ") }
}

fn is_not_found(e: &Error) -> bool {
    matches!(e.kind(), ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. })
}

fn read_input(source: &str) -> Result<Vec<u8>> {
    if source == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
            .map_err(|e| support::usage(format!("--input -: could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read(source).map_err(|e| support::usage(format!("--input {source}: {e}")))
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use std::sync::Arc;

    fn api_for(fake: Arc<FakeTransport>) -> Api {
        Api::new(
            Client::builder("https://git.example.org", Auth::token("t"))
                .transport(fake)
                .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
                .probe_404(false)
                .build()
                .expect("a well-formed base URL"),
        )
    }

    fn run_args(raw: &[&str], typed: &[&str], input: Option<&str>) -> RunArgs {
        RunArgs {
            file: "ci.yml".to_owned(),
            git_ref: Some("main".to_owned()),
            raw_field: raw.iter().map(|s| (*s).to_owned()).collect(),
            field: typed.iter().map(|s| (*s).to_owned()).collect(),
            input: input.map(str::to_owned),
        }
    }

    /// `list` must look in both directories Gitea reads, and must not fail because one of them is
    /// absent — which is the normal case.
    #[tokio::test]
    async fn discovery_reads_both_directories_and_tolerates_a_missing_one() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/contents/.gitea/workflows",
                    Canned::json(
                        200,
                        r#"[{"name":"ci.yml","path":".gitea/workflows/ci.yml","type":"file"},
                            {"name":"README.md","path":".gitea/workflows/README.md","type":"file"}]"#,
                    ),
                )
                .fallback(Canned::json(404, r#"{"message":"path does not exist"}"#)),
        );
        let files = discover(&api_for(fake.clone()), &RepoSlug::new("o", "r"), None).await.unwrap();
        // The non-YAML file is not a workflow.
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].path, ".gitea/workflows/ci.yml");
        let paths: Vec<String> = fake.calls().iter().map(|c| c.path.clone()).collect();
        for dir in WORKFLOW_DIRS {
            assert!(paths.iter().any(|p| p.ends_with(dir)), "{dir} was never listed: {paths:?}");
        }
    }

    /// Bug this prevents: the directory listings overlap, so the error the user is shown
    /// would otherwise be whichever request the network happened to answer first — a message that
    /// changes between runs of the same command against the same broken instance. The triage runs
    /// in `WORKFLOW_DIRS` order, so the *first directory's* error is the one that survives.
    #[tokio::test]
    async fn a_concurrent_discovery_still_reports_the_first_directorys_error() {
        // `.gitea` is scope-shaped, `.github` is a 500. Whatever order they come back in, the
        // scope error is the one the user is told about.
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/contents/.gitea/workflows",
                    Canned::json(403, r#"{"message":"token does not have scope"}"#),
                )
                .fallback(Canned::json(500, r#"{"message":"boom"}"#)),
        );
        let e = discover(&api_for(fake.clone()), &RepoSlug::new("o", "r"), None).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::InsufficientScope { .. }), "{e:?}");
        // Both are still asked for, concurrently: neither was cancelled by an early return, the
        // way `try_join!` would have cancelled it.
        let paths: Vec<String> = fake.calls().iter().map(|c| c.path.clone()).collect();
        for dir in WORKFLOW_DIRS {
            assert!(paths.iter().any(|p| p.ends_with(dir)), "{dir} was never listed: {paths:?}");
        }
        assert_eq!(fake.call_count(), WORKFLOW_DIRS.len(), "{paths:?}");

        // Swap the two failures over and the *other* error wins, which is what makes this a test
        // of position rather than of which error happens to look more interesting.
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/contents/.gitea/workflows",
                    Canned::json(500, r#"{"message":"boom"}"#),
                )
                .fallback(Canned::json(403, r#"{"message":"token does not have scope"}"#)),
        );
        let e = discover(&api_for(fake), &RepoSlug::new("o", "r"), None).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::ServerError { .. }), "{e:?}");
    }

    /// Bug this prevents: one request per file and no more — `--limit` caps the *requests* by
    /// capping the slice this is given, and a concurrent read that fanned out over everything
    /// `discover` found would quietly undo that.
    ///
    /// The pairing is asserted alongside it, because the failure mode of getting the ordering
    /// wrong is a plausible-looking table with each row's `DISPATCH`/`RUNS-ON` columns taken from
    /// a different file. That half cannot fail here — `FakeTransport` resolves without ever
    /// returning `Pending`, so even `buffer_unordered` comes back in order (the same caveat
    /// `gitea_core::capabilities`' own concurrency test records) — and it is written down so
    /// that the pairing is at least stated where the code that must preserve it can be read.
    #[tokio::test]
    async fn every_workflow_file_is_read_once_and_the_bodies_stay_in_path_order() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/raw/.gitea/workflows/a.yml",
                    Canned::text(
                        200,
                        "name: A
on: push
",
                    ),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/raw/.gitea/workflows/b.yml",
                    Canned::text(
                        200,
                        "name: B
on: push
",
                    ),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/raw/.gitea/workflows/c.yml",
                    Canned::text(
                        200,
                        "name: C
on: push
",
                    ),
                ),
        );
        let entries: Vec<ContentsResponse> = ["a.yml", "b.yml", "c.yml"]
            .iter()
            .map(|n| ContentsResponse {
                name: (*n).to_owned(),
                path: format!(".gitea/workflows/{n}"),
                r#type: "file".to_owned(),
                ..Default::default()
            })
            .collect();

        let parsed = read_all(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &entries, None)
            .await
            .unwrap();
        let names: Vec<String> =
            parsed.iter().zip(&entries).map(|(p, e)| p.display_name(&e.path)).collect();
        assert_eq!(names, ["A", "B", "C"], "the bodies came back paired with the wrong files");
        assert_eq!(fake.call_count(), entries.len());
    }

    /// Bug this prevents: every directory 404ing because the *repository* is gone, and the command
    /// cheerfully reporting "no workflows" — which sends the user looking for the wrong bug.
    #[tokio::test]
    async fn a_real_failure_is_not_reported_as_no_workflows() {
        let fake = Arc::new(
            FakeTransport::new().fallback(Canned::json(403, r#"{"message":"token has no scope"}"#)),
        );
        let e = discover(&api_for(fake), &RepoSlug::new("o", "r"), None).await.unwrap_err();
        // The classifier turns a scope-shaped 403 into `InsufficientScope`, which names the scope
        // to ask for — the point being that the error survives at all rather than being swallowed
        // into an empty list.
        assert!(matches!(e.kind(), ErrorKind::InsufficientScope { .. }), "{e:?}");
        assert_eq!(e.exit_code(), 4);
    }

    /// Dispatch takes the workflow *filename*, and a path is reduced to it. Bug this prevents:
    /// posting to `…/workflows/.gitea%2Fworkflows%2Fci.yml/dispatches`, which 404s.
    #[tokio::test]
    async fn dispatch_posts_to_the_filename_with_string_inputs() {
        let fake = Arc::new(FakeTransport::new().on(
            "POST".parse().unwrap(),
            "/api/v1/repos/o/r/actions/workflows/ci.yml/dispatches",
            Canned::json(
                200,
                r#"{"workflow_run_id":12,"html_url":"https://x/o/r/actions/runs/3"}"#,
            ),
        ));
        let api = api_for(fake.clone());
        let inputs =
            collect_inputs(&run_args(&["environment=staging"], &["verbose=true"], None)).unwrap();
        let body = gitea_model::CreateActionWorkflowDispatch {
            inputs: Some(inputs),
            r#ref: "main".into(),
        };
        let started = send_dispatch(
            &api,
            &RepoSlug::new("o", "r"),
            file_name(".gitea/workflows/ci.yml"),
            &body,
        )
        .await
        .unwrap();
        assert_eq!(started.workflow_run_id, 12);

        // Matched on path, not on `calls()[0]`: an assertion that depends on *arrival order* is
        // one concurrent request away from going flaky instead of failing honestly.
        let posted = fake
            .calls()
            .into_iter()
            .find(|c| c.path == "/api/v1/repos/o/r/actions/workflows/ci.yml/dispatches")
            .expect("the dispatch POST");
        // Without this the server answers 204 and the typed method fails to decode a success.
        assert!(posted.query.contains("return_run_details=true"), "{}", posted.query);
        let sent: Value = serde_json::from_slice(&posted.body.expect("a JSON body")).unwrap();
        // Every input is a string on the wire, `-F verbose=true` included: that is what
        // `CreateActionWorkflowDispatch.inputs` is, and a boolean here is rejected by the server.
        assert_eq!(sent["inputs"]["environment"], "staging");
        assert_eq!(sent["inputs"]["verbose"], "true");
        assert_eq!(sent["ref"], "main");
    }

    /// Bug this prevents: `enable`/`disable` hand-building a route the spec now has, and drifting
    /// from it — the generated method is the one kept in step with the vendored spec.
    #[tokio::test]
    async fn toggles_address_the_workflow_by_file_name() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "PUT".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/workflows/ci.yml/disable",
                    Canned::new(204),
                )
                .on(
                    "PUT".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/workflows/ci.yml/enable",
                    Canned::new(204),
                ),
        );
        let api = api_for(fake.clone());
        api.workflow().disable("o", "r", "ci.yml").await.unwrap();
        api.workflow().enable("o", "r", "ci.yml").await.unwrap();
        assert_eq!(fake.call_count(), 2);
    }

    /// The STATE column must come from the Actions listing, keyed by file name, and must not take
    /// the command down with it when that listing cannot be read.
    #[tokio::test]
    async fn states_are_keyed_by_file_name_and_best_effort() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/workflows",
            Canned::json(
                200,
                r#"{"total_count":1,"workflows":[{"id":"ci.yml","name":"CI","state":"disabled_manually"}]}"#,
            ),
        ));
        let got = states(&api_for(fake), &RepoSlug::new("o", "r")).await;
        assert_eq!(got["ci.yml"], "disabled_manually");

        let fake = Arc::new(FakeTransport::new().fallback(Canned::json(500, r#"{"message":"x"}"#)));
        assert!(states(&api_for(fake), &RepoSlug::new("o", "r")).await.is_empty());
    }

    /// `--input -` is this build's spelling of `gh workflow run --json`, and it must agree with
    /// `gea api`'s field convention: `-f`/`-F` override the JSON object key by key.
    #[test]
    fn inputs_come_from_json_and_are_overridden_by_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inputs.json");
        std::fs::write(&path, r#"{"environment":"production","count":3,"verbose":false}"#).unwrap();
        let args = run_args(&["environment=staging"], &[], Some(path.to_str().unwrap()));
        let inputs = collect_inputs(&args).unwrap();
        assert_eq!(inputs["environment"], "staging", "an explicit -f wins over --input");
        assert_eq!(inputs["count"], "3", "a JSON number becomes its string form");
        assert_eq!(inputs["verbose"], "false");
    }

    /// Bug this prevents: an array or object silently becoming `["a","b"]` in an input the
    /// workflow then reads as literal JSON.
    #[test]
    fn a_non_scalar_input_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, r#"{"tags":["a","b"]}"#).unwrap();
        let e = collect_inputs(&run_args(&[], &[], Some(path.to_str().unwrap()))).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("strings"), "{e}");

        let e = collect_inputs(&run_args(&["tags[]=a"], &[], None)).unwrap_err();
        assert!(e.to_string().contains("flat name"), "{e}");
    }

    /// A workflow with no `workflow_dispatch` cannot be started, and saying so beats the server's
    /// 404 — which is indistinguishable from a misspelled file name.
    #[test]
    fn a_workflow_without_dispatch_is_refused_before_the_request() {
        let parsed = yaml::parse("on: push\njobs:\n  a:\n    runs-on: docker\n");
        let e = check_inputs(&Term::piped(), &parsed, &BTreeMap::new(), "ci.yml").unwrap_err();
        assert!(e.to_string().contains("workflow_dispatch"), "{e}");
    }

    /// A required input that was not given is an error naming the flag to pass, before any
    /// request. Bug this prevents: starting a run that immediately fails on an empty input.
    #[test]
    fn a_missing_required_input_names_the_flag() {
        let parsed = yaml::parse(
            "on:\n  workflow_dispatch:\n    inputs:\n      environment:\n        required: true\n        type: choice\n        options:\n          - staging\n",
        );
        let e = check_inputs(&Term::piped(), &parsed, &BTreeMap::new(), "ci.yml").unwrap_err();
        assert!(e.to_string().contains("-f environment="), "{e}");
        assert!(e.to_string().contains("staging"), "{e}");

        let mut given = BTreeMap::new();
        given.insert("environment".to_owned(), "staging".to_owned());
        assert!(check_inputs(&Term::piped(), &parsed, &given, "ci.yml").is_ok());
        // An input the file does not declare is a warning, not a refusal: the scanner is
        // shallow, and being wrong here must not block a dispatch that would have worked.
        let mut odd = BTreeMap::new();
        odd.insert("environment".to_owned(), "staging".to_owned());
        odd.insert("typo".to_owned(), "1".to_owned());
        assert!(check_inputs(&Term::piped(), &parsed, &odd, "ci.yml").is_ok());
    }

    /// Bug this prevents: `check_inputs` reporting only the *first* missing required input, so a
    /// workflow wanting three of them costs three dispatches to discover — each one a request
    /// that ends in the same refusal. Every name must be in the one message.
    #[test]
    fn every_missing_required_input_is_named_at_once() {
        let parsed = yaml::parse(
            "on:\n  workflow_dispatch:\n    inputs:\n      environment:\n        required: true\n        type: choice\n        options:\n          - staging\n      version:\n        required: true\n      notes:\n        required: false\n",
        );
        let e = check_inputs(&Term::piped(), &parsed, &BTreeMap::new(), "ci.yml").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        let text = e.to_string();
        assert!(text.contains("-f environment="), "{text}");
        assert!(text.contains("-f version="), "{text}");
        assert!(text.contains("staging"), "{text}");
        // An input that is not required must not be demanded.
        assert!(!text.contains("notes"), "{text}");

        // Give one of the two and only the other is still asked for.
        let mut given = BTreeMap::new();
        given.insert("environment".to_owned(), "staging".to_owned());
        let e = check_inputs(&Term::piped(), &parsed, &given, "ci.yml").unwrap_err();
        assert!(e.to_string().contains("-f version="), "{e}");
        assert!(!e.to_string().contains("-f environment="), "{e}");
    }

    /// Snapshot of `workflow list` in both modes.
    #[test]
    fn workflow_list_output_goldens() {
        use crate::output::Term;
        let entry = ContentsResponse {
            name: "ci.yml".to_owned(),
            path: ".gitea/workflows/ci.yml".to_owned(),
            sha: "abc123".to_owned(),
            html_url: "https://git.example.org/o/r/src/branch/main/.gitea/workflows/ci.yml"
                .to_owned(),
            r#type: "file".to_owned(),
            ..Default::default()
        };
        let parsed = yaml::parse(
            "name: CI\non:\n  push:\n  workflow_dispatch:\njobs:\n  build:\n    runs-on: docker\n",
        );
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(100), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts {
                    json: Some("path,state,dispatch,runs_on".into()),
                    ..Default::default()
                },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Custom(WORKFLOW_FIELDS),
                value: Value::Array(vec![as_json(&entry, &parsed, "active")]),
                count: 1,
                total: None,
                noun: "workflows",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["NAME", "FILE", "STATE", "DISPATCH", "RUNS-ON", "EVENTS", "JOBS"]);
                t.row([
                    parsed.display_name(&entry.path),
                    entry.name.clone(),
                    "active".to_owned(),
                    "yes".to_owned(),
                    join(&parsed.runs_on),
                    join(&parsed.events),
                    join(&parsed.jobs),
                ]);
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

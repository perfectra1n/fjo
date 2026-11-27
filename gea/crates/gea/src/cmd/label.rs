//! `gea label` — labels, at repository *and* organization scope.
//!
//! # Why `--org` is on every subcommand
//!
//! Gitea (like Gitea, unlike GitHub) has organization-level labels: `/orgs/{org}/labels`
//! alongside `/repos/{owner}/{repo}/labels`. They are separate objects with separate ids on
//! separate routes, and an issue can carry either. A `label` group that only knew about
//! repository labels would silently be half a group — `gea label list` on a repo in an
//! organization would omit the labels most of its issues actually use.
//!
//! There is a known upstream wart worth being explicit about: creating an organization label
//! does **not** associate it with any repository, and it does not appear in a repository's
//! label list. That is Gitea's behaviour, not a bug here, and it is why `list` renders a
//! `SCOPE` column when both scopes are shown — otherwise "I created it and it vanished" is the
//! only conclusion available.
//!
//! # `clone`
//!
//! `gea label clone <owner/name>` copies labels from another repository, which is the one
//! thing in this group that no single API call does: it is a list plus one create per label,
//! with existing names skipped so that re-running it is safe.

use clap::{Args as ClapArgs, Subcommand};
use gitea_core::error::Result;
use gitea_core::types::{RepoRef, RepoSlug};
use gitea_model::Label;

use crate::cmd::issue::shared::{self, Cx, LabelPatch, LabelScope, Out, label_chip};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Manage repository and organization labels.

Use --org to manage organization labels. They are available to the organization's
repositories but are only listed here with --org.

  gea label list
  gea label create bug -c e11d21 -d 'Something is broken'
  gea label edit bug --name defect
  gea label clone gitea/gitea
  gea label list --org acme";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List labels
    List(ListArgs),
    /// Create a label
    Create(CreateArgs),
    /// Change a label's name, colour or description
    Edit(EditArgs),
    /// Delete a label
    Delete(DeleteArgs),
    /// Copy labels from another repository
    Clone(CloneArgs),
}

/// `--org acme` moves every subcommand to the organization's labels.
#[derive(Debug, ClapArgs, Clone)]
pub struct ScopeArgs {
    /// Work on this organization's labels instead of the repository's
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,

    /// Maximum number of labels (also settable as --limit)
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Also show the organization's labels alongside the repository's
    #[arg(long, conflicts_with = "org")]
    pub include_org: bool,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Label name
    #[arg(value_name = "NAME")]
    pub name: String,

    #[command(flatten)]
    pub scope: ScopeArgs,

    /// Colour as RRGGBB, with or without a leading '#'
    ///
    /// Short-only: the global `--color` (when to colourise output) already claims the long
    /// name, and clap answers a duplicate long name with a panic rather than an error.
    #[arg(short = 'c', value_name = "HEX")]
    pub color: Option<String>,

    /// What the label means
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Make the label mutually exclusive within its `scope/` prefix (Gitea-only)
    #[arg(long)]
    pub exclusive: bool,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    /// The label to change, by name
    #[arg(value_name = "NAME")]
    pub name: String,

    #[command(flatten)]
    pub scope: ScopeArgs,

    /// New name
    #[arg(long = "name", value_name = "NAME")]
    pub new_name: Option<String>,

    /// New colour, as RRGGBB. Short-only; see `CreateArgs::color`
    #[arg(short = 'c', value_name = "HEX")]
    pub color: Option<String>,

    /// New description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Make the label mutually exclusive, or stop it being so
    #[arg(long, value_name = "BOOL")]
    pub exclusive: Option<bool>,

    /// Archive the label, or un-archive it
    #[arg(long, value_name = "BOOL")]
    pub archived: Option<bool>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The label to delete, by name
    #[arg(value_name = "NAME")]
    pub name: String,

    #[command(flatten)]
    pub scope: ScopeArgs,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct CloneArgs {
    /// Repository to copy labels from
    #[arg(value_name = "OWNER/NAME", value_parser = clap::value_parser!(RepoRef))]
    pub source: RepoRef,

    #[command(flatten)]
    pub scope: ScopeArgs,

    /// Overwrite a label of the same name instead of leaving it alone
    #[arg(long)]
    pub overwrite: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let Some(out) = Out::prepare(globals, gitea_client::fields::FIELDS_LABEL)? else {
        return Ok(());
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        // `--org` genuinely needs no repository, and resolving one anyway would make
        // `gea label list --org acme` fail outside a checkout for no reason.
        let scope_args = match &args.cmd {
            Cmd::List(a) => &a.scope,
            Cmd::Create(a) => &a.scope,
            Cmd::Edit(a) => &a.scope,
            Cmd::Delete(a) => &a.scope,
            Cmd::Clone(a) => &a.scope,
        };
        let cx = match &scope_args.org {
            Some(_) => Cx::global(&rt, globals, out),
            None => Cx::in_repo(&rt, globals, out)?,
        };
        let scope = scope_of(&cx, scope_args)?;

        match &args.cmd {
            Cmd::List(a) => list(&cx, globals, &scope, a).await,
            Cmd::Create(a) => create(&cx, &scope, a).await,
            Cmd::Edit(a) => edit(&cx, &scope, a).await,
            Cmd::Delete(a) => delete(&cx, &scope, a).await,
            Cmd::Clone(a) => clone_from(&cx, &scope, a).await,
        }
    })
}

fn scope_of(cx: &Cx, args: &ScopeArgs) -> Result<LabelScope> {
    Ok(match &args.org {
        Some(org) => LabelScope::Org(org.clone()),
        None => LabelScope::Repo(cx.repo()?.clone()),
    })
}

// ---------------------------------------------------------------------------------- list

async fn list(cx: &Cx, globals: &GlobalOpts, scope: &LabelScope, a: &ListArgs) -> Result<()> {
    let mut rows: Vec<(String, Label)> = if a.include_org {
        // Two independent scope walks, so they run at the same time rather than one after the
        // other.
        //
        // `join!`, never `try_join!`, and that is the whole point: the owner of a personal
        // repository is a user, and `/orgs/{user}/labels` 404s for one. That is not a failure of
        // `--include-org` — the user asked for organization labels "as well", not "instead" — so
        // the org arm is *allowed* to fail and is traced and dropped. `try_join!` would abandon
        // the repository's labels the moment that tolerated 404 arrived and turn a working
        // command into a hard error.
        let owner = cx.repo()?.owner.clone();
        let org_scope = LabelScope::Org(owner.clone());
        let (repo, org) = futures::join!(
            shared::all_labels(&cx.api, scope),
            shared::all_labels(&cx.api, &org_scope)
        );
        // Reassembled repository-first, organization-second, which is the order the serial
        // version produced and the order the SCOPE column is read in.
        let mut rows: Vec<(String, Label)> =
            repo?.into_iter().map(|l| (scope_word(scope), l)).collect();
        match org {
            Ok(org) => rows.extend(org.into_iter().map(|l| ("org".to_owned(), l))),
            Err(e) => cx.trace(&format!("--include-org: {owner} has no organization labels ({e})")),
        }
        rows
    } else {
        shared::all_labels(&cx.api, scope)
            .await?
            .into_iter()
            .map(|l| (scope_word(scope), l))
            .collect()
    };

    // Counted before the cap is applied: `all_labels` walks the whole collection, so the total
    // the banner needs is already here and costs no second request.
    let total = rows.len() as u64;
    let cap = support::limit(a.limit, globals);
    rows.truncate(cap);

    if cx.out.is_machine() {
        let labels: Vec<&Label> = rows.iter().map(|(_, l)| l).collect();
        return cx.out.machine(support::to_value(&labels)?, &cx.term);
    }
    if rows.is_empty() {
        support::note(&cx.term, &format!("no labels in {scope}"));
        return cx.out.table(&Table::new(&cx.term));
    }

    let mut table = Table::new(&cx.term);
    let scoped = a.include_org;
    if scoped {
        table.headers(["SCOPE", "NAME", "COLOR", "DESCRIPTION"]);
    } else {
        table.headers(["NAME", "COLOR", "DESCRIPTION"]);
    }
    for (where_, label) in &rows {
        let mut cells = Vec::new();
        if scoped {
            cells.push(where_.clone());
        }
        cells.push(label_chip(&cx.term, label));
        cells.push(normalise_color(&label.color));
        cells.push(label.description.clone());
        table.row(cells);
    }
    table.banner(support::banner(
        rows.len(),
        Some(total),
        &format!("label{} in {scope}", support::plural_s(total)),
    ));
    cx.out.table(&table)
}

fn scope_word(scope: &LabelScope) -> String {
    match scope {
        LabelScope::Repo(_) => "repo".to_owned(),
        LabelScope::Org(_) => "org".to_owned(),
    }
}

// -------------------------------------------------------------------------------- create

/// Gitea's own default when the web UI creates a label with no colour chosen.
///
/// The API rejects an empty colour outright, so a porcelain `create` has to pick something; the
/// alternative is making `-c` mandatory, which no other label tool does.
const DEFAULT_COLOR: &str = "#ededed";

async fn create(cx: &Cx, scope: &LabelScope, a: &CreateArgs) -> Result<()> {
    let color = match &a.color {
        Some(c) => normalise_color(c),
        None => DEFAULT_COLOR.to_owned(),
    };
    validate_color(&color)?;

    let body = gitea_model::CreateLabelOption {
        name: a.name.clone(),
        color,
        description: a.description.clone(),
        exclusive: Some(a.exclusive),
        is_archived: Some(false),
    };
    let label = match scope {
        LabelScope::Repo(slug) => {
            cx.api.issue().create_label(&slug.owner, &slug.name, &body).await?
        }
        LabelScope::Org(org) => cx.api.org().create_label(org, &body).await?,
    };
    emit(cx, &label, "Created", scope)
}

// ---------------------------------------------------------------------------------- edit

async fn edit(cx: &Cx, scope: &LabelScope, a: &EditArgs) -> Result<()> {
    let mut patch = LabelPatch {
        name: a.new_name.clone(),
        description: a.description.clone(),
        exclusive: a.exclusive,
        is_archived: a.archived,
        ..LabelPatch::default()
    };
    if let Some(c) = &a.color {
        let color = normalise_color(c);
        validate_color(&color)?;
        patch.color = Some(color);
    }
    if patch.is_empty() {
        return Err(support::usage(
            "nothing to change; pass --name, -c/--color, -d/--description, --exclusive or \
             --archived",
        ));
    }

    // The edit endpoint is by id, and the user typed a name, so the id is looked up. This is
    // also the check that the label exists: a PATCH against a guessed id would hit a *different*
    // label.
    let existing = by_name(cx, scope, &a.name).await?;
    let label = shared::patch_label(cx, scope, existing.id, &patch).await?;
    emit(cx, &label, "Updated", scope)
}

// -------------------------------------------------------------------------------- delete

async fn delete(cx: &Cx, scope: &LabelScope, a: &DeleteArgs) -> Result<()> {
    let existing = by_name(cx, scope, &a.name).await?;
    cx.confirm(
        &format!(
            "Delete label {:?} from {scope} (it is removed from every issue using it)",
            existing.name
        ),
        a.yes,
    )?;
    match scope {
        LabelScope::Repo(slug) => {
            cx.api.issue().delete_label(&slug.owner, &slug.name, existing.id.get()).await?
        }
        LabelScope::Org(org) => cx.api.org().delete_label(org, existing.id.get()).await?,
    }
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&existing)?, &cx.term);
    }
    cx.out.text(&format!("Deleted label {:?} from {scope}\n", existing.name))
}

// --------------------------------------------------------------------------------- clone

async fn clone_from(cx: &Cx, scope: &LabelScope, a: &CloneArgs) -> Result<()> {
    let source = RepoSlug::new(a.source.slug.owner.clone(), a.source.slug.name.clone());
    let from = shared::all_labels(&cx.api, &LabelScope::Repo(source.clone())).await?;
    if from.is_empty() {
        support::note(&cx.term, &format!("{source} has no labels to copy"));
        return cx.out.table(&Table::new(&cx.term));
    }
    let existing = shared::all_labels(&cx.api, scope).await?;

    let mut created: Vec<Label> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut updated: Vec<Label> = Vec::new();
    for label in &from {
        let hit = existing.iter().find(|l| l.name.eq_ignore_ascii_case(&label.name));
        match (hit, a.overwrite) {
            // Left alone by default: a label already in use carries meaning that the source
            // repository's colour and description should not silently replace.
            (Some(_), false) => skipped.push(label.name.clone()),
            (Some(target), true) => {
                let patch = LabelPatch {
                    color: Some(normalise_color(&label.color)),
                    description: Some(label.description.clone()),
                    exclusive: Some(label.exclusive),
                    ..LabelPatch::default()
                };
                updated.push(shared::patch_label(cx, scope, target.id, &patch).await?);
            }
            (None, _) => {
                let body = gitea_model::CreateLabelOption {
                    name: label.name.clone(),
                    color: normalise_color(&label.color),
                    description: Some(label.description.clone()),
                    exclusive: Some(label.exclusive),
                    is_archived: Some(label.is_archived),
                };
                created.push(match scope {
                    LabelScope::Repo(slug) => {
                        cx.api.issue().create_label(&slug.owner, &slug.name, &body).await?
                    }
                    LabelScope::Org(org) => cx.api.org().create_label(org, &body).await?,
                });
            }
        }
    }

    if cx.out.is_machine() {
        let touched: Vec<&Label> = created.iter().chain(updated.iter()).collect();
        return cx.out.machine(support::to_value(&touched)?, &cx.term);
    }
    let mut text = format!(
        "Copied {} label{} from {source} to {scope}\n",
        created.len() + updated.len(),
        if created.len() + updated.len() == 1 { "" } else { "s" }
    );
    if !skipped.is_empty() {
        // Three parts, not one pronoun: "they already exist" wants the subject form and
        // "replace them" the object form, so one substitution cannot serve both.
        let (subject, verb, object) =
            if skipped.len() == 1 { ("it", "exists", "it") } else { ("they", "exist", "them") };
        text.push_str(&format!(
            "Left {} alone because {subject} already {verb} (pass --overwrite to replace \
             {object})\n",
            skipped.join(", ")
        ));
    }
    cx.out.text(&text)
}

// ------------------------------------------------------------------------------- helpers

/// A label by name, with a message that lists what does exist.
async fn by_name(cx: &Cx, scope: &LabelScope, name: &str) -> Result<Label> {
    let labels = shared::all_labels(&cx.api, scope).await?;
    if let Some(hit) = labels.iter().find(|l| l.name.eq_ignore_ascii_case(name)) {
        return Ok(hit.clone());
    }
    let mut names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
    names.sort_unstable();
    Err(support::usage(format!(
        "{scope} has no label named {name:?}; it has: {}",
        if names.is_empty() { "none".to_owned() } else { names.join(", ") }
    )))
}

/// `e11d21` and `#e11d21` both become `#e11d21`.
///
/// Gitea accepts both on the way in and answers with the bare form, so normalising on the way
/// out keeps `gea label list` stable whichever way the label was created — and keeps the value
/// paste-able back into `-c`.
fn normalise_color(color: &str) -> String {
    let hex = color.trim().trim_start_matches('#');
    if hex.is_empty() { String::new() } else { format!("#{}", hex.to_ascii_lowercase()) }
}

fn validate_color(color: &str) -> Result<()> {
    let hex = color.trim_start_matches('#');
    // Gitea accepts 3- and 6-digit hex. Checking here rather than letting the server refuse
    // means the message can say what the format is; the server's is "color is not valid".
    if (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(support::usage(format!(
        "{color:?} is not a colour; write it as RRGGBB or RGB hex, e.g. -c e11d21"
    )))
}

fn emit(cx: &Cx, label: &Label, verb: &str, scope: &LabelScope) -> Result<()> {
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(label)?, &cx.term);
    }
    cx.out.text(&format!(
        "{verb} label {:?} ({}) in {scope}\n",
        label.name,
        normalise_color(&label.color)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use std::sync::{Arc, Mutex};

    macro_rules! method {
        ($name:literal) => {
            $name.parse().expect("a valid HTTP method")
        };
    }

    fn slug() -> RepoSlug {
        RepoSlug::new("perf3ct", "gea")
    }

    fn cx(
        fake: Arc<FakeTransport>,
        buf: &Arc<Mutex<Vec<u8>>>,
        globals: &GlobalOpts,
        term: Term,
    ) -> Cx {
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(fake)
            .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
            .build()
            .expect("a well-formed base URL");
        let out = Out::to_buffer(globals, gitea_client::fields::FIELDS_LABEL, buf);
        Cx::for_test(gitea_client::Api::new(client), Some(slug()), term, out)
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8 output")
    }

    const LABELS: &str = r#"[
        {"id": 4, "name": "bug", "color": "e11d21", "description": "Something is broken"},
        {"id": 5, "name": "ci", "color": "1d76db", "description": ""}
    ]"#;

    fn label_routes() -> FakeTransport {
        FakeTransport::new()
            .on(
                method!("GET"),
                "/api/v1/settings/api",
                Canned::json(200, r#"{"max_response_items":50}"#),
            )
            .on_sequence(
                method!("GET"),
                "/api/v1/repos/perf3ct/gea/labels",
                Vec::from([Canned::json(200, LABELS), Canned::json(200, "[]")]),
            )
    }

    #[tokio::test]
    async fn list_renders_a_table_and_normalises_colours() {
        let fake = Arc::new(label_routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::tty(80));
        let args = ListArgs { scope: ScopeArgs { org: None }, limit: None, include_org: false };
        list(&cx, &globals, &LabelScope::Repo(slug()), &args).await.unwrap();
        insta::assert_snapshot!("list_human", text(&buf));
    }

    /// Bug this prevents — the reported one. Against a real Gitea with 61 labels the banner
    /// said `Showing 30 labels`, which is the one thing a truncation banner must not do: claim a
    /// count without admitting the 31 it withheld. `all_labels` walks the whole collection, so
    /// the total is already in hand before `-L` is applied and costs no second request.
    #[tokio::test]
    async fn a_truncated_list_says_how_many_it_withheld() {
        let fake = Arc::new(label_routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::tty(80));
        let args = ListArgs { scope: ScopeArgs { org: None }, limit: Some(1), include_org: false };
        list(&cx, &globals, &LabelScope::Repo(slug()), &args).await.unwrap();

        let out = text(&buf);
        assert!(
            out.starts_with("Showing 1 of 2 labels in perf3ct/gea\n"),
            "the banner must name the total it did not print: {out}"
        );
    }

    /// ...and the complete list still does not invent an "of N", nor pluralise a single label.
    #[tokio::test]
    async fn a_complete_list_states_only_what_it_printed() {
        for (cap, expected) in [
            (2usize, "Showing 2 labels in perf3ct/gea\n"),
            (30, "Showing 2 labels in perf3ct/gea\n"),
        ] {
            let fake = Arc::new(label_routes());
            let buf = Arc::new(Mutex::new(Vec::new()));
            let globals = GlobalOpts::default();
            let cx = cx(fake, &buf, &globals, Term::tty(80));
            let args =
                ListArgs { scope: ScopeArgs { org: None }, limit: Some(cap), include_org: false };
            list(&cx, &globals, &LabelScope::Repo(slug()), &args).await.unwrap();
            assert!(text(&buf).starts_with(expected), "-L {cap}: {}", text(&buf));
        }
    }

    /// Bug this prevents: `--include-org` reported as a failure when the repository's owner is a
    /// user. `/orgs/{user}/labels` 404s for one, and `--include-org` means "as well", not
    /// "instead" — so the two walks run concurrently under `join!` and the organization arm is
    /// allowed to fail. `try_join!` would abandon the repository's labels the moment the
    /// tolerated 404 arrived, turning a working command into a hard error.
    #[tokio::test]
    async fn include_org_survives_an_owner_that_is_not_an_organization() {
        let fake = Arc::new(label_routes().on(
            method!("GET"),
            "/api/v1/orgs/perf3ct/labels",
            Canned::json(404, r#"{"message":"user redirect does not exist"}"#),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::tty(80));
        let args = ListArgs { scope: ScopeArgs { org: None }, limit: None, include_org: true };

        list(&cx, &globals, &LabelScope::Repo(slug()), &args).await.unwrap();

        let out = text(&buf);
        assert!(out.starts_with("Showing 2 labels in perf3ct/gea\n"), "{out}");
        assert!(out.contains("SCOPE"), "both scopes were asked for, so the column is shown: {out}");
        assert!(out.contains("repo"), "the repository's labels must survive the org 404: {out}");
        // Matched on path, not position: the two walks race, and an index would be flaky.
        assert!(
            fake.calls().iter().any(|c| c.path == "/api/v1/orgs/perf3ct/labels"),
            "the org scope was never walked: {:?}",
            fake.calls()
        );
    }

    #[tokio::test]
    async fn list_json_is_the_api_field_names() {
        let fake = Arc::new(label_routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts { json: Some("name,color".to_owned()), ..GlobalOpts::default() };
        let cx = cx(fake, &buf, &globals, Term::piped());
        let args = ListArgs { scope: ScopeArgs { org: None }, limit: None, include_org: false };
        list(&cx, &globals, &LabelScope::Repo(slug()), &args).await.unwrap();
        insta::assert_snapshot!("list_json", text(&buf));
    }

    /// Bug this prevents: an org-scoped subcommand addressing the repository route. The two
    /// have different ids, so `gea label edit --org acme bug -c fff` against
    /// `/repos/...` either 404s or edits an unrelated repository label with the same id.
    #[tokio::test]
    async fn org_scope_uses_the_org_route() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/orgs/acme/labels",
                    Vec::from([Canned::json(200, LABELS), Canned::json(200, "[]")]),
                )
                .on(
                    method!("PATCH"),
                    "/api/v1/orgs/acme/labels/4",
                    Canned::json(200, r#"{"id":4,"name":"defect","color":"e11d21"}"#),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let args = EditArgs {
            name: "bug".to_owned(),
            scope: ScopeArgs { org: Some("acme".to_owned()) },
            new_name: Some("defect".to_owned()),
            color: None,
            description: None,
            exclusive: None,
            archived: None,
        };
        edit(&cx, &LabelScope::Org("acme".to_owned()), &args).await.unwrap();

        let patch = fake.calls_to(&method!("PATCH"), "/api/v1/orgs/acme/labels/4");
        assert_eq!(patch.len(), 1);
        // Only the name: a full `EditLabelOption` would also send `color:""`, which Gitea
        // rejects, and `exclusive:false`, which would un-exclusive the label.
        assert_eq!(patch[0].body_str(), r#"{"name":"defect"}"#);
        assert!(
            fake.calls().iter().all(|c| !c.path.contains("/repos/")),
            "an --org command must not touch the repository route: {:?}",
            fake.calls()
        );
    }

    /// Bug this prevents: `clone` overwriting labels that already exist in the target. A
    /// colour and description that somebody chose deliberately would be replaced by whichever
    /// repository they happened to copy from.
    #[tokio::test]
    async fn clone_skips_existing_labels_unless_told_otherwise() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/gitea/gitea/labels",
                    Vec::from([Canned::json(200, LABELS), Canned::json(200, "[]")]),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/labels",
                    Vec::from([
                        Canned::json(200, r#"[{"id":9,"name":"bug","color":"000000"}]"#),
                        Canned::json(200, "[]"),
                    ]),
                )
                .on(
                    method!("POST"),
                    "/api/v1/repos/perf3ct/gea/labels",
                    Canned::json(201, r#"{"id":10,"name":"ci","color":"1d76db"}"#),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let args = CloneArgs {
            source: "gitea/gitea".parse().unwrap(),
            scope: ScopeArgs { org: None },
            overwrite: false,
        };
        clone_from(&cx, &LabelScope::Repo(slug()), &args).await.unwrap();

        let posts = fake.calls_to(&method!("POST"), "/api/v1/repos/perf3ct/gea/labels");
        assert_eq!(posts.len(), 1, "only the label that did not already exist");
        assert!(posts[0].body_str().contains(r#""name":"ci""#), "{}", posts[0].body_str());
        assert!(
            fake.calls().iter().all(|c| c.method.as_str() != "PATCH"),
            "nothing is overwritten without --overwrite"
        );
        insta::assert_snapshot!("clone_human", text(&buf));
    }

    #[test]
    fn colours_are_accepted_with_or_without_a_hash_and_in_three_digits() {
        assert_eq!(normalise_color("E11D21"), "#e11d21");
        assert_eq!(normalise_color("#e11d21"), "#e11d21");
        assert_eq!(normalise_color(""), "");
        assert!(validate_color("#e11d21").is_ok());
        assert!(validate_color("fff").is_ok());
        // A named colour is the mistake people actually make, and the message has to name the
        // format rather than repeat the server's "color is not valid".
        let e = validate_color("red").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("RRGGBB"), "{e}");
    }

    #[tokio::test]
    async fn edit_with_no_flags_is_a_usage_error_before_any_request() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let args = EditArgs {
            name: "bug".to_owned(),
            scope: ScopeArgs { org: None },
            new_name: None,
            color: None,
            description: None,
            exclusive: None,
            archived: None,
        };
        assert_eq!(edit(&cx, &LabelScope::Repo(slug()), &args).await.unwrap_err().exit_code(), 2);
        assert_eq!(fake.call_count(), 0);
    }

    #[tokio::test]
    async fn a_missing_label_names_the_ones_that_exist() {
        let fake = Arc::new(label_routes());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::piped());
        let e = by_name(&cx, &LabelScope::Repo(slug()), "nope").await.unwrap_err();
        assert!(e.to_string().contains("bug, ci"), "{e}");
    }
}

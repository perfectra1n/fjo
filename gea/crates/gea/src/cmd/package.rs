//! `gea package` — the package registry.
//!
//! `gh` has no package commands and neither does `tea`, so there is no precedent to copy. Two
//! properties of Gitea's registry shape everything below, and both are things users get wrong.
//!
//! # A package belongs to an owner, not to a repository
//!
//! The route is `/packages/{owner}/{type}/{name}/{version}`. There is no repository in it. A
//! package may be *linked* to one — `Package.repository` — and the web UI shows it on that
//! repository's page, which is where the misconception comes from, but the link is metadata. Two
//! consequences:
//!
//! * deleting the repository does not delete its packages, and they keep counting against the
//!   owner's quota (see [`crate::cmd::quota`]);
//! * `gea package list` has to decide *whose* packages to list. It infers the owner from the
//!   repository when there is one, because a checkout of `acme/thing` almost always means "the
//!   packages `acme` publishes" — and says which owner it used, so the inference is never silent.
//!
//! # Packages are immutable
//!
//! There is no PATCH. A published version cannot be edited — not its files, not its metadata, not
//! its description. The only way to change one is to delete it and publish again, usually under a
//! new version, because most registries also refuse to accept a version number they have already
//! seen.
//!
//! So this group deliberately has **no `edit` subcommand**. Offering one that could only ever
//! fail would be worse than not having it: the user would spend the round trip and then read a
//! server error instead of the explanation, which is in `--help` where they will look first.
//!
//! Publishing is likewise absent, and is *not* an oversight: each of the 23 registry types has its
//! own upload protocol (`cargo publish`, `npm publish`, `docker push`, …) and Gitea's REST API
//! deliberately does not abstract over them. `gea` has nothing better to offer than the native
//! tool, so it says so rather than wrapping one of them badly.

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::Package;

use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const PACKAGE_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_PACKAGE);
const FILE_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_PACKAGE_FILE);

/// The registry types Gitea 16 implements, for `--type`'s help and for a typo warning.
///
/// Not used to reject a value: the list grows with each release and refusing a type a newer
/// instance supports would make `gea` the reason a working registry is unreachable.
const TYPES: &[&str] = &[
    "alpine",
    "arch",
    "cargo",
    "chef",
    "composer",
    "conan",
    "conda",
    "container",
    "cran",
    "debian",
    "generic",
    "go",
    "helm",
    "maven",
    "npm",
    "nuget",
    "pub",
    "pypi",
    "rpm",
    "rubygems",
    "swift",
    "vagrant",
];

const LONG_ABOUT: &str = "\
Manage package versions.

Packages belong to a user or organization, not to a repository. Linking a package
to a repository does not change its owner. Deleting the linked repository does not
delete the package.

Published packages cannot be edited. Publish new versions with the registry's own
tools, such as cargo publish, npm publish, or docker push.

The owner defaults to the current repository's owner, or your account outside a checkout.

  gea package list
  gea package list acme --type container
  gea package list --linked            # only packages linked to this repository
  gea package view mycrate 1.2.3
  gea package files mycrate 1.2.3
  gea package delete mycrate 1.2.3";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List an owner's packages
    List(ListArgs),
    /// Show one package version
    View(SelectArgs),
    /// A package version's files
    Files(SelectArgs),
    /// Delete a package version
    Delete(DeleteArgs),
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List packages belonging to a user or organization.

The owner defaults to the current repository's owner, or your account outside a
checkout. The selected owner is reported on stderr.

  gea package list
  gea package list acme
  gea package list --type container
  gea package list --name mycrate
  gea package list --linked")]
pub struct ListArgs {
    /// User or organization; inferred from the repository, or you, when omitted
    #[arg(value_name = "OWNER")]
    pub owner: Option<String>,

    /// Only this registry type: cargo, npm, container, generic, …
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Only packages whose name contains this
    #[arg(long, value_name = "TEXT")]
    pub name: Option<String>,

    /// Only packages linked to the repository you are in
    #[arg(long)]
    pub linked: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a package version.

The type and version are selected automatically when only one matches.
If several match, specify one from the listed candidates.")]
pub struct SelectArgs {
    /// Package name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Version; inferred when the package has only one
    #[arg(value_name = "VERSION")]
    pub version: Option<String>,

    /// User or organization; inferred as for `list`
    #[arg(long, value_name = "OWNER")]
    pub owner: Option<String>,

    /// Registry type; inferred when unambiguous
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Delete a package version. This cannot be undone.

Some registries refuse to reuse deleted version numbers. Check before deleting
if you intend to publish a replacement. Deletion reduces the owner's storage usage.")]
pub struct DeleteArgs {
    /// Package name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Version; inferred when the package has only one
    #[arg(value_name = "VERSION")]
    pub version: Option<String>,

    /// User or organization; inferred as for `list`
    #[arg(long, value_name = "OWNER")]
    pub owner: Option<String>,

    /// Registry type; inferred when unambiguous
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            Self::List(_) | Self::View(_) => Some(PACKAGE_FIELDS),
            Self::Files(_) => Some(FILE_FIELDS),
            Self::Delete(_) => None,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if let Some(fields) = args.command.fields()
        && porcelain::discovery(globals, fields)?
    {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, &api, globals, a).await,
            Cmd::View(a) => view(&rt, &api, globals, a).await,
            Cmd::Files(a) => files(&rt, &api, globals, a).await,
            Cmd::Delete(a) => delete(&rt, &api, globals, a).await,
        }
    })
}

// -------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &ListArgs) -> Result<()> {
    let owner = owner_for(rt, api, globals, args.owner.as_deref()).await?;
    if let Some(t) = &args.r#type {
        warn_unknown_type(rt, t);
    }
    let mut packages = fetch(api, globals, &owner, args.r#type.as_deref(), args.name.as_deref())
        .await
        .map_err(|e| explain(e, &owner))?;

    if args.linked {
        // The API has no "linked to this repository" filter, so it is applied here. Named
        // `--linked` rather than `--repo` because `-R/--repo` is a global with a different job.
        let slug = rt.repo(globals)?.slug.clone();
        let full = slug.to_string();
        packages.retain(|p| {
            p.repository.as_ref().is_some_and(|r| r.full_name.eq_ignore_ascii_case(&full))
        });
    }

    let machine = Machine::compile(globals, PACKAGE_FIELDS)?;
    if packages.is_empty() {
        return porcelain::empty(globals, rt.term(), machine.as_ref(), &nothing(&owner, args));
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&packages)?);
    }

    porcelain::print(globals, &render_list(rt.term(), &packages))
}

/// The `list` table.
pub(crate) fn render_list(term: &Term, packages: &[Package]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["NAME", "VERSION", "TYPE", "LINKED REPO", "CREATED"]);
    for p in packages {
        t.row([
            p.name.clone(),
            porcelain::dash(&p.version),
            porcelain::dash(&p.r#type),
            porcelain::dash(
                &p.repository.as_ref().map(|r| r.full_name.clone()).unwrap_or_default(),
            ),
            porcelain::when(p.created_at),
        ]);
    }
    porcelain::rendered_table(term, t, "packages", None)
}

fn nothing(owner: &str, args: &ListArgs) -> String {
    let mut filters: Vec<String> = Vec::new();
    if let Some(t) = &args.r#type {
        filters.push(format!("type {t}"));
    }
    if let Some(n) = &args.name {
        filters.push(format!("name containing {n:?}"));
    }
    if args.linked {
        filters.push("linked to this repository".to_owned());
    }
    let suffix = if filters.is_empty() {
        String::new()
    } else {
        format!(" matching {}", filters.join(" and "))
    };
    format!(
        "{owner} has no packages{suffix}. Publish with the registry tool, such as cargo publish, npm publish, or docker push."
    )
}

// ---------------------------------------------------------------------------- view / files

async fn view(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &SelectArgs) -> Result<()> {
    let owner = owner_for(rt, api, globals, args.owner.as_deref()).await?;
    let found =
        resolve(api, globals, &owner, &args.name, args.version.as_deref(), args.r#type.as_deref())
            .await?;

    if let Some(m) = Machine::compile(globals, PACKAGE_FIELDS)? {
        return m.write(globals, rt.term(), porcelain::json_of(&found)?);
    }

    porcelain::print(globals, &render_view(rt.term(), &found))
}

/// The `view` detail.
///
/// The immutability note is printed every time rather than only on an attempted change, because
/// "how do I edit this" is the next question and the answer — you cannot — is not the one people
/// expect from a registry.
pub(crate) fn render_view(term: &Term, p: &Package) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let mut o = String::new();
    let repo = p.repository.as_ref().map(|r| r.full_name.as_str()).unwrap_or("");
    if !term.tty {
        let _ = writeln!(o, "{}\t{}\t{}\t{}\t{}", p.name, p.version, p.r#type, repo, p.html_url);
        return o;
    }
    let _ = writeln!(o, "{} {}", p.name, p.version);
    let _ = writeln!(o);
    let _ = writeln!(o, "type      {}", porcelain::dash(&p.r#type));
    let _ = writeln!(
        o,
        "owner     {}",
        porcelain::dash(p.owner.as_ref().map(|u| u.login.as_str()).unwrap_or(""))
    );
    let _ = writeln!(
        o,
        "creator   {}",
        porcelain::dash(p.creator.as_ref().map(|u| u.login.as_str()).unwrap_or(""))
    );
    let _ = writeln!(o, "linked to {}", porcelain::dash(repo));
    let _ = writeln!(o, "created   {}", porcelain::when(p.created_at));
    let _ = writeln!(o, "url       {}", porcelain::dash(&p.html_url));
    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "This version cannot be edited. Publish a new version to make changes.\n\
         List its files with `gea package files {} {}`.",
        shell_word(&p.name),
        shell_word(&p.version)
    );
    o
}

async fn files(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &SelectArgs) -> Result<()> {
    let owner = owner_for(rt, api, globals, args.owner.as_deref()).await?;
    let found =
        resolve(api, globals, &owner, &args.name, args.version.as_deref(), args.r#type.as_deref())
            .await?;
    let files = api
        .package()
        .list_package_files(&owner, &found.r#type, &found.name, &found.version)
        .await
        .map_err(|e| explain(e, &owner))?;

    let machine = Machine::compile(globals, FILE_FIELDS)?;
    if files.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            &format!("{} {} has no files recorded", found.name, found.version),
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&files)?);
    }

    porcelain::print(globals, &render_files(rt.term(), &files))?;
    let total: i64 = files.iter().map(|f| f.size).sum();
    porcelain::note(
        rt.term(),
        &format!(
            "{} of quota usage. Check `gea quota status`.",
            crate::cmd::support::size::human(total)
        ),
    );
    Ok(())
}

/// The `files` table.
pub(crate) fn render_files(term: &Term, files: &[gitea_model::PackageFile]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["NAME", "SIZE", "SHA256"]);
    for f in files {
        t.row([
            f.name.clone(),
            crate::cmd::support::size::human(f.size),
            // Twelve characters: enough to recognise, short enough not to eat the terminal. The
            // full digest is one `--json sha256` away.
            f.sha256.chars().take(12).collect::<String>(),
        ]);
    }
    porcelain::rendered_table(term, t, "files", None)
}

// ------------------------------------------------------------------------------------ delete

async fn delete(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &DeleteArgs) -> Result<()> {
    let owner = owner_for(rt, api, globals, args.owner.as_deref()).await?;
    let found =
        resolve(api, globals, &owner, &args.name, args.version.as_deref(), args.r#type.as_deref())
            .await?;
    porcelain::confirm(
        rt,
        &format!(
            "Delete {} {} ({}) from {owner}? Most registries will refuse to accept that version \
             number again.",
            found.name, found.version, found.r#type
        ),
        args.yes,
    )?;
    api.package()
        .delete_package_version(&owner, &found.r#type, &found.name, &found.version)
        .await
        .map_err(|e| explain(e, &owner))?;
    porcelain::note(rt.term(), &format!("Deleted {} {} from {owner}", found.name, found.version));
    Ok(())
}

// ------------------------------------------------------------------------------------ shared

/// Whose packages to act on.
///
/// Explicit wins; then the repository's owner, because a checkout of `acme/thing` almost always
/// means "what `acme` publishes"; then the authenticated user, so the command still works outside
/// a checkout. Which one was used goes to stderr on a terminal — an inference nobody can see is
/// indistinguishable from a bug.
async fn owner_for(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(o) = explicit {
        return porcelain::resolve_user(rt, o).await;
    }
    if let Ok(ctx) = rt.repo(globals) {
        let owner = ctx.slug.owner.clone();
        porcelain::note(
            rt.term(),
            &format!(
                "Showing packages for {owner} (owner of {}). Pass an owner to select another.",
                ctx.slug
            ),
        );
        return Ok(owner);
    }
    let me = api.user().get_current().await?.login;
    porcelain::note(
        rt.term(),
        &format!("Showing packages for {me}. Pass an owner to select another."),
    );
    Ok(me)
}

async fn fetch(
    api: &Api,
    globals: &GlobalOpts,
    owner: &str,
    r#type: Option<&str>,
    name: Option<&str>,
) -> Result<Vec<Package>> {
    let query = gitea_client::query::ListPackagesQuery {
        r#type: r#type.map(str::to_owned),
        q: name.map(str::to_owned),
        ..Default::default()
    };
    let take = porcelain::item_limit(globals).unwrap_or(usize::MAX);
    let mut stream = api.package().list_packages(owner, &query).take(take);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item?);
    }
    Ok(out)
}

/// Find the one package version the user meant.
///
/// The API's path needs `{type}` and `{version}` as well as the name, and a user reading
/// `gea package list` knows the name. So an omitted type or version is resolved by listing and
/// insisting the answer is unique — never by picking the first, which would silently delete the
/// wrong version the one time it mattered.
async fn resolve(
    api: &Api,
    globals: &GlobalOpts,
    owner: &str,
    name: &str,
    version: Option<&str>,
    r#type: Option<&str>,
) -> Result<Package> {
    // Fully specified: one request, no listing. This is the scripted path and it must not pay for
    // the convenience the interactive path wants.
    if let (Some(v), Some(t)) = (version, r#type) {
        return api
            .package()
            .get_package(owner, t, name, v)
            .await
            .map_err(|e| explain_missing(e, owner, name, Some(v), Some(t)));
    }

    let candidates: Vec<Package> = fetch(api, globals, owner, r#type, Some(name))
        .await
        .map_err(|e| explain(e, owner))?
        .into_iter()
        // `q` is a substring match, so `mycrate` also returns `mycrate-macros`; the exact name is
        // what was asked for.
        .filter(|p| p.name == name)
        .filter(|p| version.is_none_or(|v| p.version == v))
        .collect();

    match candidates.len() {
        0 => Err(explain_missing(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "package",
                id: name.to_owned(),
                slug: None,
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            owner,
            name,
            version,
            r#type,
        )),
        1 => Ok(candidates.into_iter().next().expect("length checked")),
        _ => Err(ambiguous(name, &candidates)),
    }
}

/// The refusal when a name matches several versions or types.
///
/// Lists them, because the user's next command is one of these lines and retyping the whole
/// invocation from `--help` is friction they do not need.
fn ambiguous(name: &str, candidates: &[Package]) -> Error {
    let mut lines = String::new();
    for p in candidates.iter().take(20) {
        lines.push_str(&format!(
            "\n  gea package view {} {} --type {}",
            shell_word(&p.name),
            shell_word(&p.version),
            p.r#type
        ));
    }
    if candidates.len() > 20 {
        lines.push_str(&format!("\n  … and {} more", candidates.len() - 20));
    }
    Error::new(ErrorKind::Usage(format!(
        "{name:?} matches {} package versions, so it is not clear which one you mean. Name the \
         version (and --type if the name exists in more than one registry):{lines}",
        candidates.len()
    )))
}

fn explain_missing(
    e: Error,
    owner: &str,
    name: &str,
    version: Option<&str>,
    r#type: Option<&str>,
) -> Error {
    match &*e.kind {
        ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. } => {
            let what = match (version, r#type) {
                (Some(v), Some(t)) => format!("{name} {v} ({t})"),
                (Some(v), None) => format!("{name} {v}"),
                _ => name.to_owned(),
            };
            Error::new(ErrorKind::Usage(format!(
                "{owner} has no package {what}. Check the user or organization name, then run `gea package list {owner}`."
            )))
        }
        _ => e,
    }
}

fn explain(e: Error, owner: &str) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } => Error::new(ErrorKind::Usage(
            "this instance did not answer the package endpoint; the registry can be switched off \
             with [packages] ENABLED = false in app.ini. `gea nodeinfo` shows what the instance \
             is running."
                .to_owned(),
        )),
        ErrorKind::ResourceNotFound { .. } => Error::new(ErrorKind::Usage(format!(
            "no such owner {owner:?}, or they have no package registry. Packages belong to a user \
             or an organization, never to a repository."
        ))),
        _ => e,
    }
}

/// Warn about a `--type` this build does not know, without refusing it.
fn warn_unknown_type(rt: &Runtime, given: &str) {
    let t = given.trim().to_ascii_lowercase();
    if !TYPES.contains(&t.as_str()) {
        porcelain::note(
            rt.term(),
            &format!(
                "note: {given:?} is not a registry type this build knows; sending it anyway. \
                 Known: {}.",
                TYPES.join(", ")
            ),
        );
    }
}

fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '@'))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use clap::{CommandFactory, Parser};
    use gitea_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    #[derive(Parser)]
    struct Harness {
        #[command(subcommand)]
        _cmd: Cmd,
    }

    /// Bug this prevents — and the property the task asks for: an `edit` subcommand existing.
    /// Packages are immutable, so an `edit` could only ever spend a round trip and return a
    /// server error; the explanation belongs in `--help`, where someone looks first.
    #[test]
    fn there_is_no_edit_subcommand_and_the_help_says_why() {
        let cmd = Harness::command();
        let names: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        assert_eq!(names, vec!["list", "view", "files", "delete"]);
        for forbidden in ["edit", "update", "publish", "upload", "create"] {
            assert!(!names.contains(&forbidden), "`package {forbidden}` must not exist: {names:?}");
        }
        // And the group's own help has to explain it, or the absence is just a gap.
        assert!(LONG_ABOUT.contains("cannot be edited"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("Publish new versions"), "{LONG_ABOUT}");
        // Publishing is absent for a different reason, and that one is stated too.
        assert!(LONG_ABOUT.contains("cargo publish"), "{LONG_ABOUT}");
    }

    /// The other misconception, asserted on the help text so it cannot be edited away silently:
    /// packages belong to an owner, not a repository.
    #[test]
    fn the_help_states_that_packages_belong_to_an_owner_not_a_repository() {
        assert!(LONG_ABOUT.contains("not to a repository"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("linked"), "{LONG_ABOUT}");
    }

    const PACKAGES: &str = r#"[
      {"id":1,"name":"mycrate","version":"1.2.3","type":"cargo","created_at":null,
       "html_url":"https://git.example.org/acme/-/packages/cargo/mycrate/1.2.3",
       "repository":{"full_name":"acme/thing","name":"thing","id":9,"owner":{"login":"acme"}}},
      {"id":2,"name":"web","version":"sha-abc","type":"container","created_at":null,
       "html_url":"https://git.example.org/acme/-/packages/container/web/sha-abc",
       "repository":null}
    ]"#;

    fn packages() -> Vec<Package> {
        serde_json::from_str(PACKAGES).expect("the fixture is valid Package JSON")
    }

    /// Bug this prevents: `--type` and `--name` reaching the wire under the wrong parameter names.
    /// The name filter is `q`, not `name`, and a request that sends `name=` is silently unfiltered
    /// — so `gea package list --name mycrate` would list everything and look like it worked.
    #[tokio::test]
    async fn the_filters_reach_the_wire_as_type_and_q() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/packages/acme",
            testing::one_page(PACKAGES),
        ));
        let api = testing::api(fake.clone());
        let out = fetch(&api, &GlobalOpts::default(), "acme", Some("cargo"), Some("mycrate"))
            .await
            .unwrap();
        assert_eq!(out.len(), 2, "the fake answers with the whole fixture");
        let call = &fake.calls_to(&testing::method("GET"), "/api/v1/packages/acme")[0];
        assert_eq!(call.query_param("type"), Some("cargo"));
        assert_eq!(call.query_param("q"), Some("mycrate"));
        // Owner-scoped, with no repository anywhere in the path — the group's central point.
        assert!(!call.path.contains("/repos/"), "{}", call.path);
    }

    /// Bug this prevents: `q` being a substring match, so `gea package view mycrate` would find
    /// `mycrate-macros` too and either show the wrong package or refuse as ambiguous when it is
    /// not. The exact name is what was asked for.
    #[tokio::test]
    async fn a_substring_match_from_the_server_is_narrowed_to_the_exact_name() {
        let body = r#"[
          {"id":1,"name":"mycrate","version":"1.2.3","type":"cargo","created_at":null},
          {"id":2,"name":"mycrate-macros","version":"1.2.3","type":"cargo","created_at":null}
        ]"#;
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/packages/acme",
            testing::one_page(body),
        ));
        let api = testing::api(fake);
        let found =
            resolve(&api, &GlobalOpts::default(), "acme", "mycrate", None, None).await.unwrap();
        assert_eq!(found.name, "mycrate");
        assert_eq!(found.version, "1.2.3");
    }

    /// Fully specified: one request to the versioned path, and no listing at all. This is the
    /// scripted path and it must not pay for the interactive path's convenience.
    #[tokio::test]
    async fn a_fully_named_package_is_fetched_directly() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/packages/acme/cargo/mycrate/1.2.3",
            Canned::json(200, r#"{"id":1,"name":"mycrate","version":"1.2.3","type":"cargo"}"#),
        ));
        let api = testing::api(fake.clone());
        let found =
            resolve(&api, &GlobalOpts::default(), "acme", "mycrate", Some("1.2.3"), Some("cargo"))
                .await
                .unwrap();
        assert_eq!(found.version, "1.2.3");
        assert_eq!(fake.call_count(), 1, "no listing request should have been made");
    }

    #[test]
    fn the_list_table_renders_the_same_data_two_ways() {
        insta::assert_snapshot!("package_list_human", render_list(&testing::term(), &packages()));
        insta::assert_snapshot!("package_list_piped", render_list(&Term::piped(), &packages()));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        insta::assert_snapshot!(
            "package_list_json",
            testing::as_json(
                PACKAGE_FIELDS,
                "id,name,version,type,html_url",
                porcelain::json_of(&packages()).unwrap()
            )
        );
    }

    /// The detail view, which says out loud that the version cannot be edited.
    #[test]
    fn the_view_states_immutability_every_time() {
        let out = render_view(&testing::term(), &packages()[0]);
        assert!(out.contains("cannot be edited"), "{out}");
        insta::assert_snapshot!("package_view_human", out);
    }

    fn pkg(name: &str, version: &str, r#type: &str) -> Package {
        Package {
            name: name.to_owned(),
            version: version.to_owned(),
            r#type: r#type.to_owned(),
            ..Default::default()
        }
    }

    /// Bug this prevents: picking the first match when a name is ambiguous. On `view` that shows
    /// the wrong package; on `delete` it destroys the wrong one, unrecoverably.
    #[test]
    fn an_ambiguous_name_refuses_and_lists_the_candidates() {
        let e = ambiguous(
            "mycrate",
            &[pkg("mycrate", "1.2.3", "cargo"), pkg("mycrate", "1.3.0", "cargo")],
        );
        assert_eq!(e.exit_code(), 2);
        let msg = e.to_string();
        assert!(msg.contains("1.2.3"), "{msg}");
        assert!(msg.contains("1.3.0"), "{msg}");
        assert!(msg.contains("--type cargo"), "{msg}");
    }

    #[test]
    fn a_missing_package_says_where_packages_live() {
        let e = explain_missing(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "package",
                id: "x".to_owned(),
                slug: None,
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            "acme",
            "mycrate",
            Some("1.0.0"),
            Some("cargo"),
        );
        let msg = e.to_string();
        assert!(msg.contains("gea package list acme"), "{msg}");
        assert!(msg.contains("user or organization"), "{msg}");
    }

    #[test]
    fn a_version_with_shell_metacharacters_is_quoted_in_the_hint() {
        assert_eq!(shell_word("1.2.3"), "1.2.3");
        assert_eq!(shell_word("v1.0.0-rc.1+build"), "v1.0.0-rc.1+build");
        assert_eq!(shell_word("1.0 beta"), "'1.0 beta'");
        assert_eq!(shell_word(""), "''");
    }

    /// The 22 types in Gitea 16 (plus `arch`, added in 12), so `--type` help and the typo
    /// warning stay truthful. `container` is the one people reach for first and the one most
    /// often typed as `docker`.
    #[test]
    fn the_known_types_cover_what_gitea_implements() {
        assert!(TYPES.contains(&"container"));
        assert!(TYPES.contains(&"cargo"));
        assert!(TYPES.contains(&"generic"));
        assert!(!TYPES.contains(&"docker"), "the registry type is `container`, not `docker`");
        // Sorted, so the warning's list reads predictably.
        let mut sorted = TYPES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, TYPES);
    }
}

//! `gea nodeinfo` — what kind of instance this is, **before you log in**.
//!
//! # The gap this fills
//!
//! Every other command in `gea` assumes a configured host and a token. This one assumes neither,
//! because the question it answers comes *first*: someone has a URL and wants to know whether it
//! is Gitea, which version, and what the API will let them ask for. All of that is public on a
//! default instance: `/version` names the server's version and `/settings/api` its limits.
//!
//! The name is historical — Gitea's API specification has no NodeInfo document (Gitea serves one
//! only with federation switched on), so the command reads `/version` instead, which every Gitea
//! answers.
//!
//! # Telling Gitea from Forgejo
//!
//! Forgejo is a Gitea fork and answers the same `/version` route, but with its own version and a
//! `+gitea-<ver>` suffix naming the Gitea API it is compatible with (`11.0.1+gitea-1.22.0`). That
//! suffix is what [`Flavour::detect`] keys on: `gea` is generated from Gitea's specification, so
//! a Forgejo server works for most commands and not for the ones Forgejo has dropped or renamed,
//! and saying that once, here, is why no other command has to probe.
//!
//! # Why it can bypass [`crate::runtime::Runtime`]
//!
//! `Runtime::new` resolves a host out of `hosts.toml`, and `Hosts::adopt_env_host` only adopts a
//! `--host` that has *no* entry when a token is also in the environment — which is right for every
//! other command, and exactly wrong for this one. So when host resolution fails and `--host` was
//! given, this command builds a bare unauthenticated [`Client`] against that URL. That is what
//! makes
//!
//! ```text
//! gea --host gitea.com nodeinfo
//! ```
//!
//! work on a machine with no configuration at all, which is the whole point.
//!
//! # Why `/settings/api` failing is not fatal
//!
//! An instance can require authentication for `/settings/api`, or sit behind a proxy that blocks
//! it. The `/version` half still answers "is this Gitea and which version", which is most of the
//! value, so a failure there is a note on stderr rather than a non-zero exit.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_core::config::HostEntry;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::{Auth, Client};
use gitea_model::{GeneralApiSettings, ServerVersion};

use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;

const VERSION_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_SERVER_VERSION);
const SETTINGS_FIELDS: Fields =
    Fields::Generated(gitea_client::fields::FIELDS_GENERAL_API_SETTINGS);

const LONG_ABOUT: &str = "\
Show the server's software, version, and API limits.

No authentication is required. Use --host to query an unconfigured server.
`nodeinfo` combines /version and /settings/api; `nodeinfo limits` shows API limits,
including max_response_items (the maximum page size).

  gea nodeinfo
  gea nodeinfo --json version --jq .version
  gea nodeinfo limits
  gea --host gitea.com nodeinfo";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Only the API limits, from /settings/api
    Limits,
}

impl Args {
    fn fields(&self) -> Fields {
        match self.command {
            Some(Cmd::Limits) => SETTINGS_FIELDS,
            None => VERSION_FIELDS,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if porcelain::discovery(globals, args.fields())? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let (client, term, host) = connect(globals)?;
        let api = Api::new(client);
        match args.command {
            Some(Cmd::Limits) => limits(&api, globals, &term).await,
            None => overview(&api, globals, &term, &host).await,
        }
    })
}

/// A client, a terminal, and the host's label — without insisting on a configured host.
///
/// Prefers the full [`crate::runtime::Runtime`], so a configured host keeps its retry policy, its
/// `hosts.toml` URL (including a subpath install), and any token that happens to be present — a
/// token is not *needed* here, but an instance with `REQUIRE_SIGNIN_VIEW` will refuse without one,
/// and silently dropping a credential the user has would break that case.
///
/// Falls back to a bare unauthenticated client when resolution failed *and* `--host` named
/// something, which is the fresh-install case this command exists for.
fn connect(globals: &GlobalOpts) -> Result<(Client, Term, String)> {
    match crate::runtime::Runtime::new(globals) {
        Ok(rt) => {
            let host = rt.host().to_string();
            Ok((rt.client().clone(), *rt.term(), host))
        }
        Err(e) => {
            let unconfigured =
                matches!(&*e.kind, ErrorKind::NoHostConfigured | ErrorKind::UnknownHost { .. });
            let Some(given) = globals.host.as_deref().filter(|_| unconfigured) else {
                return Err(e);
            };
            // `HostEntry::from_input` owns the "did they mean http or https" decision, and it has
            // a test table behind it. Re-deriving the scheme here would be a second, worse copy.
            let entry = HostEntry::from_input(given)?;
            let client = Client::builder(&entry.url, Auth::None)
                .user_agent(crate::runtime::user_agent())
                .build()?;
            Ok((client, Term::detect(), entry.name.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------------- overview

async fn overview(api: &Api, globals: &GlobalOpts, term: &Term, host: &str) -> Result<()> {
    // Compiled before anything is sent, which `cmd::support::machine` states is the point rather
    // than an optimisation: `gea nodeinfo --jq '.bad['` is a typo in an argument, and it must be
    // reported as one instead of after a round trip to a server that was never at fault.
    let machine = Machine::compile(globals, VERSION_FIELDS)?;

    // The machine view is decided *before* any request, so it still costs exactly one: the
    // `/settings/api` half exists to fill in the human table, and `--json`/`--jq` select out of
    // the `/version` document alone.
    if let Some(m) = machine {
        let version = api.misc().get_version().await.map_err(explain)?;
        return m.write(globals, term, porcelain::json_of(&version)?);
    }

    // Two independent documents, so two overlapped reads rather than two round trips in a row.
    //
    // `join!`, emphatically not `try_join!`: `/settings/api` is *allowed* to fail here — see the
    // module docs — and `try_join!` would abandon the `/version` half the moment a guarded
    // instance answered 401, turning the note below into a non-zero exit. Unwrapping in a fixed
    // order afterwards also keeps `/version`'s error the one the user sees, whichever request
    // happened to finish first.
    //
    // `misc`/`settings` are hoisted into `let`s because each borrows `api` and the future outlives
    // the temporary an inline call would produce.
    let (misc, settings_api) = (api.misc(), api.settings());
    let (version, settings) =
        futures::join!(misc.get_version(), settings_api.get_general_api_settings());
    let version = version.map_err(explain)?;

    // Best-effort: the `/version` half already answers "is this Gitea", so an instance that
    // guards `/settings/api` still gets a useful answer.
    let settings = match settings {
        Ok(s) => Some(s),
        Err(e) => {
            porcelain::note(
                term,
                &format!(
                    "note: {host} did not answer /settings/api ({}), so the API limits below are \
                     unknown. Some instances require a token for it.",
                    headline(&e)
                ),
            );
            None
        }
    };
    porcelain::print(globals, &render_overview(term, host, &version, settings.as_ref()))
}

/// The overview: what the instance is and the limits it enforces.
///
/// Two documents in one view, which is the orchestration that earns this a porcelain command:
/// `/version` answers "is this Gitea and which version" and `/settings/api` answers "what will
/// it let me ask for", and nobody wants those separately.
pub(crate) fn render_overview(
    term: &Term,
    host: &str,
    version: &ServerVersion,
    settings: Option<&GeneralApiSettings>,
) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let flavour = Flavour::detect(&version.version);
    let mut o = String::new();

    if !term.tty {
        // One TAB-separated line: `software<TAB>version<TAB>max_response_items`. Chosen so
        // `gea nodeinfo | cut -f2` is the version check a script wants.
        let _ = writeln!(
            o,
            "{}\t{}\t{}",
            flavour.name(),
            version.version,
            settings.map(|s| s.max_response_items.to_string()).unwrap_or_default()
        );
        return o;
    }

    let _ = writeln!(o, "{host}");
    let _ = writeln!(o);
    let _ = writeln!(o, "software      {}", flavour.describe(&version.version));

    let _ = writeln!(o);
    match settings {
        Some(s) => {
            let _ = writeln!(o, "api limits");
            // `max_response_items` first and explained, because it is the one that silently
            // truncates a paginated walk — the failure `gitea_core::http::paginate` exists to
            // survive, and the reason most "my loop stopped after 50" reports happen.
            let _ = writeln!(
                o,
                "  max_response_items          {}  every `limit` is clamped to this, silently",
                s.max_response_items
            );
            let _ = writeln!(
                o,
                "  default_paging_num          {}  page size when none is asked for",
                s.default_paging_num
            );
            let _ = writeln!(
                o,
                "  default_git_trees_per_page  {}  entries per page of a git tree",
                s.default_git_trees_per_page
            );
            let _ = writeln!(
                o,
                "  default_max_blob_size       {}  above this, file contents are omitted",
                crate::cmd::support::size::human(s.default_max_blob_size)
            );
        }
        None => {
            let _ = writeln!(o, "api limits    unknown");
        }
    }

    if let Some(note) = flavour.note() {
        let _ = writeln!(o);
        let _ = writeln!(o, "{note}");
    }
    o
}

// ------------------------------------------------------------------------------------ limits

async fn limits(api: &Api, globals: &GlobalOpts, term: &Term) -> Result<()> {
    let settings = api.settings().get_general_api_settings().await.map_err(explain)?;
    if let Some(m) = Machine::compile(globals, SETTINGS_FIELDS)? {
        return m.write(globals, term, porcelain::json_of(&settings)?);
    }
    porcelain::print(globals, &render_limits(term, &settings))
}

/// The `limits` table, with a column saying what each number does to a request.
pub(crate) fn render_limits(term: &Term, settings: &GeneralApiSettings) -> String {
    let mut t = porcelain::table(term);
    t.headers(["SETTING", "VALUE", "MEANING"]);
    t.row([
        "max_response_items".to_owned(),
        settings.max_response_items.to_string(),
        "every `limit` query parameter is clamped to this, without saying so".to_owned(),
    ]);
    t.row([
        "default_paging_num".to_owned(),
        settings.default_paging_num.to_string(),
        "page size when a request does not ask for one".to_owned(),
    ]);
    t.row([
        "default_git_trees_per_page".to_owned(),
        settings.default_git_trees_per_page.to_string(),
        "entries per page when listing a git tree".to_owned(),
    ]);
    t.row([
        "default_max_blob_size".to_owned(),
        crate::cmd::support::size::human(settings.default_max_blob_size),
        "file contents above this are omitted from responses".to_owned(),
    ]);
    porcelain::rendered_table(term, t, "settings", None)
}

// ------------------------------------------------------------------------------------ shared

/// Which server software a `/version` string came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Flavour {
    /// A plain Gitea version, e.g. `1.27.3`.
    Gitea,
    /// Forgejo, which appends the Gitea API version it tracks: `11.0.1+gitea-1.22.0`. Carries
    /// that Gitea version.
    Forgejo { gitea: String },
    /// `/version` answered with an empty string.
    Unknown,
}

impl Flavour {
    fn detect(version: &str) -> Self {
        let v = version.trim();
        if v.is_empty() {
            return Self::Unknown;
        }
        match v.split_once("+gitea-") {
            Some((_, gitea)) => Self::Forgejo { gitea: gitea.to_owned() },
            None => Self::Gitea,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Gitea => "gitea",
            Self::Forgejo { .. } => "forgejo",
            Self::Unknown => "",
        }
    }

    fn describe(&self, version: &str) -> String {
        match self {
            Self::Gitea => format!("gitea {}", version.trim()),
            Self::Forgejo { gitea } => {
                let own = version.trim().split('+').next().unwrap_or_default();
                format!("forgejo {own} (Gitea-compatible API {gitea})")
            }
            Self::Unknown => "-".to_owned(),
        }
    }

    /// A line about what the reported software means for `gea`.
    ///
    /// The honest version of a version check: `gea` is generated from Gitea's specification, so a
    /// Forgejo instance mostly works and some endpoints differ or do not exist there at all —
    /// the Actions run and workflow routes being the clearest example.
    fn note(&self) -> Option<String> {
        match self {
            Self::Gitea => None,
            Self::Forgejo { .. } => Some(
                "This server runs Forgejo, not Gitea. Most commands work, but Gitea-specific \
                 ones — including the Actions run, job and workflow routes — may be unavailable \
                 or behave differently. Forgejo has its own CLI, fjo."
                    .to_owned(),
            ),
            Self::Unknown => Some(
                "The instance reported an empty version. That is unusual for Gitea; it may be a \
                 proxy answering /version, or a build without version information."
                    .to_owned(),
            ),
        }
    }
}

/// The first line of a rendered error, for a one-line note.
fn headline(e: &Error) -> String {
    gitea_core::error::render::headline(e.kind())
}

/// `/version` missing means this is almost certainly not a Gitea instance.
///
/// The default 404 message talks about resources and tokens, neither of which is the issue when
/// the version route itself is absent. Naming the likely cause — wrong URL, or a plain web
/// server — is the difference between one more command and half an hour.
fn explain(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } | ErrorKind::ResourceNotFound { .. } => {
            Error::new(ErrorKind::Usage(
                "/api/v1/version was not found. This may not be a Gitea server, or the URL may be wrong. Check the browser address, including any subpath such as https://example.org/gitea."
                    .to_owned(),
            ))
        }
        ErrorKind::NotAuthenticated { .. } | ErrorKind::TokenRejected { .. } => {
            Error::new(ErrorKind::Usage(
                "this instance requires a credential even for its public metadata, which means \
                 REQUIRE_SIGNIN_VIEW is on. Run `gea auth login` first, or pass a token in \
                 $GITEA_TOKEN."
                    .to_owned(),
            ))
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const VERSION: &str = r#"{"version":"1.27.3"}"#;

    const SETTINGS: &str = r#"{"max_response_items":50,"default_paging_num":30,
      "default_git_trees_per_page":1000,"default_max_blob_size":10485760}"#;

    fn version() -> ServerVersion {
        serde_json::from_str(VERSION).expect("the fixture is valid ServerVersion JSON")
    }

    fn settings() -> GeneralApiSettings {
        serde_json::from_str(SETTINGS).expect("the fixture is valid GeneralApiSettings JSON")
    }

    /// The property the whole command rests on: **no credential at all**. `Auth::None` means no
    /// `Authorization` header reaches the wire, and both endpoints still answer.
    #[tokio::test]
    async fn both_endpoints_are_read_with_no_credential() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/version", Canned::json(200, VERSION))
                .on(testing::method("GET"), "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        // Built by hand rather than through `testing::api`, because that one attaches a token and
        // the absence of one is exactly what is under test.
        let client = Client::builder("https://git.example.org", Auth::None)
            .transport(fake.clone())
            .build()
            .expect("a well-formed base URL");
        let api = Api::new(client);

        assert_eq!(api.misc().get_version().await.unwrap().version, "1.27.3");
        let s = api.settings().get_general_api_settings().await.unwrap();
        assert_eq!(s.max_response_items, 50);

        for call in fake.calls() {
            assert!(
                call.header("authorization").is_none(),
                "{} {} sent an Authorization header",
                call.method,
                call.path
            );
        }
    }

    /// Everything `overview` writes goes to a file, so a test can call it without a snapshot of
    /// stdout being at stake — what is under test here is which requests it sends, not what it
    /// prints.
    fn to_file(extra: GlobalOpts) -> (GlobalOpts, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a writable temporary directory");
        (GlobalOpts { output: Some(dir.path().join("out")), ..extra }, dir)
    }

    fn both_documents() -> Arc<FakeTransport> {
        Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/version", Canned::json(200, VERSION))
                .on(testing::method("GET"), "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        )
    }

    /// Bug this prevents: spending a round trip before reporting a typo in `--jq`. `machine.rs`
    /// states the rule — a bad expression is a usage error, and it has to land *before* anything
    /// is sent, or the user waits on a server that was never at fault to be told they mistyped.
    #[tokio::test]
    async fn a_bad_jq_expression_is_reported_before_any_request_is_sent() {
        let fake = both_documents();
        let api = testing::api(fake.clone());
        let (globals, _dir) =
            to_file(GlobalOpts { jq: Some(".bad[".into()), ..Default::default() });

        assert!(overview(&api, &globals, &testing::term(), "git.example.org").await.is_err());
        assert_eq!(
            fake.call_count(),
            0,
            "nothing should have gone to the wire: {:?}",
            fake.calls()
        );
    }

    /// Bug this prevents: `--json` paying for the human-only half of the view. `/settings/api`
    /// fills in a table nobody is rendering on the machine path, and a script that polls
    /// `gea nodeinfo --json version` should not double its request count for it.
    #[tokio::test]
    async fn the_machine_view_reads_only_the_version() {
        let fake = both_documents();
        let api = testing::api(fake.clone());
        let (globals, _dir) =
            to_file(GlobalOpts { json: Some("version".into()), ..Default::default() });

        overview(&api, &globals, &testing::term(), "git.example.org").await.unwrap();
        assert_eq!(fake.calls_to(&testing::method("GET"), "/api/v1/settings/api").len(), 0);
        assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
    }

    /// The human view is two documents, and both are requested. Asserted as a *set*, not a
    /// sequence: they are joined, so which one the transport sees first is not fixed.
    #[tokio::test]
    async fn the_human_view_reads_both_documents_once_each() {
        let fake = both_documents();
        let api = testing::api(fake.clone());
        let (globals, _dir) = to_file(GlobalOpts::default());

        overview(&api, &globals, &testing::term(), "git.example.org").await.unwrap();
        assert_eq!(fake.calls_to(&testing::method("GET"), "/api/v1/version").len(), 1);
        assert_eq!(fake.calls_to(&testing::method("GET"), "/api/v1/settings/api").len(), 1);
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
    }

    /// Bug this prevents: joining the two reads with `try_join!`, which abandons the whole command
    /// the moment `/settings/api` answers 401 — turning the note this module promises into a
    /// non-zero exit. `join!` drives both arms to completion and lets the settings half be `None`.
    #[tokio::test]
    async fn a_guarded_settings_endpoint_does_not_fail_the_joined_overview() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/version", Canned::json(200, VERSION))
                .on(
                    testing::method("GET"),
                    "/api/v1/settings/api",
                    Canned::json(401, r#"{"message":"token required"}"#),
                ),
        );
        let api = testing::api(fake.clone());
        let (globals, dir) = to_file(GlobalOpts::default());

        overview(&api, &globals, &testing::term(), "git.example.org").await.unwrap();
        let out = std::fs::read_to_string(dir.path().join("out")).unwrap();
        assert!(out.contains("api limits    unknown"), "{out}");
    }

    #[test]
    fn the_overview_renders_the_same_data_two_ways() {
        insta::assert_snapshot!(
            "nodeinfo_human",
            render_overview(&testing::term(), "git.example.org", &version(), Some(&settings()))
        );
        insta::assert_snapshot!(
            "nodeinfo_piped",
            render_overview(&Term::piped(), "git.example.org", &version(), Some(&settings()))
        );
    }

    #[test]
    fn the_json_output_uses_the_documents_own_field_names() {
        insta::assert_snapshot!(
            "nodeinfo_json",
            testing::as_json(VERSION_FIELDS, "version", porcelain::json_of(&version()).unwrap())
        );
        insta::assert_snapshot!(
            "nodeinfo_limits_json",
            testing::as_json(
                SETTINGS_FIELDS,
                "max_response_items,default_paging_num",
                porcelain::json_of(&settings()).unwrap()
            )
        );
    }

    #[test]
    fn the_limits_table_explains_what_each_number_does() {
        insta::assert_snapshot!(
            "nodeinfo_limits_human",
            render_limits(&testing::term(), &settings())
        );
    }

    /// Bug this prevents: reporting a Forgejo instance as if everything will work. The APIs
    /// overlap enough that most commands do, which is precisely why the difference has to be
    /// stated — otherwise `gea run list` 404ing looks like a bug in gea. And a plain Gitea must
    /// not be nagged about.
    #[test]
    fn a_forgejo_instance_is_named_and_gitea_is_not_nagged_about() {
        assert_eq!(Flavour::detect("1.27.3"), Flavour::Gitea);
        assert_eq!(Flavour::detect("1.28.0+dev-12-gabcdef"), Flavour::Gitea);
        assert!(Flavour::detect("1.27.3").note().is_none());

        let forgejo = Flavour::detect("11.0.1+gitea-1.22.0");
        assert_eq!(forgejo, Flavour::Forgejo { gitea: "1.22.0".to_owned() });
        assert_eq!(
            forgejo.describe("11.0.1+gitea-1.22.0"),
            "forgejo 11.0.1 (Gitea-compatible API 1.22.0)"
        );
        let note = forgejo.note().unwrap();
        assert!(note.contains("Forgejo, not Gitea"), "{note}");

        assert_eq!(Flavour::detect("  "), Flavour::Unknown);
        assert!(Flavour::Unknown.note().is_some(), "an empty version is worth noting");
    }

    /// Bug this prevents: a 404 on `/version` being reported as "resource not found", which
    /// sends the user looking for a missing repository rather than at the URL they typed.
    #[test]
    fn a_missing_version_route_blames_the_url_not_the_token() {
        let e = explain(Error::new(ErrorKind::RouteNotFound {
            method: "GET".to_owned(),
            path: "/version".to_owned(),
            instance: None,
        }));
        let msg = e.to_string();
        assert!(msg.contains("not be a Gitea server"), "{msg}");
        assert!(msg.contains("subpath"), "{msg}");
    }

    /// A 401 here means `REQUIRE_SIGNIN_VIEW`, which is a specific setting with a specific
    /// remedy — not the generic "your token was rejected".
    #[test]
    fn a_401_on_public_metadata_names_require_signin_view() {
        let e =
            explain(Error::new(ErrorKind::NotAuthenticated { host: "git.example.org".to_owned() }));
        assert!(e.to_string().contains("REQUIRE_SIGNIN_VIEW"), "{e}");
    }

    /// The command's whole premise, asserted on the help text: it needs no credential, and it
    /// works against a host that is not configured.
    #[test]
    fn the_help_promises_it_works_before_logging_in() {
        assert!(LONG_ABOUT.contains("No authentication is required"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("--host gitea.com nodeinfo"), "{LONG_ABOUT}");
        assert!(LONG_ABOUT.contains("max_response_items"), "{LONG_ABOUT}");
    }
}

//! `gea webhook` — webhooks at repository, organization, user, and **global** scope.
//!
//! `gh` has no `webhook` command at all, and `tea` has none either, so this group has no
//! muscle memory to copy; it copies the *shape* of `gh`'s other groups instead —
//! `list/create/view/edit/delete` — and adds `test`.
//!
//! # Four scopes, one command
//!
//! Gitea hangs webhooks off four different collections:
//!
//! ```text
//! /repos/{owner}/{repo}/hooks   default; the repository from -R or the checkout
//! /orgs/{org}/hooks             --org NAME
//! /user/hooks                   --user   (the authenticated user's own hooks)
//! /admin/hooks                  --global (instance-wide; needs an admin token)
//! ```
//!
//! Making that a flag rather than four command groups is the whole reason this is porcelain:
//! the four collections have identical payloads, identical verbs, and identical human output,
//! and a user who has learned `gea webhook list` knows all four.
//!
//! # Config is an untyped object
//!
//! The API models a hook's configuration as a free-form map, so the *server* is the validator
//! and this command deliberately does not second-guess it: `-f key=value` reaches the map
//! verbatim. `--url` and `--content-type` are sugar for the two keys every hook needs, because
//! `-f url=…` for a mandatory field is a trap the first time someone forgets it and gets a hook
//! that posts nowhere.
//!
//! # `--inactive`, not `--active`
//!
//! `CreateHookOption.active` defaults to `false`, which means a hook created with the API's own
//! default delivers nothing and looks fine in the UI. This command defaults it to **true** and
//! spells the other direction `--inactive`, because "created a webhook that silently drops every
//! event" is not a default worth being faithful to.

use std::collections::BTreeMap;

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{CreateHookOption, CreateHookOptionType, EditHookOption, Hook};
use gitea_core::error::Result;
use gitea_core::http::Paging;
use gitea_core::types::ids::HookId;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// Every hook payload — repository, org, user, global — is the same `Hook`, so one field table
/// serves all four scopes and `--json` means the same thing everywhere.
const OP_LIST: &str = "repoListHooks";
const OP_ONE: &str = "repoGetHook";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List webhooks
    List(Common),
    /// Create a webhook
    Create(Create),
    /// Show one webhook, with its whole configuration
    View(One),
    /// Change a webhook, leaving everything you did not name alone
    Edit(Edit),
    /// Delete a webhook
    Delete(Delete),
    /// Ask the server to deliver a synthetic push event (repository scope only)
    Test(Test),
}

/// Which of the four collections to act on.
///
/// `--repo` is not declared here: it is the global `-R/--repo`, and the repository scope is what
/// you get when none of these is given.
#[derive(Debug, Clone, ClapArgs)]
pub struct Scope {
    /// Act on an organization's webhooks
    #[arg(long, value_name = "NAME", conflicts_with_all = ["user", "global"])]
    pub org: Option<String>,

    /// Act on your own account's webhooks
    #[arg(long, conflicts_with_all = ["org", "global"])]
    pub user: bool,

    /// Act on the instance's global webhooks (needs an admin token)
    #[arg(long, conflicts_with_all = ["org", "user"])]
    pub global: bool,
}

#[derive(Debug, ClapArgs)]
pub struct Common {
    #[command(flatten)]
    pub scope: Scope,
}

#[derive(Debug, ClapArgs)]
pub struct One {
    /// The webhook's numeric id, as shown by `gea webhook list`
    #[arg(value_name = "ID")]
    pub id: HookId,
    #[command(flatten)]
    pub scope: Scope,
}

#[derive(Debug, ClapArgs)]
pub struct Delete {
    #[arg(value_name = "ID")]
    pub id: HookId,
    #[command(flatten)]
    pub scope: Scope,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct Test {
    #[arg(value_name = "ID")]
    pub id: HookId,
    /// Pretend the push was to this ref
    #[arg(long, value_name = "REF")]
    pub r#ref: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Create {
    /// Where to POST the payload
    #[arg(long, value_name = "URL")]
    pub url: String,

    /// Delivery format. The spec lists gitea, gitea, gogs, slack, discord, dingtalk,
    /// telegram, msteams, feishu, wechatwork, matrix and packagist; an unlisted value is sent
    /// verbatim, so a newer instance's type still works.
    #[arg(long = "type", value_name = "TYPE", default_value = "gitea")]
    pub kind: String,

    /// Events to deliver; repeatable. Defaults to `push`.
    #[arg(short = 'e', long = "event", value_name = "EVENT")]
    pub event: Vec<String>,

    /// Payload encoding: json or form
    #[arg(long, value_name = "TYPE", default_value = "json")]
    pub content_type: String,

    /// Only deliver pushes whose branch matches this glob
    #[arg(long, value_name = "GLOB")]
    pub branch_filter: Option<String>,

    /// Value for the Authorization header the server will send
    #[arg(long, value_name = "HEADER")]
    pub authorization_header: Option<String>,

    /// Any other configuration key: -f secret=hunter2. Repeatable, and passed through
    /// unvalidated because the API models the config as an untyped object.
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub field: Vec<String>,

    /// Create the hook switched off
    #[arg(long)]
    pub inactive: bool,

    #[command(flatten)]
    pub scope: Scope,
}

#[derive(Debug, ClapArgs)]
pub struct Edit {
    #[arg(value_name = "ID")]
    pub id: HookId,

    /// Where to POST the payload
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,

    /// Payload encoding: json or form
    #[arg(long, value_name = "TYPE")]
    pub content_type: Option<String>,

    /// Add an event to the set; repeatable
    #[arg(long = "add-event", value_name = "EVENT")]
    pub add_event: Vec<String>,

    /// Remove an event from the set; repeatable
    #[arg(long = "remove-event", value_name = "EVENT")]
    pub remove_event: Vec<String>,

    /// Only deliver pushes whose branch matches this glob
    #[arg(long, value_name = "GLOB")]
    pub branch_filter: Option<String>,

    /// Value for the Authorization header the server will send
    #[arg(long, value_name = "HEADER")]
    pub authorization_header: Option<String>,

    /// Set a configuration key; repeatable
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub field: Vec<String>,

    /// Switch the hook on
    #[arg(long, conflicts_with = "inactive")]
    pub active: bool,

    /// Switch the hook off
    #[arg(long)]
    pub inactive: bool,

    #[command(flatten)]
    pub scope: Scope,
}

/// The resolved collection, so the request builders below never re-read the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Repo { owner: String, name: String },
    Org(String),
    User,
    Global,
}

impl Target {
    fn resolve(scope: &Scope, rt: &Runtime, globals: &GlobalOpts) -> Result<Self> {
        if let Some(org) = &scope.org {
            return Ok(Self::Org(org.clone()));
        }
        if scope.user {
            return Ok(Self::User);
        }
        if scope.global {
            return Ok(Self::Global);
        }
        // Only the repository scope needs `git`, so `--global` works outside a checkout.
        let slug = &rt.repo(globals)?.slug;
        Ok(Self::Repo { owner: slug.owner.clone(), name: slug.name.clone() })
    }

    fn label(&self) -> String {
        match self {
            Self::Repo { owner, name } => format!("{owner}/{name}"),
            Self::Org(org) => format!("organization {org}"),
            Self::User => "your account".to_owned(),
            Self::Global => "this instance".to_owned(),
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let op = match &args.command {
        Cmd::List(_) => OP_LIST,
        Cmd::View(_) | Cmd::Create(_) | Cmd::Edit(_) => OP_ONE,
        // `delete` and `test` answer 204, so there is nothing to select.
        Cmd::Delete(_) | Cmd::Test(_) => "",
    };
    let fields = if op.is_empty() {
        None
    } else {
        match Json::resolve(globals, op)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        }
    };

    // Parsed before the runtime exists, so `-f nonsense` is a usage error rather than "no
    // Gitea host is set up yet".
    let config = match &args.command {
        Cmd::Create(a) => config_from(&a.field)?,
        Cmd::Edit(a) => config_from(&a.field)?,
        _ => BTreeMap::new(),
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List(a) => {
                let target = Target::resolve(&a.scope, &rt, globals)?;
                list(&api, &target, globals, &mut emit).await
            }
            Cmd::View(a) => {
                let target = Target::resolve(&a.scope, &rt, globals)?;
                let hook = get(&api, &target, a.id).await?;
                emit.one(&hook, |t| detail(t, &hook))
            }
            Cmd::Create(a) => {
                let target = Target::resolve(&a.scope, &rt, globals)?;
                create(&api, &target, a, config, &mut emit).await
            }
            Cmd::Edit(a) => {
                let target = Target::resolve(&a.scope, &rt, globals)?;
                edit(&api, &target, a, config, &mut emit).await
            }
            Cmd::Delete(a) => {
                let target = Target::resolve(&a.scope, &rt, globals)?;
                support::confirm_term(
                    emit.term(),
                    a.yes,
                    &format!("delete webhook {} on {}", a.id, target.label()),
                )?;
                delete(&api, &target, a.id).await?;
                emit.done(&format!("deleted webhook {}", a.id));
                Ok(())
            }
            Cmd::Test(a) => {
                let slug = &rt.repo(globals)?.slug;
                let query = match &a.r#ref {
                    Some(r) => gitea_client::query::RepoTestHookQuery::default().with_ref(r),
                    None => gitea_client::query::RepoTestHookQuery::default(),
                };
                api.repo().test_hook(&slug.owner, &slug.name, a.id.get(), &query).await?;
                emit.done(&format!(
                    "requested a test push delivery from {} to webhook {}",
                    slug, a.id
                ));
                Ok(())
            }
        }
    })
}

// --------------------------------------------------------------------------------- the calls

async fn list(api: &Api, target: &Target, globals: &GlobalOpts, emit: &mut Emit<'_>) -> Result<()> {
    let cap = support::item_cap(globals);
    let (hooks, total) = if globals.paginate {
        let hooks = match target {
            Target::Repo { owner, name } => {
                let q = gitea_client::query::RepoListHooksQuery::default();
                support::drain(api.repo().list_hooks(owner, name, &q), cap).await?
            }
            Target::Org(org) => {
                let q = gitea_client::query::OrgListHooksQuery::default();
                support::drain(api.org().list_hooks(org, &q), cap).await?
            }
            Target::User => {
                let q = gitea_client::query::UserListHooksQuery::default();
                support::drain(api.user().list_hooks(&q), cap).await?
            }
            Target::Global => {
                let q = gitea_client::query::AdminListHooksQuery::default();
                support::drain(api.admin().list_hooks(&q), cap).await?
            }
        };
        let n = hooks.len() as u64;
        (hooks, Some(n))
    } else {
        let paging = Paging { limit: cap, per_page: None };
        let (hooks, info) = match target {
            Target::Repo { owner, name } => {
                let q = gitea_client::query::RepoListHooksQuery::default();
                api.repo().list_hooks_page(owner, name, &q, paging).await?
            }
            Target::Org(org) => {
                let q = gitea_client::query::OrgListHooksQuery::default();
                api.org().list_hooks_page(org, &q, paging).await?
            }
            Target::User => {
                let q = gitea_client::query::UserListHooksQuery::default();
                api.user().list_hooks_page(&q, paging).await?
            }
            Target::Global => {
                let q = gitea_client::query::AdminListHooksQuery::default();
                api.admin().list_hooks_page(&q, paging).await?
            }
        };
        (hooks, info.total_count)
    };

    emit.many(&hooks, total, "webhooks", |table, hooks| {
        table.headers(["ID", "TYPE", "URL", "EVENTS", "ACTIVE"]);
        for h in hooks {
            table.row([
                h.id.to_string(),
                h.r#type.clone(),
                hook_url(h),
                h.events.join(","),
                if h.active { "active" } else { "inactive" }.to_owned(),
            ]);
        }
    })
}

async fn get(api: &Api, target: &Target, id: HookId) -> Result<Hook> {
    match target {
        Target::Repo { owner, name } => api.repo().get_hook(owner, name, id.get()).await,
        Target::Org(org) => api.org().get_hook(org, id.get()).await,
        Target::User => api.user().get_hook(id.get()).await,
        Target::Global => api.admin().get_hook(id.get()).await,
    }
}

async fn delete(api: &Api, target: &Target, id: HookId) -> Result<()> {
    match target {
        Target::Repo { owner, name } => api.repo().delete_hook(owner, name, id.get()).await,
        Target::Org(org) => api.org().delete_hook(org, id.get()).await,
        Target::User => api.user().delete_hook(id.get()).await,
        Target::Global => api.admin().delete_hook(id.get()).await,
    }
}

async fn create(
    api: &Api,
    target: &Target,
    args: &Create,
    mut config: BTreeMap<String, String>,
    emit: &mut Emit<'_>,
) -> Result<()> {
    // `-f url=…` loses to `--url`, and both are recorded in the same map, because the server
    // reads exactly one `url` key and silently ignoring the flag would be worse.
    config.insert("url".to_owned(), args.url.clone());
    config.insert("content_type".to_owned(), args.content_type.clone());

    let events = if args.event.is_empty() { vec!["push".to_owned()] } else { args.event.clone() };

    let body = CreateHookOption {
        active: Some(!args.inactive),
        authorization_header: args.authorization_header.clone(),
        branch_filter: args.branch_filter.clone(),
        config,
        events: Some(events),
        name: None,
        r#type: CreateHookOptionType::from(args.kind.as_str()),
    };

    let hook = match target {
        Target::Repo { owner, name } => api.repo().create_hook(owner, name, &body).await?,
        Target::Org(org) => api.org().create_hook(org, &body).await?,
        Target::User => api.user().create_hook(&body).await?,
        Target::Global => api.admin().create_hook(&body).await?,
    };
    emit.done(&format!("created webhook {} on {}", hook.id, target.label()));
    emit.one(&hook, |t| detail(t, &hook))
}

/// Read, modify, write — and that is not an optimization, it is the only correct shape.
///
/// `EditHookOption` has no `Option` fields, so every one of them is serialised on every PATCH.
/// Sending a default-constructed body would post `"active": false` and switch off the hook the
/// user was only renaming. Seeding from the current hook means an unspecified flag genuinely
/// means "leave it alone".
///
/// `authorization_header` is the one field the server does not echo back — it returns an empty
/// string — but Gitea also ignores an empty `authorization_header` on a PATCH, so round-tripping
/// the blank is a no-op rather than a secret being cleared.
async fn edit(
    api: &Api,
    target: &Target,
    args: &Edit,
    config: BTreeMap<String, String>,
    emit: &mut Emit<'_>,
) -> Result<()> {
    let current = get(api, target, args.id).await?;

    let mut merged = current.config.clone();
    merged.extend(config);
    if let Some(url) = &args.url {
        merged.insert("url".to_owned(), url.clone());
    }
    if let Some(ct) = &args.content_type {
        merged.insert("content_type".to_owned(), ct.clone());
    }

    let mut events = current.events.clone();
    for e in &args.add_event {
        if !events.contains(e) {
            events.push(e.clone());
        }
    }
    events.retain(|e| !args.remove_event.contains(e));

    let active = match (args.active, args.inactive) {
        (true, _) => true,
        (_, true) => false,
        _ => current.active,
    };

    let body = EditHookOption {
        active: Some(active),
        authorization_header: Some(
            args.authorization_header
                .clone()
                .unwrap_or_else(|| current.authorization_header.clone()),
        ),
        branch_filter: Some(
            args.branch_filter.clone().unwrap_or_else(|| current.branch_filter.clone()),
        ),
        config: Some(merged),
        events: Some(events),
        name: None,
    };

    let hook = match target {
        Target::Repo { owner, name } => {
            api.repo().edit_hook(owner, name, args.id.get(), &body).await?
        }
        Target::Org(org) => api.org().edit_hook(org, args.id.get(), &body).await?,
        Target::User => api.user().edit_hook(args.id.get(), &body).await?,
        Target::Global => api.admin().edit_hook(args.id.get(), &body).await?,
    };
    emit.done(&format!("updated webhook {}", hook.id));
    emit.one(&hook, |t| detail(t, &hook))
}

// ------------------------------------------------------------------------------------ helpers

/// `key=value` pairs into the untyped config map.
fn config_from(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(support::usage(format!(
                "-f {pair:?} is not a key=value pair; webhook configuration keys are free-form, \
                 so write them as -f secret=hunter2"
            )));
        };
        if key.trim().is_empty() {
            return Err(support::usage(format!("-f {pair:?} has no key before the '='")));
        }
        out.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(out)
}

/// The delivery URL, which lives in the untyped config map rather than in a typed field.
fn hook_url(h: &Hook) -> String {
    h.config.get("url").cloned().unwrap_or_default()
}

fn detail(table: &mut crate::output::Table, h: &Hook) {
    table.row(["id".to_owned(), h.id.to_string()]);
    table.row(["type".to_owned(), h.r#type.clone()]);
    table.row(["url".to_owned(), hook_url(h)]);
    table.row(["active".to_owned(), h.active.to_string()]);
    table.row(["events".to_owned(), h.events.join(",")]);
    if !h.branch_filter.is_empty() {
        table.row(["branch_filter".to_owned(), h.branch_filter.clone()]);
    }
    for (key, value) in &h.config {
        if key == "url" {
            continue;
        }
        table.row([format!("config.{key}"), value.clone()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    fn scope(org: Option<&str>, user: bool, global: bool) -> Scope {
        Scope { org: org.map(str::to_owned), user, global }
    }

    fn repo_target() -> Target {
        Target::Repo { owner: "acme".into(), name: "widget".into() }
    }

    const ONE_HOOK: &str = r#"[{"id":4,"type":"gitea","active":true,
        "events":["push","pull_request"],
        "config":{"url":"https://ci.example/hook","content_type":"json"}}]"#;

    /// Bug this prevents: the scope flags all reaching the same collection, so `--global` quietly
    /// lists the repository's hooks. Four flags, four paths, asserted on the wire.
    #[tokio::test]
    async fn each_scope_reaches_its_own_collection() {
        for (target, path) in [
            (repo_target(), "/api/v1/repos/acme/widget/hooks"),
            (Target::Org("acme".into()), "/api/v1/orgs/acme/hooks"),
            (Target::User, "/api/v1/user/hooks"),
            (Target::Global, "/api/v1/admin/hooks"),
        ] {
            let fake = Arc::new(testing::on(
                FakeTransport::new(),
                "GET",
                path,
                Canned::json(200, ONE_HOOK),
            ));
            let api = testing::api(fake.clone());
            let globals = GlobalOpts::default();
            let mut buf: Vec<u8> = Vec::new();
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            list(&api, &target, &globals, &mut emit).await.unwrap();
            assert_eq!(fake.calls()[0].path, path, "{target:?}");
            assert_eq!(
                String::from_utf8(buf).unwrap(),
                "4\tgitea\thttps://ci.example/hook\tpush,pull_request\tactive\n"
            );
        }
    }

    /// Bug this prevents: honouring `CreateHookOption`'s own default of `active: false`, which
    /// creates a webhook that looks configured and delivers nothing.
    #[tokio::test]
    async fn create_defaults_to_active_with_a_push_event() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/acme/widget/hooks",
            Canned::json(201, r#"{"id":9,"type":"gitea","active":true,"events":["push"]}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Create {
            url: "https://ci.example/hook".into(),
            kind: "gitea".into(),
            event: Vec::new(),
            content_type: "json".into(),
            branch_filter: None,
            authorization_header: None,
            field: Vec::new(),
            inactive: false,
            scope: scope(None, false, false),
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        create(&api, &repo_target(), &args, BTreeMap::new(), &mut emit).await.unwrap();

        let body: serde_json::Value =
            serde_json::from_str(&fake.calls()[0].body_str()).expect("a JSON body");
        assert_eq!(body["active"], serde_json::json!(true));
        assert_eq!(body["events"], serde_json::json!(["push"]));
        assert_eq!(body["config"]["url"], serde_json::json!("https://ci.example/hook"));
        assert_eq!(body["config"]["content_type"], serde_json::json!("json"));
    }

    /// Bug this prevents: `-f` pairs being dropped, or a typed flag being overwritten by the
    /// free-form map. The config is an untyped object, so both have to land in it and `--url`
    /// has to win.
    #[tokio::test]
    async fn free_form_config_and_the_url_flag_share_one_map() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/user/hooks",
            Canned::json(201, r#"{"id":1}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Create {
            url: "https://real.example/hook".into(),
            kind: "matrix".into(),
            event: vec!["push".into()],
            content_type: "json".into(),
            branch_filter: None,
            authorization_header: None,
            field: vec!["url=https://ignored.example".into(), "secret=hunter2".into()],
            inactive: true,
            scope: scope(None, true, false),
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let config = config_from(&args.field).unwrap();
        create(&api, &Target::User, &args, config, &mut emit).await.unwrap();

        let body: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(body["config"]["url"], serde_json::json!("https://real.example/hook"));
        assert_eq!(body["config"]["secret"], serde_json::json!("hunter2"));
        assert_eq!(body["active"], serde_json::json!(false));
        // `matrix` is absent from the spec this build was generated from, and must still reach
        // the wire verbatim rather than falling back to a default type.
        assert_eq!(body["type"], serde_json::json!("matrix"));
    }

    /// Bug this prevents: a default-constructed `EditHookOption` deactivating the hook and
    /// erasing its events, because none of its fields is an `Option` and all of them serialise.
    #[tokio::test]
    async fn edit_preserves_everything_it_was_not_asked_to_change() {
        let fake = FakeTransport::new();
        let fake = testing::on(
            fake,
            "GET",
            "/api/v1/repos/acme/widget/hooks/4",
            Canned::json(
                200,
                r#"{"id":4,"type":"gitea","active":true,"branch_filter":"main",
                    "events":["push","issues"],
                    "config":{"url":"https://old.example","content_type":"json","secret":"keep"}}"#,
            ),
        );
        let fake = Arc::new(testing::on(
            fake,
            "PATCH",
            "/api/v1/repos/acme/widget/hooks/4",
            Canned::json(200, r#"{"id":4,"active":true}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Edit {
            id: HookId::new(4),
            url: Some("https://new.example".into()),
            content_type: None,
            add_event: vec!["release".into()],
            remove_event: vec!["issues".into()],
            branch_filter: None,
            authorization_header: None,
            field: Vec::new(),
            active: false,
            inactive: false,
            scope: scope(None, false, false),
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        edit(&api, &repo_target(), &args, BTreeMap::new(), &mut emit).await.unwrap();

        let patch = fake.calls_to_patch();
        let body: serde_json::Value = serde_json::from_str(&patch).unwrap();
        assert_eq!(body["active"], serde_json::json!(true), "the hook must stay switched on");
        assert_eq!(body["branch_filter"], serde_json::json!("main"));
        assert_eq!(body["config"]["secret"], serde_json::json!("keep"));
        assert_eq!(body["config"]["url"], serde_json::json!("https://new.example"));
        assert_eq!(body["events"], serde_json::json!(["push", "release"]));
    }

    trait PatchBody {
        fn calls_to_patch(&self) -> String;
    }

    impl PatchBody for FakeTransport {
        fn calls_to_patch(&self) -> String {
            self.calls()
                .into_iter()
                .find(|c| c.method == "PATCH")
                .expect("a PATCH was sent")
                .body_str()
        }
    }

    #[test]
    fn a_config_pair_without_an_equals_is_a_usage_error() {
        assert_eq!(config_from(&["secret".to_owned()]).unwrap_err().exit_code(), 2);
        assert_eq!(config_from(&["=x".to_owned()]).unwrap_err().exit_code(), 2);
        // A value containing `=` is legal and is not split twice.
        let map = config_from(&["secret=a=b".to_owned()]).unwrap();
        assert_eq!(map.get("secret").map(String::as_str), Some("a=b"));
    }
}

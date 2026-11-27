//! `gea admin user` — accounts on the instance.
//!
//! Creating an account is the single thing an instance operator does most often, and it is the one
//! `tea admin users create` already covers, so this is the one place in the group where there is
//! prior art to match rather than invent.
//!
//! # Why `edit` builds its own request body
//!
//! `PATCH /admin/users/{username}` is a partial update on the wire: Gitea's own option struct
//! uses pointers, so a key that is *absent* means "leave it alone". The generated
//! `EditUserOption` cannot express absence — every field is a plain `bool`/`String`/`i64` with
//! `#[serde(default)]`, so all of them serialise on every request. Handing the typed method a
//! struct built from flags would therefore send, among other things:
//!
//! ```text
//! "max_repo_creation": 0            the account may now create no repositories
//! "hide_email": false              the account's email address is now public
//! "allow_create_organization": false
//! ```
//!
//! …for a command line that only said `--full-name "Ada Lovelace"`. That is silent, unrecoverable
//! damage to somebody else's account, so `edit` assembles a body containing **only** the keys the
//! operator named and sends it through [`gitea_core::http::Client`] directly.
//!
//! Two guards keep that from rotting: `EDIT_KEYS` is checked against `EditUserOption`'s own
//! serialisation by a test, so a spec rename fails the build rather than silently sending a key
//! Gitea ignores; and a second test asserts the hand-built path is byte-identical to the one the
//! generated method produces.
//!
//! **The emitter has since been fixed**: an optional PATCH body field is now `Option<T>` with
//! `skip_serializing_if`, so a default `EditUserOption` serialises to `{}` rather than to a full
//! set of zero values. The hand-built body stays because it is also how this command expresses
//! "the operator did not mention this key", and because the two guards above are what would
//! catch a regression — but it is no longer working around a missing feature.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{CreateUserOption, User, VisibilityMode};
use gitea_core::error::Result;
use gitea_core::http::{Paging, Request, encode};
use serde_json::{Map, Value};

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;
use crate::output::Table;

pub const OP_USER: &str = "adminSearchUsers";

/// Every key `edit` is willing to send.
///
/// Exists for the test that checks it against `EditUserOption`'s serialised field names, in both
/// directions: a key that is not on the generated option is a key Gitea will ignore, and a
/// silently ignored `--admin` reports success and changes nothing. `#[cfg(test)]` because the
/// production path builds its keys from the flags directly — this is the list that says what those
/// keys are *allowed* to be.
#[cfg(test)]
const EDIT_KEYS: &[&str] = &[
    "active",
    "admin",
    "description",
    "email",
    "full_name",
    "location",
    "login_name",
    "max_repo_creation",
    "must_change_password",
    "password",
    "prohibit_login",
    "restricted",
    "source_id",
    "visibility",
    "website",
];

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the accounts on this instance
    List(List),

    /// Create an account, and optionally make it a site administrator
    ///
    /// The account can sign in immediately. Its password must be changed at first web
    /// sign-in unless --no-must-change-password is set.
    Create(Create),

    /// Change an account, touching only the settings you name
    ///
    /// Unspecified settings are unchanged.
    Edit(Edit),

    /// Remove an account from this instance
    ///
    /// Gitea refuses by default if the account still owns repositories or packages. --purge
    /// deletes those too, and there is no undo.
    Delete(Delete),
}

#[derive(Debug, ClapArgs)]
pub struct List {
    /// Only accounts whose external authentication name matches
    #[arg(long, value_name = "NAME")]
    pub login_name: Option<String>,

    /// Only accounts from this authentication source id
    #[arg(long, value_name = "ID")]
    pub source_id: Option<i64>,

    /// Only accounts with two-factor authentication enabled
    #[arg(long = "2fa")]
    pub two_factor: bool,

    /// Ordering: oldest, newest, alphabetically, reversealphabetically, recentupdate, leastupdate
    #[arg(long, value_name = "ORDER")]
    pub sort: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Create {
    /// The account name people will type
    #[arg(value_name = "USERNAME")]
    pub username: String,

    /// The account's email address; Gitea requires one and refuses duplicates
    #[arg(long, value_name = "EMAIL")]
    pub email: String,

    /// The initial password. Prompted for if omitted and you are on a terminal.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// Display name
    #[arg(long, value_name = "NAME")]
    pub full_name: Option<String>,

    /// Make the account a site administrator
    #[arg(long)]
    pub admin: bool,

    /// Restrict the account: it can only see repositories it is explicitly given access to
    #[arg(long)]
    pub restricted: bool,

    /// Let the account keep the password you set, rather than choosing its own at first sign-in
    #[arg(long)]
    pub no_must_change_password: bool,

    /// Accepted for clarity; this is already the default
    #[arg(long, conflicts_with = "no_must_change_password")]
    pub must_change_password: bool,

    /// Email the account about its creation
    #[arg(long)]
    pub send_notify: bool,

    /// Profile visibility: public, limited or private
    #[arg(long, value_name = "MODE")]
    pub visibility: Option<String>,

    /// Authentication source id, for an externally authenticated account
    #[arg(long, value_name = "ID")]
    pub source_id: Option<i64>,

    /// The name to authenticate with against that source
    #[arg(long, value_name = "NAME")]
    pub login_name: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Edit {
    #[arg(value_name = "USERNAME")]
    pub username: String,

    /// Grant site administrator
    #[arg(long, conflicts_with = "no_admin")]
    pub admin: bool,
    /// Revoke site administrator
    #[arg(long)]
    pub no_admin: bool,

    /// Allow the account to sign in
    #[arg(long, conflicts_with = "deactivate")]
    pub activate: bool,
    /// Stop the account signing in, without deleting anything
    #[arg(long)]
    pub deactivate: bool,

    /// Refuse this account's sign-ins outright
    #[arg(long, conflicts_with = "allow_login")]
    pub prohibit_login: bool,
    /// Undo --prohibit-login
    #[arg(long)]
    pub allow_login: bool,

    /// Restrict the account to repositories it is explicitly given access to
    #[arg(long, conflicts_with = "unrestrict")]
    pub restrict: bool,
    /// Undo --restrict
    #[arg(long)]
    pub unrestrict: bool,

    /// New email address
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,

    /// New display name
    #[arg(long, value_name = "NAME")]
    pub full_name: Option<String>,

    /// New password. The account is not asked to change it again unless you say so.
    #[arg(long, value_name = "PASSWORD")]
    pub password: Option<String>,

    /// Require a password change at the next web sign-in
    #[arg(long)]
    pub must_change_password: bool,

    /// How many repositories the account may create; -1 means the instance default
    #[arg(long, value_name = "N", allow_hyphen_values = true)]
    pub max_repo_creation: Option<i64>,

    /// Profile visibility: public, limited or private
    #[arg(long, value_name = "MODE")]
    pub visibility: Option<String>,

    /// Free-text description on the profile
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Profile location
    #[arg(long, value_name = "TEXT")]
    pub location: Option<String>,

    /// Profile website
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Delete {
    #[arg(value_name = "USERNAME")]
    pub username: String,

    /// Also delete everything the account owns: repositories, packages, comments
    #[arg(long)]
    pub purge: bool,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List(_) | Cmd::Create(_) | Cmd::Edit(_) => OP_USER,
        Cmd::Delete(_) => "",
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    !matches!(cmd, Cmd::List(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List(a) => list(api, globals, emit, a).await,
        Cmd::Create(a) => create(api, emit, a).await,
        Cmd::Edit(a) => edit(api, emit, a).await,
        Cmd::Delete(a) => delete(api, emit, a).await,
    }
}

async fn list(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, args: &List) -> Result<()> {
    let mut q = gitea_client::query::AdminSearchUsersQuery::default();
    if let Some(n) = &args.login_name {
        q = q.with_login_name(n);
    }
    if let Some(id) = args.source_id {
        q = q.with_source_id(id);
    }
    if args.two_factor {
        q = q.with_is_2_fa_enabled(true);
    }
    if let Some(s) = &args.sort {
        q = q.with_sort(s);
    }

    let cap = support::item_cap(globals);
    let (users, total) = if globals.paginate {
        let users = support::drain(api.admin().search_users(&q), cap).await?;
        let n = users.len() as u64;
        (users, Some(n))
    } else {
        let (users, info) =
            api.admin().search_users_page(&q, Paging { limit: cap, per_page: None }).await?;
        (users, info.total_count)
    };

    emit.many(&users, total, "accounts", |table, users| {
        table.headers(["LOGIN", "EMAIL", "ROLE", "STATE", "LAST LOGIN"]);
        for u in users {
            table.row([
                u.login.clone(),
                u.email.clone(),
                if u.is_admin { "admin" } else { "user" }.to_owned(),
                state_of(u),
                u.last_login.as_ref().map(ToString::to_string).unwrap_or_default(),
            ]);
        }
    })
}

/// Create, then — only if asked — promote.
///
/// `CreateUserOption` has no `admin` field, so `--admin` is genuinely a second request. Doing it
/// here rather than making the operator run two commands is the whole point of the porcelain
/// layer, and the ordering is the safe one: an account that exists but was not promoted is easy to
/// fix, while the reverse is not expressible.
async fn create(api: &Api, emit: &mut Emit<'_>, args: &Create) -> Result<()> {
    let password = match &args.password {
        Some(p) => p.clone(),
        None => prompt_password(emit)?,
    };

    let body = CreateUserOption {
        created_at: None,
        email: args.email.clone(),
        full_name: args.full_name.clone(),
        login_name: args.login_name.clone(),
        // Defaulted the other way round from the API: the operator typed this password, so the
        // account should not keep it.
        must_change_password: Some(!args.no_must_change_password),
        password: Some(password),
        restricted: Some(args.restricted),
        send_notify: Some(args.send_notify),
        source_id: args.source_id,
        username: args.username.clone(),
        visibility: args.visibility.as_deref().map(VisibilityMode::from),
    };

    let mut user = api.admin().create_user(&body).await?;
    emit.done(&format!("created account {}", user.login));

    if args.admin {
        let mut patch = Map::new();
        patch.insert("admin".to_owned(), Value::Bool(true));
        user = send_patch(api, &args.username, patch).await?;
        emit.done(&format!("made {} a site administrator", user.login));
    }

    emit.one(&user, |t| detail(t, &user))
}

async fn edit(api: &Api, emit: &mut Emit<'_>, args: &Edit) -> Result<()> {
    let patch = patch_from(args);
    if patch.is_empty() {
        return Err(support::usage(format!(
            "nothing to change on {}; name at least one setting, e.g. --full-name or --admin",
            args.username
        )));
    }
    let changed: Vec<&str> = patch.keys().map(String::as_str).collect();
    let user = send_patch(api, &args.username, patch.clone()).await?;
    emit.done(&format!("updated {} ({})", user.login, changed.join(", ")));
    emit.one(&user, |t| detail(t, &user))
}

async fn delete(api: &Api, emit: &mut Emit<'_>, args: &Delete) -> Result<()> {
    let what = if args.purge {
        format!("delete {} and everything it owns — there is no undo", args.username)
    } else {
        format!("delete the account {}", args.username)
    };
    support::confirm_term(emit.term(), args.yes, &what)?;

    let q = gitea_client::query::AdminDeleteUserQuery::default().with_purge(args.purge);
    api.admin().delete_user(&args.username, &q).await?;
    emit.done(&format!("deleted {}", args.username));
    Ok(())
}

// ------------------------------------------------------------------------------- the PATCH

/// Only the keys the operator named. See the module comment for why this is not the typed option.
fn patch_from(args: &Edit) -> Map<String, Value> {
    let mut p = Map::new();
    let mut set_bool = |key: &str, on: bool, off: bool| {
        if on {
            p.insert(key.to_owned(), Value::Bool(true));
        } else if off {
            p.insert(key.to_owned(), Value::Bool(false));
        }
    };
    set_bool("admin", args.admin, args.no_admin);
    set_bool("active", args.activate, args.deactivate);
    set_bool("prohibit_login", args.prohibit_login, args.allow_login);
    set_bool("restricted", args.restrict, args.unrestrict);
    if args.must_change_password {
        p.insert("must_change_password".to_owned(), Value::Bool(true));
    }
    for (key, value) in [
        ("email", &args.email),
        ("full_name", &args.full_name),
        ("password", &args.password),
        ("visibility", &args.visibility),
        ("description", &args.description),
        ("location", &args.location),
        ("website", &args.website),
    ] {
        if let Some(v) = value {
            p.insert(key.to_owned(), Value::String(v.clone()));
        }
    }
    if let Some(n) = args.max_repo_creation {
        p.insert("max_repo_creation".to_owned(), Value::from(n));
    }
    p
}

/// The path the generated `Admin::edit_user` builds. Pinned by a test against the real method, so
/// a spec change that moves the route cannot leave this behind.
fn edit_path(username: &str) -> String {
    format!("/admin/users/{}", encode::seg(username))
}

async fn send_patch(api: &Api, username: &str, patch: Map<String, Value>) -> Result<User> {
    let req = Request::patch(edit_path(username)).json_body(&Value::Object(patch))?;
    api.client().json(req).await
}

// -------------------------------------------------------------------------------- presentation

fn prompt_password(emit: &Emit<'_>) -> Result<String> {
    if !emit.term().tty {
        return Err(support::usage(
            "creating an account needs a password; pass --password, or run this on a terminal to \
             be prompted for one",
        ));
    }
    inquire::Password::new("Initial password:")
        .with_display_toggle_enabled()
        .without_confirmation()
        .prompt()
        .map_err(|_| gitea_core::Error::new(gitea_core::ErrorKind::Cancelled))
}

/// The words an operator scans for: whether the account can sign in at all.
fn state_of(u: &User) -> String {
    let mut flags: Vec<&str> = Vec::new();
    if !u.active {
        flags.push("inactive");
    }
    if u.prohibit_login {
        flags.push("login-prohibited");
    }
    if u.restricted {
        flags.push("restricted");
    }
    if flags.is_empty() { "ok".to_owned() } else { flags.join("+") }
}

fn detail(table: &mut Table, u: &User) {
    table.row(["login".to_owned(), u.login.clone()]);
    table.row(["id".to_owned(), u.id.to_string()]);
    table.row(["email".to_owned(), u.email.clone()]);
    table.row(["full_name".to_owned(), u.full_name.clone()]);
    table.row(["site_admin".to_owned(), u.is_admin.to_string()]);
    table.row(["state".to_owned(), state_of(u)]);
    table.row(["visibility".to_owned(), u.visibility.to_string()]);
    if !u.login_name.is_empty() {
        table.row(["login_name".to_owned(), u.login_name.clone()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_client::gitea_model::EditUserOption;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    fn blank_edit(username: &str) -> Edit {
        Edit {
            username: username.to_owned(),
            admin: false,
            no_admin: false,
            activate: false,
            deactivate: false,
            prohibit_login: false,
            allow_login: false,
            restrict: false,
            unrestrict: false,
            email: None,
            full_name: None,
            password: None,
            must_change_password: false,
            max_repo_creation: None,
            visibility: None,
            description: None,
            location: None,
            website: None,
        }
    }

    /// **The bug this whole module is shaped around.** `EditUserOption` has no `Option` fields, so
    /// serialising one sends every key. A `PATCH` built that way from `--full-name` alone would
    /// also send `max_repo_creation: 0` (the account may now create no repositories) and
    /// `hide_email: false` (its address is now public) — silent damage to somebody else's account.
    #[test]
    fn a_patch_carries_only_the_settings_that_were_named() {
        let mut args = blank_edit("ada");
        args.full_name = Some("Ada Lovelace".to_owned());
        let patch = patch_from(&args);
        assert_eq!(patch.keys().collect::<Vec<_>>(), vec!["full_name"]);

        // The generated option now agrees, which it did not before the request-body `Presence`
        // policy landed: a field nobody named is omitted rather than sent as its zero value.
        let typed = serde_json::to_value(EditUserOption {
            full_name: Some("Ada Lovelace".to_owned()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(typed, serde_json::json!({"full_name": "Ada Lovelace"}));
    }

    /// `--admin`/`--no-admin` and friends have to reach the wire as `true` *and* `false`, not as
    /// "present or absent": revoking site administrator is exactly as important as granting it.
    #[test]
    fn paired_flags_send_both_polarities() {
        let mut on = blank_edit("ada");
        on.admin = true;
        on.deactivate = true;
        on.unrestrict = true;
        let p = patch_from(&on);
        assert_eq!(p["admin"], serde_json::json!(true));
        assert_eq!(p["active"], serde_json::json!(false));
        assert_eq!(p["restricted"], serde_json::json!(false));

        // `-1` means "the instance default" and must survive as a negative number, which is why
        // the flag allows a hyphen value.
        let mut n = blank_edit("ada");
        n.max_repo_creation = Some(-1);
        assert_eq!(patch_from(&n)["max_repo_creation"], serde_json::json!(-1));
    }

    /// Bug this prevents: a spec rename leaving `EDIT_KEYS` pointing at a key Gitea ignores, so
    /// `--admin` reports success and changes nothing.
    #[test]
    fn every_key_edit_can_send_exists_on_the_generated_option() {
        let mut everything = blank_edit("ada");
        everything.admin = true;
        everything.deactivate = true;
        everything.prohibit_login = true;
        everything.restrict = true;
        everything.must_change_password = true;
        everything.email = Some("a@b".into());
        everything.full_name = Some("A".into());
        everything.password = Some("p".into());
        everything.visibility = Some("private".into());
        everything.description = Some("d".into());
        everything.location = Some("l".into());
        everything.website = Some("w".into());
        everything.max_repo_creation = Some(3);
        for key in patch_from(&everything).keys() {
            assert!(EDIT_KEYS.contains(&key.as_str()), "{key} is not declared in EDIT_KEYS");
        }

        // ...and every key in EDIT_KEYS survives a round trip through the generated option.
        //
        // Round-tripped rather than read off `EditUserOption::default()`, which is how this used
        // to work: an unset request-body field is no longer serialized, so the default is now
        // `{}`. The round trip is the better check anyway — models carry no
        // `deny_unknown_fields`, so a key the spec renamed away is silently dropped on the way
        // in and is therefore missing on the way back out.
        let every_key = serde_json::json!({
            "active": true,
            "admin": true,
            "description": "d",
            "email": "a@b",
            "full_name": "A",
            "location": "l",
            "login_name": "ada",
            "max_repo_creation": 3,
            "must_change_password": true,
            "password": "p",
            "prohibit_login": false,
            "restricted": false,
            "source_id": 1,
            "visibility": "private",
            "website": "w",
        });
        // The literal above cannot drift from EDIT_KEYS without failing here first.
        assert_eq!(
            every_key.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            EDIT_KEYS
        );

        let model: EditUserOption = serde_json::from_value(every_key).unwrap();
        let round_tripped = serde_json::to_value(&model).unwrap();
        for key in EDIT_KEYS {
            assert!(
                round_tripped.get(*key).is_some(),
                "EditUserOption has no {key:?} field any more"
            );
        }
    }

    /// Bug this prevents: the hand-built PATCH path drifting from the generated route, which would
    /// 404 only for `edit` while every other admin command kept working.
    #[tokio::test]
    async fn the_hand_built_patch_path_matches_the_generated_one() {
        let fake = Arc::new(testing::on_fn(
            FakeTransport::new(),
            "PATCH",
            "/api/v1/admin/users/od%2Fd",
            |_| Canned::json(200, r#"{"login":"od/d"}"#),
        ));
        let api = testing::api(fake.clone());

        // The generated method, then ours. A name needing percent-encoding is the case where a
        // hand-built path is most likely to diverge.
        api.admin().edit_user("od/d", &EditUserOption::default()).await.unwrap();
        send_patch(&api, "od/d", Map::new()).await.unwrap();

        let paths: Vec<String> = fake.calls().into_iter().map(|c| c.path).collect();
        assert_eq!(paths[0], paths[1], "generated and hand-built paths must agree");
    }

    /// Bug this prevents: `edit` with no flags issuing an empty PATCH, which succeeds and reports
    /// a change that did not happen.
    #[tokio::test]
    async fn edit_with_nothing_to_change_is_a_usage_error() {
        let api = testing::api(Arc::new(FakeTransport::new()));
        let mut buf: Vec<u8> = Vec::new();
        let globals = GlobalOpts::default();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let e = edit(&api, &mut emit, &blank_edit("ada")).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--admin"), "{e}");
    }

    /// `--admin` on `create` is two requests, because `CreateUserOption` has no `admin` field.
    /// Both must happen, in that order, and the second must carry only `admin`.
    #[tokio::test]
    async fn create_with_admin_promotes_in_a_second_request() {
        let fake = testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/admin/users",
            Canned::json(201, r#"{"login":"ada","is_admin":false}"#),
        );
        let fake = Arc::new(testing::on(
            fake,
            "PATCH",
            "/api/v1/admin/users/ada",
            Canned::json(200, r#"{"login":"ada","is_admin":true}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Create {
            username: "ada".into(),
            email: "ada@example.invalid".into(),
            password: Some("hunter2hunter2".into()),
            full_name: None,
            admin: true,
            restricted: false,
            no_must_change_password: false,
            must_change_password: false,
            send_notify: false,
            visibility: None,
            source_id: None,
            login_name: None,
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            create(&api, &mut emit, &args).await.unwrap();
        }

        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        let created: Value = serde_json::from_str(&calls[0].body_str()).unwrap();
        assert_eq!(created["username"], serde_json::json!("ada"));
        // The operator typed this password, so the account must be made to change it.
        assert_eq!(created["must_change_password"], serde_json::json!(true));
        assert_eq!(
            serde_json::from_str::<Value>(&calls[1].body_str()).unwrap(),
            serde_json::json!({"admin": true}),
            "the promotion must not carry anything else"
        );
    }

    /// Bug this prevents: prompting for a password in CI and hanging the job.
    #[test]
    fn a_missing_password_off_a_terminal_names_the_flag() {
        let mut buf: Vec<u8> = Vec::new();
        let globals = GlobalOpts::default();
        let emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let e = prompt_password(&emit).unwrap_err();
        assert!(e.to_string().contains("--password"), "{e}");
    }

    #[test]
    fn the_list_view_says_whether_an_account_can_sign_in() {
        let users: Vec<User> = serde_json::from_str(
            r#"[{"login":"root","email":"root@x","is_admin":true,"active":true},
                {"login":"ada","email":"ada@x","active":false},
                {"login":"eve","email":"eve@x","active":true,"prohibit_login":true,
                 "restricted":true}]"#,
        )
        .unwrap();
        let out =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::tty(90), |e| {
                e.many(&users, Some(3), "accounts", |table, users| {
                    table.headers(["LOGIN", "EMAIL", "ROLE", "STATE", "LAST LOGIN"]);
                    for u in users {
                        table.row([
                            u.login.clone(),
                            u.email.clone(),
                            if u.is_admin { "admin" } else { "user" }.to_owned(),
                            state_of(u),
                            String::new(),
                        ]);
                    }
                })
            });
        insta::assert_snapshot!(out);
    }
}

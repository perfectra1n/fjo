//! `gea user` — the authenticated user and other people: keys, tokens, follows, stars, watches.
//!
//! # `token create` is the one endpoint that refuses your token
//!
//! `POST /users/{username}/tokens` (and, on Gitea, the sibling list and delete routes) is gated
//! behind `reqBasicAuth()` on the server: a Bearer token is **not** accepted, no matter its scopes.
//! That is deliberate on Gitea's side — a token must not be able to mint another token — and it
//! shapes this module:
//!
//! * a second [`gitea_core::http::Client`] is built with HTTP Basic credentials for those three
//!   routes only, at the same base URL, with the same user agent and retry policy;
//! * the password is **never** a flag. It comes from a hidden prompt, or from stdin when there is
//!   no terminal (`printf '%s' "$PASS" | gea user token create ci`), so it cannot land in shell
//!   history, in `ps` output, or in a CI log;
//! * `--otp` (global) is passed through, because an account with 2FA enabled needs it here.
//!
//! `list` and `delete` try the ordinary token first and fall back to Basic **only** if the server
//! refuses — so on an instance that does allow a token, no password is ever asked for.
//!
//! # The `sha1` is shown once, and only once
//!
//! Gitea returns the token's plaintext exactly once, in the creation response; there is no route
//! that can ever show it again. So `token create` prints it as its *result* on stdout (which is
//! what makes `TOKEN=$(gea user token create ci --jq .sha1)` work) and says on stderr that this
//! was the only chance. It is never written to a log, a config file, or a keyring by this command:
//! storing it is `gea auth login`'s job, and doing it in two places would mean two places to leak
//! from.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::{Auth, Client, Credentials, RetryPolicy};
use gitea_core::types::{RepoRef, RepoSlug};
use gitea_model::{
    AccessToken, CreateAccessTokenOption, CreateGpgKeyOption, CreateKeyOption, GpgKey, PublicKey,
    Repository, User,
};

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// What `@me` means for a user-valued argument, per `docs/porcelain-conventions.md`.
const ME: &str = "@me";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Show a user; defaults to you
    View(ViewArgs),
    /// Search the instance's users
    List(ListArgs),
    /// SSH keys
    #[command(name = "ssh-key", subcommand)]
    SshKey(KeyCmd),
    /// GPG keys
    #[command(name = "gpg-key", subcommand)]
    GpgKey(GpgCmd),
    /// Access tokens (password authentication required)
    #[command(subcommand)]
    Token(TokenCmd),
    /// Follow a user
    Follow(UserArgs),
    /// Stop following a user
    Unfollow(UserArgs),
    /// Star a repository
    Star(RepoArgs),
    /// Unstar a repository
    Unstar(RepoArgs),
    /// List starred repositories
    Stars(StarsArgs),
    /// Watch a repository
    Watch(RepoArgs),
    /// Stop watching a repository
    Unwatch(RepoArgs),
}

#[derive(Debug, Subcommand)]
pub enum KeyCmd {
    /// List SSH keys
    List(KeyListArgs),
    /// Add an SSH key from a file, or from stdin with `-`
    Add(KeyAddArgs),
    /// Delete an SSH key by id
    Delete(KeyDeleteArgs),
}

#[derive(Debug, Subcommand)]
pub enum GpgCmd {
    /// List GPG keys
    List(KeyListArgs),
    /// Add an armoured GPG public key from a file, or from stdin with `-`
    Add(GpgAddArgs),
    /// Delete a GPG key by id
    Delete(KeyDeleteArgs),
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// List your access tokens
    List(TokenListArgs),
    /// Create an access token and print it once
    Create(TokenCreateArgs),
    /// Delete an access token by name or id
    Delete(TokenDeleteArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    /// The user, or `@me`
    #[arg(value_name = "USER")]
    pub user: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Match logins, names and emails against this text
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct KeyListArgs {
    /// Whose keys to list; defaults to you
    #[arg(value_name = "USER")]
    pub user: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct KeyAddArgs {
    /// A file containing one public key; `-` reads stdin
    #[arg(value_name = "FILE")]
    pub file: String,
    /// A label for the key; defaults to the key's own comment
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct GpgAddArgs {
    /// A file containing an armoured public key; `-` reads stdin
    #[arg(value_name = "FILE")]
    pub file: String,
    /// An armoured signature, when the instance asks you to prove the key is yours
    #[arg(long, value_name = "FILE")]
    pub signature: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct KeyDeleteArgs {
    /// The key's id, as printed by `list`
    #[arg(value_name = "ID")]
    pub id: i64,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct TokenListArgs {
    /// Whose tokens; defaults to you
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct TokenCreateArgs {
    /// A name for the token, so you can recognise it later
    #[arg(value_name = "NAME")]
    pub name: String,
    /// A scope, e.g. `read:repository`, `write:issue`, or `all`; repeatable
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Whose account to create it on; defaults to you
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct TokenDeleteArgs {
    /// The token's name or id, as printed by `list`
    #[arg(value_name = "NAME | ID")]
    pub token: String,
    /// Whose account; defaults to you
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct UserArgs {
    /// The user
    #[arg(value_name = "USER")]
    pub user: String,
}

#[derive(Debug, ClapArgs)]
pub struct RepoArgs {
    /// The repository as `owner/name`; defaults to the one you are in
    #[arg(value_name = "REPO")]
    pub repo: Option<RepoRef>,
}

#[derive(Debug, ClapArgs)]
pub struct StarsArgs {
    /// Whose stars; defaults to you
    #[arg(value_name = "USER")]
    pub user: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::View(a) => view(&rt, globals, &api, a).await,
            Cmd::List(a) => list(&rt, globals, &api, a).await,

            Cmd::SshKey(KeyCmd::List(a)) => ssh_list(&rt, globals, &api, a).await,
            Cmd::SshKey(KeyCmd::Add(a)) => ssh_add(&rt, globals, &api, a).await,
            Cmd::SshKey(KeyCmd::Delete(a)) => ssh_delete(&rt, &api, a).await,

            Cmd::GpgKey(GpgCmd::List(a)) => gpg_list(&rt, globals, &api, a).await,
            Cmd::GpgKey(GpgCmd::Add(a)) => gpg_add(&rt, globals, &api, a).await,
            Cmd::GpgKey(GpgCmd::Delete(a)) => gpg_delete(&rt, &api, a).await,

            Cmd::Token(TokenCmd::List(a)) => token_list(&rt, globals, &api, a).await,
            Cmd::Token(TokenCmd::Create(a)) => token_create(&rt, globals, &api, a).await,
            Cmd::Token(TokenCmd::Delete(a)) => token_delete(&rt, globals, &api, a).await,

            Cmd::Follow(a) => follow(&rt, &api, a, true).await,
            Cmd::Unfollow(a) => follow(&rt, &api, a, false).await,

            Cmd::Star(a) => star(&rt, globals, &api, a, true).await,
            Cmd::Unstar(a) => star(&rt, globals, &api, a, false).await,
            Cmd::Stars(a) => stars(&rt, globals, &api, a).await,

            Cmd::Watch(a) => watch(&rt, globals, &api, a, true).await,
            Cmd::Unwatch(a) => watch(&rt, globals, &api, a, false).await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::View(_) | Cmd::List(_) => Fields::Op("userGet"),
        Cmd::SshKey(KeyCmd::List(_)) | Cmd::SshKey(KeyCmd::Add(_)) => {
            Fields::Op("userCurrentListKeys")
        }
        Cmd::GpgKey(GpgCmd::List(_)) | Cmd::GpgKey(GpgCmd::Add(_)) => {
            Fields::Op("userCurrentListGPGKeys")
        }
        Cmd::Token(TokenCmd::List(_)) => Fields::Op("userGetTokens"),
        Cmd::Token(TokenCmd::Create(_)) => Fields::Op("userCreateToken"),
        Cmd::Stars(_) => Fields::Op("userCurrentListStarred"),
        _ => Fields::None,
    }
}

/// Resolve a user argument, treating `@me` and an absent value as the authenticated user.
///
/// One request, and only when it is needed: `gea user view ada` asks the server nothing extra.
async fn whom(api: &Api, given: Option<&str>) -> Result<String> {
    match given {
        Some(u) if u != ME => Ok(u.to_owned()),
        _ => Ok(api.user().get_current().await?.login),
    }
}

// ------------------------------------------------------------------------------ view / list

async fn view(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ViewArgs) -> Result<()> {
    let user = match args.user.as_deref() {
        Some(u) if u != ME => api.user().get(u).await?,
        _ => api.user().get_current().await?,
    };
    emit::detail(
        rt,
        globals,
        Fields::Op("userGet"),
        serde_json::to_value(&user).map_err(encode_failed)?,
        vec![
            ("login".to_owned(), user.login.clone()),
            ("name".to_owned(), user.full_name.clone()),
            ("id".to_owned(), user.id.to_string()),
            ("email".to_owned(), user.email.clone()),
            ("bio".to_owned(), user.description.clone()),
            ("location".to_owned(), user.location.clone()),
            ("website".to_owned(), user.website.clone()),
            ("followers".to_owned(), user.followers_count.to_string()),
            ("following".to_owned(), user.following_count.to_string()),
            ("starred".to_owned(), user.starred_repos_count.to_string()),
            ("admin".to_owned(), yes_no(user.is_admin)),
            ("visibility".to_owned(), user.visibility.to_string()),
            ("created".to_owned(), support::ago(user.created.as_ref())),
            ("last login".to_owned(), support::ago(user.last_login.as_ref())),
            ("url".to_owned(), user.html_url.clone()),
        ],
    )
}

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let q = query::UserSearchQuery {
        q: args.query.clone(),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    let users: Vec<User> = api.user().search(&q).await?.data;

    let listing = Listing {
        fields: Fields::Op("userGet"),
        value: serde_json::to_value(&users).map_err(encode_failed)?,
        count: users.len(),
        total: None,
        noun: "users",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["LOGIN", "NAME", "EMAIL", "ADMIN", "CREATED"]);
        for u in &users {
            t.row([
                u.login.clone(),
                u.full_name.clone(),
                u.email.clone(),
                yes_no(u.is_admin),
                support::ago(u.created.as_ref()),
            ]);
        }
    })
}

// -------------------------------------------------------------------------------- ssh keys

async fn ssh_list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &KeyListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let keys: Vec<PublicKey> = match args.user.as_deref() {
        // Another user's keys are public information and need no token; your own come from
        // `/user/keys`, which also reports keys you have not made public.
        Some(u) if u != ME => {
            let q = query::UserListKeysQuery::default();
            api.user().list_keys(u, &q).take(limit).try_collect().await?
        }
        _ => {
            let q = query::UserCurrentListKeysQuery::default();
            api.user().current_list_keys(&q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("userCurrentListKeys"),
        value: serde_json::to_value(&keys).map_err(encode_failed)?,
        count: keys.len(),
        total: None,
        noun: "SSH keys",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "TITLE", "TYPE", "FINGERPRINT", "ADDED"]);
        for k in &keys {
            t.row([
                k.id.to_string(),
                k.title.clone(),
                key_type(&k.key),
                k.fingerprint.clone(),
                support::ago(k.created_at.as_ref()),
            ]);
        }
    })
}

async fn ssh_add(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &KeyAddArgs) -> Result<()> {
    let material = read_key(&args.file)?;
    let key = ssh_key_body(&material, args.title.as_deref())?;
    let created = api.user().current_post_key(&key).await?;
    support::note(rt.term(), &format!("added SSH key {} ({})", created.id, created.title));
    emit::detail(
        rt,
        globals,
        Fields::Op("userCurrentListKeys"),
        serde_json::to_value(&created).map_err(encode_failed)?,
        vec![
            ("id".to_owned(), created.id.to_string()),
            ("title".to_owned(), created.title.clone()),
            ("fingerprint".to_owned(), created.fingerprint.clone()),
        ],
    )
}

/// Build the request body, deriving a title when none was given.
///
/// An OpenSSH public key is `<type> <base64> [comment]`, and the comment is almost always
/// `user@host` — a better label than anything a tool could invent, and the one the web UI shows
/// when you paste the same key. Only when there is no comment does this fall back.
fn ssh_key_body(material: &str, title: Option<&str>) -> Result<CreateKeyOption> {
    let key = material.trim();
    if key.is_empty() {
        return Err(support::usage(
            "no key material: pass a path to a `.pub` file, or `-` to read one from stdin"
                .to_owned(),
        ));
    }
    // The private-key check comes **first**, and the order is the point: a PEM file is also
    // multi-line, so checking line count first would answer "that looks like more than one key"
    // to someone who just tried to upload their private key — a message that hides the real
    // problem behind a plausible one.
    if key.starts_with("-----BEGIN") {
        return Err(support::usage(
            "this is a private key; nothing was sent. Use the matching .pub file.".to_owned(),
        ));
    }
    if key.lines().count() > 1 {
        return Err(support::usage(
            "that looks like more than one key: a public key is one line, \
             `ssh-ed25519 AAAA… you@host`. Add keys one at a time"
                .to_owned(),
        ));
    }
    let mut parts = key.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    if !kind.starts_with("ssh-") && !kind.starts_with("ecdsa-") && !kind.starts_with("sk-") {
        return Err(support::usage(format!(
            "{kind:?} is not an SSH key type; a public key line starts with ssh-ed25519, \
             ssh-rsa, ecdsa-sha2-nistp256, or sk-ssh-ed25519@openssh.com"
        )));
    }
    let comment = parts.nth(1).unwrap_or_default();
    Ok(CreateKeyOption {
        key: key.to_owned(),
        title: match title {
            Some(t) => t.to_owned(),
            None if !comment.is_empty() => comment.to_owned(),
            None => "added by gea".to_owned(),
        },
        // Only meaningful for deploy keys; a user key is never read-only.
        read_only: Some(false),
    })
}

async fn ssh_delete(rt: &Runtime, api: &Api, args: &KeyDeleteArgs) -> Result<()> {
    support::confirm_runtime(rt, args.yes, &format!("delete SSH key {}", args.id))?;
    api.user().current_delete_key(args.id).await?;
    support::note(rt.term(), &format!("deleted SSH key {}", args.id));
    Ok(())
}

// -------------------------------------------------------------------------------- gpg keys

async fn gpg_list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &KeyListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let keys: Vec<GpgKey> = match args.user.as_deref() {
        Some(u) if u != ME => {
            let q = query::UserListGpgKeysQuery::default();
            api.user().list_gpg_keys(u, &q).take(limit).try_collect().await?
        }
        _ => {
            let q = query::UserCurrentListGpgKeysQuery::default();
            api.user().current_list_gpg_keys(&q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("userCurrentListGPGKeys"),
        value: serde_json::to_value(&keys).map_err(encode_failed)?,
        count: keys.len(),
        total: None,
        noun: "GPG keys",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "KEY ID", "EMAILS", "CAN SIGN", "VERIFIED", "EXPIRES"]);
        for k in &keys {
            t.row([
                k.id.to_string(),
                k.key_id.clone(),
                k.emails.iter().map(|e| e.email.clone()).collect::<Vec<_>>().join(", "),
                yes_no(k.can_sign),
                yes_no(k.verified),
                support::ago(k.expires_at.as_ref()),
            ]);
        }
    })
}

async fn gpg_add(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &GpgAddArgs) -> Result<()> {
    let armored = read_key(&args.file)?;
    if !armored.contains("BEGIN PGP PUBLIC KEY BLOCK") {
        return Err(support::usage(
            "that does not look like an armoured PGP public key; export one with \
             `gpg --armor --export <key-id>`"
                .to_owned(),
        ));
    }
    let signature = match &args.signature {
        Some(path) => read_key(path)?,
        None => String::new(),
    };
    let created = api
        .user()
        .current_post_gpg_key(&CreateGpgKeyOption {
            armored_public_key: armored,
            armored_signature: Some(signature),
        })
        .await?;
    support::note(rt.term(), &format!("added GPG key {} ({})", created.id, created.key_id));
    emit::detail(
        rt,
        globals,
        Fields::Op("userCurrentListGPGKeys"),
        serde_json::to_value(&created).map_err(encode_failed)?,
        vec![
            ("id".to_owned(), created.id.to_string()),
            ("key id".to_owned(), created.key_id.clone()),
            ("verified".to_owned(), yes_no(created.verified)),
        ],
    )
}

async fn gpg_delete(rt: &Runtime, api: &Api, args: &KeyDeleteArgs) -> Result<()> {
    support::confirm_runtime(rt, args.yes, &format!("delete GPG key {}", args.id))?;
    api.user().current_delete_gpg_key(args.id).await?;
    support::note(rt.term(), &format!("deleted GPG key {}", args.id));
    Ok(())
}

// ---------------------------------------------------------------------------------- tokens

async fn token_list(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &TokenListArgs,
) -> Result<()> {
    let username = whom(api, args.username.as_deref()).await?;
    let limit = support::limit(None, globals);
    let q = query::UserGetTokensQuery::default();

    // The token first: if this instance allows it, nobody is asked for a password.
    let attempt: Result<Vec<AccessToken>> =
        api.user().get_tokens(&username, &q).take(limit).try_collect().await;
    let tokens = match attempt {
        Ok(t) => t,
        Err(e) if needs_basic_auth(&e) => {
            support::note(
                rt.term(),
                "this instance requires a password for the token routes (a token is not accepted \
                 there, by design)",
            );
            let basic = basic_api(rt, globals, &username)?;
            basic.user().get_tokens(&username, &q).take(limit).try_collect().await?
        }
        Err(e) => return Err(e),
    };

    let listing = Listing {
        fields: Fields::Op("userGetTokens"),
        value: serde_json::to_value(&tokens).map_err(encode_failed)?,
        count: tokens.len(),
        total: None,
        noun: "tokens",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "SCOPES", "LAST EIGHT", "CREATED"]);
        for tok in &tokens {
            t.row([
                tok.id.to_string(),
                tok.name.clone(),
                tok.scopes.join(", "),
                // The last eight characters are all Gitea keeps of the value, and they are what
                // lets you tell two tokens apart. Not a secret: they cannot be used to authenticate.
                tok.token_last_eight.clone(),
                support::ago(tok.created_at.as_ref()),
            ]);
        }
    })
}

async fn token_create(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &TokenCreateArgs,
) -> Result<()> {
    // Ask who we are *before* prompting for a password, so the prompt can name the account.
    let username = match args.username.as_deref() {
        Some(u) => u.to_owned(),
        None => whom(api, None).await.map_err(|e| {
            if needs_basic_auth(&e) || matches!(e.kind(), ErrorKind::NotAuthenticated { .. }) {
                support::usage(
                    "creating a token needs the account's name, and there is no working token to \
                     ask the server with; pass --username <user>"
                        .to_owned(),
                )
            } else {
                e
            }
        })?,
    };

    let basic = basic_api(rt, globals, &username)?;
    let body =
        CreateAccessTokenOption { name: args.name.clone(), scopes: Some(args.scopes.clone()) };
    let created = basic.user().create_token(&username, &body).await?;

    // The value, once. On stderr so that `--jq .sha1` (stdout) stays clean.
    support::note(rt.term(), "save this token now; Gitea will not show it again");
    if created.scopes.is_empty() {
        support::note(
            rt.term(),
            "note: no --scope was specified. Token access is limited, and scopes cannot be changed later.",
        );
    }
    emit::detail(
        rt,
        globals,
        Fields::Op("userCreateToken"),
        serde_json::to_value(&created).map_err(encode_failed)?,
        vec![
            ("name".to_owned(), created.name.clone()),
            ("id".to_owned(), created.id.to_string()),
            ("scopes".to_owned(), created.scopes.join(", ")),
            ("token".to_owned(), created.sha1.clone()),
        ],
    )
}

async fn token_delete(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &TokenDeleteArgs,
) -> Result<()> {
    let username = whom(api, args.username.as_deref()).await?;
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete the token {} from {}'s account", args.token, username),
    )?;
    // Same fallback as `list`: the route may or may not accept a token depending on the instance.
    match api.user().delete_access_token(&username, &args.token).await {
        Ok(()) => {}
        Err(e) if needs_basic_auth(&e) => {
            basic_api(rt, globals, &username)?
                .user()
                .delete_access_token(&username, &args.token)
                .await?;
        }
        Err(e) => return Err(e),
    }
    support::note(rt.term(), &format!("deleted the token {}", args.token));
    Ok(())
}

/// Whether the server refused because it wants HTTP Basic rather than a token.
///
/// Gitea's `reqBasicAuth()` answers 403 (and 401 when no credential was sent at all), so both
/// are treated as "try a password". A 404 is *not*: that means the user or the token is gone, and
/// prompting for a password would be a confusing way to say so.
fn needs_basic_auth(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::Forbidden { .. }
            | ErrorKind::TokenRejected { .. }
            | ErrorKind::NotAuthenticated { .. }
            | ErrorKind::InsufficientScope { .. }
    )
}

/// A second client, authenticated with a password, for the three token routes.
///
/// Same base URL, same user agent, same retry policy — only the credential differs. The password
/// is read here and dropped with the client: it is never stored, never logged, and never passed as
/// an argument (so it cannot appear in `ps` or in shell history).
fn basic_api(rt: &Runtime, globals: &GlobalOpts, username: &str) -> Result<Api> {
    let password = read_password(rt, username)?;
    let mut creds = Credentials::new(Auth::basic(username, password));
    if let Some(code) = &globals.otp {
        // An account with 2FA must send the code here too, or the server answers 401 with
        // "two-factor authentication required".
        creds = creds.with_otp(code);
    }
    let client = Client::builder(rt.client().web_base(), creds)
        .user_agent(crate::runtime::user_agent())
        .retry(RetryPolicy::none())
        .build()?;
    Ok(Api::new(client))
}

/// The password: a hidden prompt, or stdin when there is no terminal.
fn read_password(rt: &Runtime, username: &str) -> Result<String> {
    if support::can_prompt(rt) {
        return support::interact::secret(&format!("Password for {username}"));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .map_err(|e| support::usage(format!("could not read a password from stdin: {e}")))?;
    let password = crate::cmd::secret::strip_one_newline(&buf);
    if password.is_empty() {
        return Err(support::usage(format!(
            "this endpoint needs {username}'s password (a token is refused there by design), and \
             there is no terminal to ask on; pipe it in: printf '%s' \"$PASS\" | gea user token …"
        )));
    }
    Ok(password)
}

// ------------------------------------------------------------------- follow / star / watch

async fn follow(rt: &Runtime, api: &Api, args: &UserArgs, add: bool) -> Result<()> {
    if add {
        api.user().current_put_follow(&args.user).await?;
        support::note(rt.term(), &format!("following {}", args.user));
    } else {
        api.user().current_delete_follow(&args.user).await?;
        support::note(rt.term(), &format!("no longer following {}", args.user));
    }
    Ok(())
}

async fn star(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &RepoArgs,
    add: bool,
) -> Result<()> {
    let slug = target_repo(rt, globals, args)?;
    if add {
        api.user().current_put_star(&slug.owner, &slug.name).await?;
        support::note(rt.term(), &format!("starred {slug}"));
    } else {
        api.user().current_delete_star(&slug.owner, &slug.name).await?;
        support::note(rt.term(), &format!("unstarred {slug}"));
    }
    Ok(())
}

async fn stars(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &StarsArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let repos: Vec<Repository> = match args.user.as_deref() {
        Some(u) if u != ME => {
            let q = query::UserListStarredQuery::default();
            api.user().list_starred(u, &q).take(limit).try_collect().await?
        }
        _ => {
            let q = query::UserCurrentListStarredQuery::default();
            api.user().current_list_starred(&q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("userCurrentListStarred"),
        value: serde_json::to_value(&repos).map_err(encode_failed)?,
        count: repos.len(),
        total: None,
        noun: "starred repositories",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["REPOSITORY", "STARS", "DESCRIPTION", "UPDATED"]);
        for r in &repos {
            t.row([
                r.full_name.clone(),
                r.stars_count.to_string(),
                r.description.clone(),
                support::ago(r.updated_at.as_ref()),
            ]);
        }
    })
}

async fn watch(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &RepoArgs,
    add: bool,
) -> Result<()> {
    let slug = target_repo(rt, globals, args)?;
    if add {
        let info = api.user().current_put_subscription(&slug.owner, &slug.name).await?;
        support::note(
            rt.term(),
            &format!(
                "watching {slug}{}",
                if info.subscribed {
                    String::new()
                } else {
                    " (the server reports otherwise)".to_owned()
                }
            ),
        );
    } else {
        api.user().current_delete_subscription(&slug.owner, &slug.name).await?;
        support::note(rt.term(), &format!("no longer watching {slug}"));
    }
    Ok(())
}

/// The repository a star/watch acts on: the argument, then `-R`, then the checkout.
fn target_repo(rt: &Runtime, globals: &GlobalOpts, args: &RepoArgs) -> Result<RepoSlug> {
    match &args.repo {
        Some(r) => Ok(r.slug.clone()),
        None => Ok(rt.repo(globals)?.slug.clone()),
    }
}

/// The type prefix of an OpenSSH public key, for the listing's TYPE column.
fn key_type(key: &str) -> String {
    key.split_whitespace().next().unwrap_or_default().to_owned()
}

fn yes_no(v: bool) -> String {
    if v { "yes".to_owned() } else { String::new() }
}

fn read_key(path: &str) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| support::usage(format!("could not read the key from stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).map_err(|e| support::usage(format!("{path}: {e}")))
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth as HttpAuth, Client as HttpClient, FakeTransport};
    use gitea_core::types::ids::KeyId;
    use std::sync::Arc;

    fn api_for(fake: Arc<FakeTransport>) -> Api {
        Api::new(
            HttpClient::builder("https://git.example.org", HttpAuth::token("t"))
                .transport(fake)
                .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
                .probe_404(false)
                .build()
                .expect("a well-formed base URL"),
        )
    }

    /// `@me` and an absent user both mean the authenticated user, and a named user costs no extra
    /// request. Bug this prevents: `gea user view @me` looking up a user literally called `@me`.
    #[tokio::test]
    async fn at_me_resolves_to_the_authenticated_user() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/user",
            Canned::json(200, r#"{"login":"ada"}"#),
        ));
        let api = api_for(fake.clone());
        assert_eq!(whom(&api, Some(ME)).await.unwrap(), "ada");
        assert_eq!(whom(&api, None).await.unwrap(), "ada");
        assert_eq!(whom(&api, Some("grace")).await.unwrap(), "grace");
        // Two lookups for `@me`/None, none for a named user.
        assert_eq!(fake.call_count(), 2);
    }

    /// Bug this prevents — and the reason this check is worth the lines: pointing `ssh-key add` at
    /// `~/.ssh/id_ed25519` instead of `id_ed25519.pub` and uploading a **private** key.
    #[test]
    fn a_private_key_is_refused_and_never_sent() {
        let e = ssh_key_body("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blb…\n", None).unwrap_err();
        assert!(e.to_string().contains("private key"), "{e}");
        assert!(e.to_string().contains("nothing was sent"), "{e}");
    }

    /// The title defaults to the key's own comment, which is what the web UI shows for the same
    /// paste — and is more useful than a generated label.
    #[test]
    fn a_key_title_defaults_to_the_keys_comment() {
        let body = ssh_key_body("ssh-ed25519 AAAAC3Nz ada@thinkpad", None).unwrap();
        assert_eq!(body.title, "ada@thinkpad");
        assert_eq!(body.read_only, Some(false));

        let body = ssh_key_body("ssh-ed25519 AAAAC3Nz ada@thinkpad", Some("laptop")).unwrap();
        assert_eq!(body.title, "laptop");

        // No comment at all still produces something recognisable.
        let body = ssh_key_body("ssh-rsa AAAAB3Nz", None).unwrap();
        assert_eq!(body.title, "added by gea");
    }

    #[test]
    fn a_file_with_several_keys_or_no_key_is_refused() {
        assert!(ssh_key_body("", None).is_err());
        assert!(ssh_key_body("ssh-ed25519 A a\nssh-ed25519 B b\n", None).is_err());
        let e = ssh_key_body("hello world", None).unwrap_err();
        assert!(e.to_string().contains("not an SSH key type"), "{e}");
    }

    /// The token routes are the one place a Bearer token is refused. Bug this prevents: prompting
    /// for a password on a 404 (the user does not exist) or on a rate limit, neither of which a
    /// password would fix.
    #[test]
    fn only_an_auth_refusal_triggers_the_password_fallback() {
        assert!(needs_basic_auth(&Error::new(ErrorKind::Forbidden {
            server_message: "Only signed in user is allowed to call APIs".to_owned(),
        })));
        assert!(needs_basic_auth(&Error::new(ErrorKind::NotAuthenticated {
            host: "git.example.org".to_owned(),
        })));
        // A missing user, or a rate limit, is not something a password fixes.
        assert!(!needs_basic_auth(&Error::new(ErrorKind::ResourceNotFound {
            kind: "token",
            id: "ci".to_owned(),
            slug: None,
            // Discovered locally: there was no server reply to quote.
            server_message: None,
        })));
        assert!(!needs_basic_auth(&Error::new(ErrorKind::RateLimited {
            host: "git.example.org".to_owned(),
            retry_after: None,
        })));
    }

    /// `token create` prints the value once and says so. Bug this prevents: the `sha1` being
    /// dropped from the output (Gitea can never show it again), or being written to stderr where
    /// `$(…)` cannot capture it.
    #[tokio::test]
    async fn a_created_token_reports_its_value_on_stdout() {
        let created = AccessToken {
            id: 3,
            name: "ci".into(),
            sha1: "0123456789abcdef".into(),
            token_last_eight: "89abcdef".into(),
            scopes: vec!["write:repository".into()],
            ..Default::default()
        };
        let mut buf = Vec::new();
        emit::detail_to(
            &mut buf,
            &Term::piped(),
            &GlobalOpts::default(),
            Fields::Op("userCreateToken"),
            serde_json::to_value(&created).unwrap(),
            vec![
                ("name".to_owned(), created.name.clone()),
                ("token".to_owned(), created.sha1.clone()),
            ],
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("0123456789abcdef"), "{out}");
        // And `--jq .sha1` is the scriptable form, which requires the field to survive projection.
        let mut buf = Vec::new();
        emit::detail_to(
            &mut buf,
            &Term::piped(),
            &GlobalOpts { jq: Some(".sha1".into()), ..Default::default() },
            Fields::Op("userCreateToken"),
            serde_json::to_value(&created).unwrap(),
            vec![],
        )
        .unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "0123456789abcdef\n");
    }

    #[test]
    fn ssh_key_list_output_goldens() {
        let keys = vec![
            PublicKey {
                id: KeyId::new(4),
                title: "ada@thinkpad".into(),
                key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 ada@thinkpad".into(),
                fingerprint: "SHA256:abcdef".into(),
                ..Default::default()
            },
            PublicKey {
                id: KeyId::new(9),
                title: "ci".into(),
                key: "ssh-rsa AAAAB3NzaC1yc2E".into(),
                fingerprint: "SHA256:123456".into(),
                ..Default::default()
            },
        ];
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(90), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts { json: Some("id,title,fingerprint".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("userCurrentListKeys"),
                value: serde_json::to_value(&keys).unwrap(),
                count: keys.len(),
                total: None,
                noun: "SSH keys",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["ID", "TITLE", "TYPE", "FINGERPRINT", "ADDED"]);
                for k in &keys {
                    t.row([
                        k.id.to_string(),
                        k.title.clone(),
                        key_type(&k.key),
                        k.fingerprint.clone(),
                        String::new(),
                    ]);
                }
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

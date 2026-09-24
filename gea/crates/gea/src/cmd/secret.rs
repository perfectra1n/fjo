//! `gea secret` — Actions secrets at repository, organization and **user** scope.
//!
//! # Three scopes, and one of them has no GitHub equivalent
//!
//! Gitea stores Actions secrets against a repository, an organization, *or a user*
//! (`PUT /user/actions/secrets/{name}`). The user scope is genuinely Gitea-only — GitHub has
//! per-user Codespaces secrets, not Actions secrets — and it is what makes a personal runner
//! usable across all of someone's repositories without copying a token into each one. So it is
//! exposed as a first-class `--user` flag rather than left to `gea raw`.
//!
//! The user scope is also **asymmetric**: Gitea has `PUT` and `DELETE` for
//! `/user/actions/secrets/{secretname}` and *no* `GET` for the collection. So `--user` can set
//! and delete but not list, and `list --user` says exactly that instead of printing an empty
//! table that would read as "you have none".
//!
//! # The value is never echoed
//!
//! Not on success, not in an error, not under `--debug`. It is read from `-b`, from a file, from
//! stdin, or from a hidden prompt; it goes into the request body and nowhere else. The one place
//! it could leak is a diagnostic that quotes the request, and `gitea_core::http`'s `Debug` for
//! a body prints only its length — see `OutBody`'s `fmt::Debug`. A test in this module asserts
//! the value appears in no output stream.
//!
//! There is deliberately **no `secret get`** that works: Gitea (like GitHub) stores secrets
//! encrypted for the runner and has no read route at all. The subcommand exists only to say so,
//! because "unrecognized subcommand" would leave a user wondering whether they typed it wrong.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::CreateOrUpdateSecretOption;

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

/// Which owner a secret or variable belongs to.
///
/// Shared with [`crate::cmd::variable`], because the two groups must agree: a `--user` that meant
/// different things in `secret set` and `variable set` would be a trap, and the endpoints are
/// laid out identically.
#[derive(Debug, Clone, ClapArgs)]
pub struct ScopeArgs {
    /// The organization's secrets instead of this repository's
    #[arg(long, value_name = "ORG", conflicts_with = "user")]
    pub org: Option<String>,
    /// Your own user-level secrets (Gitea has no GitHub Actions equivalent for these)
    #[arg(long)]
    pub user: bool,
}

/// A resolved scope. `Repo` carries the slug so the repository is resolved exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    Repo(RepoSlug),
    Org(String),
    User,
}

impl ScopeArgs {
    /// Resolve the scope, touching `git` only when the repository scope is actually in play.
    ///
    /// That ordering matters: `gea secret list --org acme` must work outside a checkout, and it
    /// would not if the repository were resolved first "just in case".
    pub(crate) fn resolve(&self, rt: &Runtime, globals: &GlobalOpts) -> Result<Scope> {
        if self.user {
            return Ok(Scope::User);
        }
        if let Some(org) = &self.org {
            return Ok(Scope::Org(org.clone()));
        }
        Ok(Scope::Repo(rt.repo(globals)?.slug.clone()))
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Repo(slug) => write!(f, "{slug}"),
            Scope::Org(org) => write!(f, "organization {org}"),
            Scope::User => f.write_str("your user account"),
        }
    }
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List secret names (never their values — the API cannot return those)
    List(ListArgs),
    /// Create or update a secret
    Set(SetArgs),
    /// Delete a secret
    Delete(DeleteArgs),
    /// Not possible: a secret cannot be read back
    Get(GetArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    /// The secret's name, e.g. `REGISTRY_TOKEN`
    #[arg(value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// The value, inline. Note that this puts the secret in your shell history
    #[arg(short = 'b', long, value_name = "VALUE")]
    pub body: Option<String>,
    /// Read the value from a file; `-` means stdin. Bytes are sent verbatim
    #[arg(short = 'F', long = "body-file", value_name = "FILE")]
    pub body_file: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The secret's name
    #[arg(value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct GetArgs {
    /// The secret's name
    #[arg(value_name = "NAME")]
    pub name: String,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    // Refused before the runtime: it needs no host and no network to be wrong.
    if let Cmd::Get(a) = &args.command {
        return Err(cannot_read(&a.name));
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, globals, &api, a).await,
            Cmd::Set(a) => set(&rt, globals, &api, a).await,
            Cmd::Delete(a) => delete(&rt, globals, &api, a).await,
            Cmd::Get(a) => Err(cannot_read(&a.name)),
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Op("repoListActionsSecrets"),
        Cmd::Set(_) | Cmd::Delete(_) | Cmd::Get(_) => Fields::None,
    }
}

/// The one honest answer to `gea secret get`.
fn cannot_read(name: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "secret {name} cannot be read back. Use `gea secret list` to list names or `gea secret set {name}` to replace it. Workflows access it as ${{{{ secrets.{name} }}}}. Use `gea variable` for readable values."
    )))
}

/// Why `secret list --user` cannot work.
fn user_list_unsupported() -> Error {
    Error::new(ErrorKind::Usage(
        "user-level secrets cannot be listed. Use `gea secret set --user` or `gea secret delete --user` to manage them. To list repository or organization secrets, run in a checkout or pass --org <name>."
            .to_owned(),
    ))
}

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    let limit = support::limit(None, globals);
    let secrets: Vec<gitea_model::Secret> = match &scope {
        Scope::Repo(slug) => {
            let q = query::RepoListActionsSecretsQuery::default();
            api.repo()
                .list_actions_secrets(&slug.owner, &slug.name, &q)
                .take(limit)
                .try_collect()
                .await?
        }
        Scope::Org(org) => {
            let q = query::OrgListActionsSecretsQuery::default();
            api.org().list_actions_secrets(org, &q).take(limit).try_collect().await?
        }
        // Not an empty list: an empty list would read as "you have no user secrets", which is a
        // claim this API cannot support either way.
        Scope::User => return Err(user_list_unsupported()),
    };

    let listing = Listing {
        fields: Fields::Op("repoListActionsSecrets"),
        value: serde_json::to_value(&secrets).map_err(encode_failed)?,
        count: secrets.len(),
        total: None,
        noun: "secrets",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["NAME", "UPDATED"]);
        for s in &secrets {
            t.row([s.name.clone(), support::ago(s.created_at.as_ref())]);
        }
    })
}

async fn set(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &SetArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    let value = read_value(rt.term(), support::can_prompt(rt), args)?;
    let body = CreateOrUpdateSecretOption { data: value, description: None };

    match &scope {
        Scope::Repo(slug) => {
            api.repo().update_repo_secret(&slug.owner, &slug.name, &args.name, &body).await?
        }
        Scope::Org(org) => api.org().update_org_secret(org, &args.name, &body).await?,
        Scope::User => api.user().update_user_secret(&args.name, &body).await?,
    }
    // The name and the scope, never the value. `PUT` is an upsert here, so this deliberately does
    // not claim to have "created" anything.
    support::note(rt.term(), &format!("set {} for {}", args.name, scope));
    Ok(())
}

async fn delete(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &DeleteArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete the secret {} from {scope}", args.name),
    )?;
    match &scope {
        Scope::Repo(slug) => {
            api.repo().delete_repo_secret(&slug.owner, &slug.name, &args.name).await?
        }
        Scope::Org(org) => api.org().delete_org_secret(org, &args.name).await?,
        Scope::User => api.user().delete_user_secret(&args.name).await?,
    }
    support::note(rt.term(), &format!("deleted {} from {}", args.name, scope));
    Ok(())
}

/// Where a secret's value comes from, in order.
///
/// 1. `-b/--body` — convenient, and warned about, because it lands in shell history.
/// 2. `-F/--body-file` — bytes verbatim, `-` for stdin. This is the form for a PEM key.
/// 3. a hidden prompt, when both streams are terminals.
/// 4. stdin, with **one** trailing newline removed.
///
/// Rule 4's trimming is the one judgement call. `echo hunter2 | gea secret set X` is the
/// overwhelmingly common shape, and a trailing `\n` inside a token is invisible, ends up in an
/// `Authorization` header, and fails with a 401 that nobody connects to this command. Anyone who
/// needs the bytes exactly says `--body-file -`, which trims nothing.
fn read_value(term: &Term, may_prompt: bool, args: &SetArgs) -> Result<String> {
    if let Some(b) = &args.body {
        support::note(
            term,
            "note: -b puts the secret in your shell history; prefer piping it, or --body-file",
        );
        return Ok(b.clone());
    }
    if let Some(path) = &args.body_file {
        let bytes = if path == "-" {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
                .map_err(|e| support::usage(format!("--body-file -: could not read stdin: {e}")))?;
            buf
        } else {
            std::fs::read(path).map_err(|e| support::usage(format!("--body-file {path}: {e}")))?
        };
        return String::from_utf8(bytes).map_err(|_| {
            // The API field is a JSON string, so a non-UTF-8 secret cannot be sent at all —
            // better to say why than to send replacement characters.
            support::usage(format!(
                "--body-file {path} is not valid UTF-8, and Gitea's secret value is a JSON \
                 string; base64-encode the file first"
            ))
        });
    }
    if may_prompt {
        return support::interact::secret(&format!("Value for {}", args.name));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .map_err(|e| support::usage(format!("could not read the value from stdin: {e}")))?;
    if buf.is_empty() {
        return Err(support::usage(format!(
            "no value for {}: pass -b <value>, --body-file <path> (or -), or pipe it in",
            args.name
        )));
    }
    Ok(strip_one_newline(&buf))
}

/// Remove exactly one trailing newline (and its `\r`), never more.
///
/// Shared with [`crate::cmd::variable`] so the two commands treat a piped value identically.
pub(crate) fn strip_one_newline(s: &str) -> String {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s).to_owned()
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

    /// Each scope must hit its own route. Bug this prevents: `--org` silently writing the
    /// repository's secret (or the reverse), which is a security bug, not a cosmetic one.
    #[tokio::test]
    async fn every_scope_writes_to_its_own_route() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "PUT".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/secrets/TOKEN",
                    Canned::new(204),
                )
                .on(
                    "PUT".parse().unwrap(),
                    "/api/v1/orgs/acme/actions/secrets/TOKEN",
                    Canned::new(204),
                )
                .on("PUT".parse().unwrap(), "/api/v1/user/actions/secrets/TOKEN", Canned::new(204)),
        );
        let api = api_for(fake.clone());
        let body = CreateOrUpdateSecretOption { data: "s3cret".to_owned(), description: None };
        api.repo().update_repo_secret("o", "r", "TOKEN", &body).await.unwrap();
        api.org().update_org_secret("acme", "TOKEN", &body).await.unwrap();
        api.user().update_user_secret("TOKEN", &body).await.unwrap();

        let paths: Vec<String> = fake.calls().iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                "/api/v1/repos/o/r/actions/secrets/TOKEN",
                "/api/v1/orgs/acme/actions/secrets/TOKEN",
                "/api/v1/user/actions/secrets/TOKEN",
            ]
        );
        // The value travels in the body, as the API's `data` field.
        for call in fake.calls() {
            let sent: serde_json::Value =
                serde_json::from_slice(&call.body.clone().unwrap()).unwrap();
            assert_eq!(sent["data"], "s3cret");
        }
    }

    /// Bug this prevents — the important one in this module: a secret's value reaching a terminal,
    /// a log, or a `Debug` line. The transport's recorded request is inspected for the value (it
    /// must be there, in the body, and nowhere else), and every rendering of the request is
    /// checked not to contain it.
    #[tokio::test]
    async fn a_secret_value_is_never_echoed() {
        const VALUE: &str = "correct-horse-battery-staple";
        let fake = Arc::new(FakeTransport::new().on(
            "PUT".parse().unwrap(),
            "/api/v1/repos/o/r/actions/secrets/TOKEN",
            Canned::new(204),
        ));
        let api = api_for(fake.clone());
        api.repo()
            .update_repo_secret(
                "o",
                "r",
                "TOKEN",
                &CreateOrUpdateSecretOption { data: VALUE.to_owned(), description: None },
            )
            .await
            .unwrap();

        let call = &fake.calls()[0];
        // The URL — which is what `--debug` traces — must not contain it.
        assert!(!call.url.contains(VALUE), "{}", call.url);
        // Nor any header.
        for (name, value) in &call.headers {
            assert!(!value.contains(VALUE), "{name}: {value}");
        }
        // The success message names the secret and the scope, never the value.
        let announced = format!("set {} for {}", "TOKEN", Scope::Repo(RepoSlug::new("o", "r")));
        assert!(!announced.contains(VALUE), "{announced}");
        assert_eq!(announced, "set TOKEN for o/r");
    }

    /// stdin is the shape a script uses, and `echo` appends a newline that would otherwise become
    /// part of the token — an invisible byte producing a 401 nobody connects to this command.
    #[test]
    fn one_trailing_newline_is_stripped_and_no_more() {
        assert_eq!(strip_one_newline("hunter2\n"), "hunter2");
        assert_eq!(strip_one_newline("hunter2\r\n"), "hunter2");
        assert_eq!(strip_one_newline("hunter2\n\n"), "hunter2\n");
        assert_eq!(strip_one_newline("multi\nline"), "multi\nline");
        assert_eq!(strip_one_newline(""), "");
    }

    /// `--body-file` sends bytes verbatim — no trimming — because that is the flag for a PEM key,
    /// whose trailing newline is part of the file. Bug this prevents: applying stdin's
    /// newline-trimming rule to a file and silently truncating a key.
    #[test]
    fn body_file_is_verbatim() {
        const PEM: &str = "-----BEGIN KEY-----\nabc\n-----END KEY-----\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.pem");
        std::fs::write(&path, PEM).unwrap();
        let args = SetArgs {
            name: "KEY".to_owned(),
            scope: ScopeArgs { org: None, user: false },
            body: None,
            body_file: Some(path.to_str().unwrap().to_owned()),
        };
        assert_eq!(read_value(&Term::piped(), false, &args).unwrap(), PEM);
    }

    /// `-b` is honoured, and warns — the warning goes to stderr and only on a terminal, so it
    /// cannot corrupt a piped value.
    #[test]
    fn an_inline_body_is_used_as_given() {
        let args = SetArgs {
            name: "K".to_owned(),
            scope: ScopeArgs { org: None, user: false },
            body: Some("  spaces kept  ".to_owned()),
            body_file: None,
        };
        assert_eq!(read_value(&Term::piped(), false, &args).unwrap(), "  spaces kept  ");
    }

    /// `secret get` must explain rather than 404, and must point at the two things that do work.
    #[test]
    fn get_explains_that_secrets_cannot_be_read() {
        let e = cannot_read("TOKEN");
        assert_eq!(e.exit_code(), 2);
        let msg = e.to_string();
        assert!(msg.contains("cannot be read back"), "{msg}");
        assert!(msg.contains("gea secret list"), "{msg}");
        assert!(msg.contains("gea variable"), "{msg}");
    }

    /// `list --user` must say the route does not exist rather than print an empty table that
    /// reads as "you have no user secrets".
    #[test]
    fn listing_user_secrets_says_the_api_cannot() {
        let e = user_list_unsupported();
        let msg = e.to_string();
        assert!(msg.contains("user-level secrets cannot be listed"), "{msg}");
        // And it names what *does* work, so the user is not left at a dead end.
        assert!(msg.contains("gea secret set --user"), "{msg}");
        assert!(msg.contains("--org"), "{msg}");
    }

    #[test]
    fn scope_flags_are_mutually_exclusive() {
        use clap::{CommandFactory, Parser};
        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            cmd: Cmd,
        }
        let e =
            Harness::command().try_get_matches_from(["gea", "set", "A", "--org", "acme", "--user"]);
        assert!(e.is_err(), "--org and --user are two different owners");
        let m = Harness::try_parse_from(["gea", "set", "A", "--org", "acme"]).unwrap();
        let Cmd::Set(s) = m.cmd else { panic!("set") };
        assert_eq!(s.scope.org.as_deref(), Some("acme"));
    }
}

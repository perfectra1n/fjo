//! `gea variable` — Actions variables at repository, organization and user scope.
//!
//! The sibling of [`crate::cmd::secret`], and deliberately sharing its `--org`/`--user` scope
//! flags (`secret::ScopeArgs`) so the two cannot drift: `--user` naming a different owner in
//! `secret set` than in `variable set` would be the kind of difference nobody notices until a
//! value lands somewhere it should not.
//!
//! Two things differ from secrets, both because a variable is *not* a secret:
//!
//! * **`get` works.** `GET …/actions/variables/{name}` returns the value, so there is a `get`
//!   here and a refusal in `secret`.
//! * **`set` is an upsert built from two routes.** Gitea splits create (`POST`) from update
//!   (`PUT`) and answers 400/404 respectively when you pick the wrong one, while `gh variable
//!   set` is idempotent. So `set` reads the variable first and then chooses. That costs one extra
//!   GET and removes an error nobody can act on.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::{ActionVariable, CreateVariableOption, UpdateVariableOption};

use crate::cmd::secret::{Scope, ScopeArgs};
use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List variables and their values
    List(ListArgs),
    /// Print one variable's value
    Get(GetArgs),
    /// Create or update a variable
    Set(SetArgs),
    /// Delete a variable
    Delete(DeleteArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct GetArgs {
    /// The variable's name
    #[arg(value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
}

#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    /// The variable's name
    #[arg(value_name = "NAME")]
    pub name: String,
    /// The value. Omit it to read one from stdin
    #[arg(value_name = "VALUE")]
    pub value: Option<String>,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Read the value from a file; `-` means stdin
    #[arg(short = 'F', long = "body-file", value_name = "FILE", conflicts_with = "value")]
    pub body_file: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The variable's name
    #[arg(value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub scope: ScopeArgs,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
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
            Cmd::Get(a) => get(&rt, globals, &api, a).await,
            Cmd::Set(a) => set(&rt, globals, &api, a).await,
            Cmd::Delete(a) => delete(&rt, globals, &api, a).await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Op("getRepoVariablesList"),
        Cmd::Get(_) => Fields::Op("getRepoVariable"),
        Cmd::Set(_) | Cmd::Delete(_) => Fields::None,
    }
}

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    let limit = support::limit(None, globals);
    let vars: Vec<ActionVariable> = match &scope {
        Scope::Repo(slug) => {
            let q = query::GetRepoVariablesListQuery::default();
            api.repo()
                .get_repo_variables_list(&slug.owner, &slug.name, &q)
                .take(limit)
                .try_collect()
                .await?
        }
        Scope::Org(org) => {
            let q = query::GetOrgVariablesListQuery::default();
            api.org().get_org_variables_list(org, &q).take(limit).try_collect().await?
        }
        Scope::User => {
            let q = query::GetUserVariablesListQuery::default();
            api.user().get_user_variables_list(&q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("getRepoVariablesList"),
        value: serde_json::to_value(&vars).map_err(encode_failed)?,
        count: vars.len(),
        total: None,
        noun: "variables",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["NAME", "VALUE"]);
        for v in &vars {
            t.row([v.name.clone(), v.data.clone()]);
        }
    })
}

async fn get(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &GetArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    let var = fetch(api, &scope, &args.name).await?;
    // Two columns rather than the bare value, because `--jq .data` (or `--json data`) is the
    // scriptable form and a human wants to see which name they asked for.
    emit::detail(
        rt,
        globals,
        Fields::Op("getRepoVariable"),
        serde_json::to_value(&var).map_err(encode_failed)?,
        vec![("name".to_owned(), var.name.clone()), ("value".to_owned(), var.data.clone())],
    )
}

async fn set(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &SetArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    let value = read_value(args)?;

    // Upsert. Gitea has two routes and no idempotent one, so ask first: `POST` on an existing
    // variable answers "variable already exists" and `PUT` on a missing one answers 404, and
    // neither message tells the user anything they can act on.
    let exists = fetch(api, &scope, &args.name).await.is_ok();
    if exists {
        let body = UpdateVariableOption { name: Some(args.name.clone()), value, description: None };
        match &scope {
            Scope::Repo(slug) => {
                api.repo().update_repo_variable(&slug.owner, &slug.name, &args.name, &body).await?
            }
            Scope::Org(org) => api.org().update_org_variable(org, &args.name, &body).await?,
            Scope::User => api.user().update_user_variable(&args.name, &body).await?,
        }
    } else {
        let body = CreateVariableOption { value, description: None };
        match &scope {
            Scope::Repo(slug) => {
                api.repo().create_repo_variable(&slug.owner, &slug.name, &args.name, &body).await?
            }
            Scope::Org(org) => api.org().create_org_variable(org, &args.name, &body).await?,
            Scope::User => api.user().create_user_variable(&args.name, &body).await?,
        }
    }
    support::note(
        rt.term(),
        &format!("{} {} for {scope}", if exists { "updated" } else { "created" }, args.name),
    );
    Ok(())
}

async fn delete(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &DeleteArgs) -> Result<()> {
    let scope = args.scope.resolve(rt, globals)?;
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete the variable {} from {scope}", args.name),
    )?;
    match &scope {
        Scope::Repo(slug) => {
            api.repo().delete_repo_variable(&slug.owner, &slug.name, &args.name).await?
        }
        Scope::Org(org) => api.org().delete_org_variable(org, &args.name).await?,
        Scope::User => api.user().delete_user_variable(&args.name).await?,
    }
    support::note(rt.term(), &format!("deleted {} from {}", args.name, scope));
    Ok(())
}

async fn fetch(api: &Api, scope: &Scope, name: &str) -> Result<ActionVariable> {
    match scope {
        Scope::Repo(slug) => api.repo().get_repo_variable(&slug.owner, &slug.name, name).await,
        Scope::Org(org) => api.org().get_org_variable(org, name).await,
        Scope::User => api.user().get_user_variable(name).await,
    }
}

/// The value: the positional argument, a file, or stdin.
///
/// Unlike a secret this is not sensitive, so there is no hidden prompt — and unlike a secret it
/// takes a positional `VALUE`, matching `gh variable set NAME VALUE`.
fn read_value(args: &SetArgs) -> Result<String> {
    if let Some(v) = &args.value {
        return Ok(v.clone());
    }
    let bytes = match args.body_file.as_deref() {
        Some("-") | None => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
                .map_err(|e| support::usage(format!("could not read the value from stdin: {e}")))?;
            buf
        }
        Some(path) => {
            std::fs::read(path).map_err(|e| support::usage(format!("--body-file {path}: {e}")))?
        }
    };
    if bytes.is_empty() {
        return Err(support::usage(format!(
            "no value for {}: pass it as an argument, with --body-file <path>, or on stdin",
            args.name
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| support::usage("a variable's value must be valid UTF-8".to_owned()))?;
    // One trailing newline removed, by the same rule as `secret set`: `echo x | …` is the common
    // shape, and the newline would otherwise become part of the value.
    Ok(crate::cmd::secret::strip_one_newline(&text))
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::RepoSlug;
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

    fn set_args(value: Option<&str>) -> SetArgs {
        SetArgs {
            name: "REGISTRY".to_owned(),
            value: value.map(str::to_owned),
            scope: ScopeArgs { org: None, user: false },
            body_file: None,
        }
    }

    /// Bug this prevents: `variable set` on an existing variable failing with "variable already
    /// exists" because it always POSTs. `gh variable set` is an upsert and scripts assume it.
    #[tokio::test]
    async fn set_updates_when_the_variable_exists_and_creates_when_it_does_not() {
        let existing = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/REGISTRY",
                    Canned::json(200, r#"{"name":"REGISTRY","data":"old"}"#),
                )
                .on(
                    "PUT".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/REGISTRY",
                    Canned::new(204),
                ),
        );
        let api = api_for(existing.clone());
        let scope = Scope::Repo(RepoSlug::new("o", "r"));
        assert!(fetch(&api, &scope, "REGISTRY").await.is_ok());
        api.repo()
            .update_repo_variable(
                "o",
                "r",
                "REGISTRY",
                &UpdateVariableOption {
                    name: Some("REGISTRY".to_owned()),
                    value: "new".to_owned(),
                    description: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(existing.calls().last().unwrap().method, "PUT");

        let missing = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/REGISTRY",
                    Canned::json(404, r#"{"message":"variable not found"}"#),
                )
                .on(
                    "POST".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/REGISTRY",
                    Canned::new(201),
                ),
        );
        let api = api_for(missing.clone());
        assert!(fetch(&api, &scope, "REGISTRY").await.is_err());
        api.repo()
            .create_repo_variable(
                "o",
                "r",
                "REGISTRY",
                &CreateVariableOption { value: "new".to_owned(), description: None },
            )
            .await
            .unwrap();
        assert_eq!(missing.calls().last().unwrap().method, "POST");
    }

    /// Each scope has its own three routes, and `--user` is the one with no GitHub equivalent.
    /// Bug this prevents: `variable delete` reporting "unexpected API response format" for a
    /// delete that succeeded. Gitea's spec declares an `ActionVariable` body on the repository and
    /// organization deletes, and the handler answers `204` with nothing — so decoding the declared
    /// type failed after the variable was already gone.
    #[tokio::test]
    async fn a_delete_answered_with_no_content_is_a_success_at_every_scope() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "DELETE".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/A",
                    Canned::new(204),
                )
                .on(
                    "DELETE".parse().unwrap(),
                    "/api/v1/orgs/acme/actions/variables/A",
                    Canned::new(204),
                )
                .on(
                    "DELETE".parse().unwrap(),
                    "/api/v1/user/actions/variables/A",
                    Canned::new(204),
                ),
        );
        let api = api_for(fake.clone());
        api.repo().delete_repo_variable("o", "r", "A").await.expect("repository scope");
        api.org().delete_org_variable("acme", "A").await.expect("organization scope");
        api.user().delete_user_variable("A").await.expect("user scope");
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test]
    async fn every_scope_reads_from_its_own_route() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/variables/V",
                    Canned::json(200, r#"{"name":"V","data":"repo"}"#),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/orgs/acme/actions/variables/V",
                    Canned::json(200, r#"{"name":"V","data":"org"}"#),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/user/actions/variables/V",
                    Canned::json(200, r#"{"name":"V","data":"user"}"#),
                ),
        );
        let api = api_for(fake);
        for (scope, expected) in [
            (Scope::Repo(RepoSlug::new("o", "r")), "repo"),
            (Scope::Org("acme".to_owned()), "org"),
            (Scope::User, "user"),
        ] {
            assert_eq!(fetch(&api, &scope, "V").await.unwrap().data, expected);
        }
    }

    #[test]
    fn a_value_comes_from_the_argument_or_a_file() {
        assert_eq!(read_value(&set_args(Some("ghcr.io"))).unwrap(), "ghcr.io");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.txt");
        std::fs::write(&path, "ghcr.io\n").unwrap();
        let mut args = set_args(None);
        args.body_file = Some(path.to_str().unwrap().to_owned());
        // The trailing newline `echo` (or an editor) added is not part of the value.
        assert_eq!(read_value(&args).unwrap(), "ghcr.io");
    }

    /// Snapshot of both output modes. The VALUE column exists here and cannot exist for secrets,
    /// which is the whole distinction between the two groups.
    #[test]
    fn variable_list_output_goldens() {
        use crate::output::Term;
        let vars = vec![
            ActionVariable {
                name: "REGISTRY".into(),
                data: "ghcr.io".into(),
                ..Default::default()
            },
            ActionVariable {
                name: "DEPLOY_ENV".into(),
                data: "staging".into(),
                ..Default::default()
            },
        ];
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(80), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts { json: Some("name,data".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("getRepoVariablesList"),
                value: serde_json::to_value(&vars).unwrap(),
                count: vars.len(),
                total: None,
                noun: "variables",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["NAME", "VALUE"]);
                for v in &vars {
                    t.row([v.name.clone(), v.data.clone()]);
                }
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

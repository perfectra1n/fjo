//! `gea admin org` — organizations on the instance.
//!
//! Only two verbs, because only two exist: `GET /admin/orgs` lists every organization including
//! the private ones an ordinary token cannot see, and `POST /admin/users/{username}/orgs` creates
//! one **owned by a named account**. There is no admin route for editing or deleting an
//! organization — `gea org` does that with an ordinary token, which is why this subgroup stops
//! here rather than pretending otherwise.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{CreateOrgOption, Organization, VisibilityMode};
use gitea_core::error::Result;
use gitea_core::http::Paging;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;
use crate::output::Table;

pub const OP_ORG: &str = "adminGetAllOrgs";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List every organization on this instance, private ones included
    List,

    /// Create an organization owned by an existing account
    ///
    /// The owner must already exist; Gitea has no route that creates an unowned organization.
    Create(Create),
}

#[derive(Debug, ClapArgs)]
pub struct Create {
    /// The organization name people will type
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The account that will own it, and be its first administrator
    #[arg(long, value_name = "USERNAME")]
    pub owner: String,

    /// Display name
    #[arg(long, value_name = "NAME")]
    pub full_name: Option<String>,

    /// Who can see it: public, limited or private
    #[arg(long, value_name = "MODE", default_value = "public")]
    pub visibility: String,

    /// Contact email shown on the profile
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,

    /// Free-text description
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Profile website
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,

    /// Profile location
    #[arg(long, value_name = "TEXT")]
    pub location: Option<String>,

    /// Let repository administrators change team access to their repository
    #[arg(long)]
    pub repo_admin_change_team_access: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List | Cmd::Create(_) => OP_ORG,
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Create(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List => {
            let q = gitea_client::query::AdminGetAllOrgsQuery::default();
            let cap = support::item_cap(globals);
            let (orgs, total) = if globals.paginate {
                let orgs = support::drain(api.admin().get_all_orgs(&q), cap).await?;
                let n = orgs.len() as u64;
                (orgs, Some(n))
            } else {
                let (orgs, info) = api
                    .admin()
                    .get_all_orgs_page(&q, Paging { limit: cap, per_page: None })
                    .await?;
                (orgs, info.total_count)
            };
            emit.many(&orgs, total, "organizations", |table, orgs| {
                table.headers(["NAME", "FULL NAME", "VISIBILITY", "DESCRIPTION"]);
                for o in orgs {
                    table.row([
                        name_of(o),
                        o.full_name.clone(),
                        o.visibility.to_string(),
                        o.description.clone(),
                    ]);
                }
            })
        }

        Cmd::Create(a) => {
            let body = CreateOrgOption {
                description: a.description.clone(),
                email: a.email.clone(),
                full_name: a.full_name.clone(),
                location: a.location.clone(),
                repo_admin_change_team_access: Some(a.repo_admin_change_team_access),
                username: a.name.clone(),
                visibility: Some(VisibilityMode::from(a.visibility.as_str())),
                website: a.website.clone(),
            };
            let org = api.admin().create_org(&a.owner, &body).await?;
            emit.done(&format!("created organization {}, owned by {}", name_of(&org), a.owner));
            emit.one(&org, |t| detail(t, &org))
        }
    }
}

/// The organization's name.
///
/// `Organization` carries both `name` and a deprecated `username`, and which of the two an
/// instance populates has changed across releases. Preferring `name` and falling back keeps the
/// human view readable against an older server; `--json` still shows exactly what arrived, because
/// that is what `--json` promises.
fn name_of(o: &Organization) -> String {
    if o.name.is_empty() { o.username.clone() } else { o.name.clone() }
}

fn detail(table: &mut Table, o: &Organization) {
    table.row(["name".to_owned(), name_of(o)]);
    table.row(["id".to_owned(), o.id.to_string()]);
    table.row(["full_name".to_owned(), o.full_name.clone()]);
    table.row(["visibility".to_owned(), o.visibility.to_string()]);
    if !o.description.is_empty() {
        table.row(["description".to_owned(), o.description.clone()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    /// Bug this prevents: creating the organization under `/orgs` with an ordinary token, which
    /// makes the *caller* its owner rather than the account `--owner` names. On a shared instance
    /// that quietly gives the operator ownership of somebody else's organization.
    #[tokio::test]
    async fn create_posts_under_the_owning_account() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/admin/users/ada/orgs",
            Canned::json(201, r#"{"username":"acme","visibility":"limited"}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Create {
            name: "acme".into(),
            owner: "ada".into(),
            full_name: None,
            visibility: "limited".into(),
            email: None,
            description: None,
            website: None,
            location: None,
            repo_admin_change_team_access: false,
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::Create(args)).await.unwrap();
        }
        assert_eq!(fake.calls()[0].path, "/api/v1/admin/users/ada/orgs");
        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent["username"], serde_json::json!("acme"));
        assert_eq!(sent["visibility"], serde_json::json!("limited"));
    }

    #[tokio::test]
    async fn list_reaches_the_admin_collection_so_private_orgs_appear() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/orgs",
            Canned::json(
                200,
                r#"[{"username":"acme","full_name":"Acme","visibility":"public"},
                    {"username":"secret","visibility":"private"}]"#,
            ),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let out = {
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut emit =
                    Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
                run(&api, &globals, &mut emit, &Cmd::List).await.unwrap();
            }
            String::from_utf8(buf).unwrap()
        };
        assert_eq!(fake.calls()[0].path, "/api/v1/admin/orgs");
        assert_eq!(out, "acme\tAcme\tpublic\t\nsecret\t\tprivate\t\n");
    }
}

//! `gea org` — organizations, and their membership.
//!
//! `gh org` has exactly one subcommand (`list`). Everything else about an organization is a raw
//! API call, which is why `create`, `edit`, `delete` and `member` are here: they are the commands
//! a self-hosted instance's admin actually runs, and Gitea's API supports all of them.
//!
//! # Membership goes through teams
//!
//! There is **no** "add a member to an organization" route. Gitea has
//! `PUT /orgs/{org}/public_members/{u}` (which only *publicises* an existing member) and
//! `PUT /teams/{id}/members/{u}` — team membership *is* organization membership. So
//! `gea org member add` takes `--team`, resolves the team name to its id, and adds the user
//! there; omitting `--team` is an error that lists the organization's teams rather than a guess.
//! Guessing would mean picking `Owners`, and silently making someone an owner is not a default any
//! tool should have.
//!
//! `remove` is not symmetric with `add`, and that is the API's shape rather than an oversight:
//! `DELETE /orgs/{org}/members/{u}` removes the person from the organization and from every team
//! in it at once.
//!
//! # Works outside a checkout
//!
//! Nothing here resolves a repository, so `gea org list` works anywhere — which is required by
//! `docs/porcelain-conventions.md` and asserted by a test in `tests/porcelain.rs`.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::{CreateOrgOption, EditOrgOption, Organization, User, VisibilityMode};

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
    /// List organizations you belong to, another user's, or every one on the instance
    List(ListArgs),
    /// Show one organization
    View(ViewArgs),
    /// Create an organization
    Create(CreateArgs),
    /// Change an organization's details
    Edit(EditArgs),
    /// Delete an organization and everything in it
    Delete(DeleteArgs),
    /// Members of an organization
    #[command(subcommand)]
    Member(MemberCmd),
}

#[derive(Debug, Subcommand)]
pub enum MemberCmd {
    /// List members
    List(MemberListArgs),
    /// Add a user, by putting them in one of the organization's teams
    Add(MemberAddArgs),
    /// Remove a user from the organization and from every team in it
    Remove(MemberRemoveArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Organizations this user belongs to, instead of your own
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,
    /// Every organization on the instance
    #[arg(long, conflicts_with = "user")]
    pub all: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// The organization's name, as it appears in URLs
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Display name
    #[arg(long = "full-name", value_name = "TEXT")]
    pub full_name: Option<String>,
    /// Description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,
    /// Who can see it
    #[arg(long, value_name = "WHEN", value_parser = VisibilityMode::KNOWN.to_vec())]
    pub visibility: Option<String>,
    /// Website
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,
    /// Location
    #[arg(long, value_name = "TEXT")]
    pub location: Option<String>,
    /// Contact email
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
    /// Display name
    #[arg(long = "full-name", value_name = "TEXT")]
    pub full_name: Option<String>,
    /// Description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,
    /// Who can see it
    #[arg(long, value_name = "WHEN", value_parser = VisibilityMode::KNOWN.to_vec())]
    pub visibility: Option<String>,
    /// Website
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,
    /// Location
    #[arg(long, value_name = "TEXT")]
    pub location: Option<String>,
    /// Contact email
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct MemberListArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
    /// Only members whose membership is public
    #[arg(long)]
    pub public: bool,
}

#[derive(Debug, ClapArgs)]
pub struct MemberAddArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The user to add
    #[arg(value_name = "USER")]
    pub user: String,
    /// Which team to put them in — organization membership *is* team membership in Gitea
    #[arg(long, value_name = "TEAM")]
    pub team: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct MemberRemoveArgs {
    /// The organization's name
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The user to remove
    #[arg(value_name = "USER")]
    pub user: String,
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
            Cmd::View(a) => view(&rt, globals, &api, a).await,
            Cmd::Create(a) => create(&rt, globals, &api, a).await,
            Cmd::Edit(a) => edit(&rt, globals, &api, a).await,
            Cmd::Delete(a) => delete(&rt, &api, a).await,
            Cmd::Member(MemberCmd::List(a)) => member_list(&rt, globals, &api, a).await,
            Cmd::Member(MemberCmd::Add(a)) => member_add(&rt, &api, a).await,
            Cmd::Member(MemberCmd::Remove(a)) => member_remove(&rt, &api, a).await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Op("orgGetAll"),
        Cmd::View(_) | Cmd::Create(_) | Cmd::Edit(_) => Fields::Op("orgGet"),
        Cmd::Member(MemberCmd::List(_)) => Fields::Op("orgListMembers"),
        Cmd::Delete(_) | Cmd::Member(_) => Fields::None,
    }
}

// ------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let orgs: Vec<Organization> = if args.all {
        let q = query::OrgGetAllQuery::default();
        api.org().get_all(&q).take(limit).try_collect().await?
    } else if let Some(user) = &args.user {
        let q = query::OrgListUserOrgsQuery::default();
        api.org().list_user_orgs(user, &q).take(limit).try_collect().await?
    } else {
        // `/user/orgs`: the ones *you* are in, which is what `gh org list` shows.
        let q = query::OrgListCurrentUserOrgsQuery::default();
        api.org().list_current_user_orgs(&q).take(limit).try_collect().await?
    };

    let listing = Listing {
        fields: Fields::Op("orgGetAll"),
        value: serde_json::to_value(&orgs).map_err(encode_failed)?,
        count: orgs.len(),
        total: None,
        noun: "organizations",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["NAME", "FULL NAME", "VISIBILITY", "DESCRIPTION"]);
        for o in &orgs {
            t.row([
                o.username.clone(),
                o.full_name.clone(),
                o.visibility.to_string(),
                o.description.clone(),
            ]);
        }
    })
}

async fn view(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ViewArgs) -> Result<()> {
    let org = api.org().get(&args.org).await?;
    detail(rt, globals, &org)
}

fn detail(rt: &Runtime, globals: &GlobalOpts, org: &Organization) -> Result<()> {
    emit::detail(
        rt,
        globals,
        Fields::Op("orgGet"),
        serde_json::to_value(org).map_err(encode_failed)?,
        vec![
            ("name".to_owned(), org.username.clone()),
            ("full name".to_owned(), org.full_name.clone()),
            ("description".to_owned(), org.description.clone()),
            ("visibility".to_owned(), org.visibility.to_string()),
            ("website".to_owned(), org.website.clone()),
            ("location".to_owned(), org.location.clone()),
            ("email".to_owned(), org.email.clone()),
            ("id".to_owned(), org.id.to_string()),
        ],
    )
}

// ------------------------------------------------------------------------------ create / edit

async fn create(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &CreateArgs) -> Result<()> {
    let body = CreateOrgOption {
        username: args.name.clone(),
        full_name: args.full_name.clone(),
        description: args.description.clone(),
        website: args.website.clone(),
        location: args.location.clone(),
        email: args.email.clone(),
        visibility: args.visibility.as_deref().map(VisibilityMode::from),
        // Gitea's own default. Sent explicitly because the field is not an `Option` in the
        // generated model, so "not set" and "false" are the same bytes on the wire either way.
        repo_admin_change_team_access: Some(false),
    };
    let org = api.org().create(&body).await?;
    support::note(rt.term(), &format!("created {}", org.username));
    detail(rt, globals, &org)
}

/// `edit` mutates only what was named.
///
/// `EditOrgOption`'s fields are plain `String`s rather than `Option<String>`s, so an omitted flag
/// would serialise as `""` and *clear* the value. The current organization is therefore fetched
/// first and every unnamed field is sent back unchanged — the same reason
/// `docs/porcelain-conventions.md` insists edits never replace a whole set.
async fn edit(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &EditArgs) -> Result<()> {
    let current = api.org().get(&args.org).await?;
    let org = api.org().edit(&args.org, &merge_edit(args, &current)).await?;
    detail(rt, globals, &org)
}

/// The named changes on top of what the organization already is.
fn merge_edit(args: &EditArgs, current: &Organization) -> EditOrgOption {
    EditOrgOption {
        full_name: Some(args.full_name.clone().unwrap_or_else(|| current.full_name.clone())),
        description: Some(args.description.clone().unwrap_or_else(|| current.description.clone())),
        website: Some(args.website.clone().unwrap_or_else(|| current.website.clone())),
        location: Some(args.location.clone().unwrap_or_else(|| current.location.clone())),
        email: Some(args.email.clone().unwrap_or_else(|| current.email.clone())),
        visibility: Some(VisibilityMode::from(
            args.visibility.as_deref().unwrap_or(current.visibility.as_str()),
        )),
        repo_admin_change_team_access: Some(current.repo_admin_change_team_access),
    }
}

async fn delete(rt: &Runtime, api: &Api, args: &DeleteArgs) -> Result<()> {
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete the organization {} and every repository in it", args.org),
    )?;
    api.org().delete(&args.org).await?;
    support::note(rt.term(), &format!("deleted {}", args.org));
    Ok(())
}

// ----------------------------------------------------------------------------------- members

async fn member_list(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &MemberListArgs,
) -> Result<()> {
    let limit = support::limit(None, globals);
    let members: Vec<User> = if args.public {
        let q = query::OrgListPublicMembersQuery::default();
        api.org().list_public_members(&args.org, &q).take(limit).try_collect().await?
    } else {
        let q = query::OrgListMembersQuery::default();
        api.org().list_members(&args.org, &q).take(limit).try_collect().await?
    };

    let listing = Listing {
        fields: Fields::Op("orgListMembers"),
        value: serde_json::to_value(&members).map_err(encode_failed)?,
        count: members.len(),
        total: None,
        noun: "members",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["LOGIN", "NAME", "EMAIL", "ADMIN"]);
        for u in &members {
            t.row([
                u.login.clone(),
                u.full_name.clone(),
                u.email.clone(),
                if u.is_admin { "yes".to_owned() } else { String::new() },
            ]);
        }
    })
}

/// Add a member — which means adding them to a team.
async fn member_add(rt: &Runtime, api: &Api, args: &MemberAddArgs) -> Result<()> {
    let Some(team_name) = &args.team else {
        let teams = crate::cmd::team::names(api, &args.org).await.unwrap_or_default();
        return Err(Error::new(ErrorKind::Usage(format!(
            "organization membership requires joining a team. Select one with --team.{}",
            if teams.is_empty() {
                String::new()
            } else {
                format!(" {} has: {}", args.org, teams.join(", "))
            }
        ))));
    };
    let team = crate::cmd::team::resolve(api, &args.org, team_name).await?;
    api.org().add_team_member(team.id.get(), &args.user).await?;
    support::note(
        rt.term(),
        &format!("added {} to the team {} in {}", args.user, team.name, args.org),
    );
    Ok(())
}

async fn member_remove(rt: &Runtime, api: &Api, args: &MemberRemoveArgs) -> Result<()> {
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("remove {} from {} (and from every team in it)", args.user, args.org),
    )?;
    api.org().delete_member(&args.org, &args.user).await?;
    support::note(rt.term(), &format!("removed {} from {}", args.user, args.org));
    Ok(())
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
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

    /// Three different questions, three different routes. Bug this prevents: `--all` quietly
    /// listing only your own organizations, which on an instance you administer looks like the
    /// instance has almost no organizations.
    #[tokio::test]
    async fn each_listing_uses_its_own_route() {
        let fake = Arc::new(
            FakeTransport::new()
                .on("GET".parse().unwrap(), "/api/v1/user/orgs", Canned::json(200, "[]"))
                .on("GET".parse().unwrap(), "/api/v1/orgs", Canned::json(200, "[]"))
                .on("GET".parse().unwrap(), "/api/v1/users/ada/orgs", Canned::json(200, "[]")),
        );
        let api = api_for(fake.clone());
        let _: Vec<Organization> = api
            .org()
            .list_current_user_orgs(&query::OrgListCurrentUserOrgsQuery::default())
            .try_collect()
            .await
            .unwrap();
        let _: Vec<Organization> =
            api.org().get_all(&query::OrgGetAllQuery::default()).try_collect().await.unwrap();
        let _: Vec<Organization> = api
            .org()
            .list_user_orgs("ada", &query::OrgListUserOrgsQuery::default())
            .try_collect()
            .await
            .unwrap();
        // `/settings/api` and `/version` are the paginator's own capability probe, not part of
        // what this test is about.
        let paths: Vec<String> =
            fake.calls().iter().map(|c| c.path.clone()).filter(|p| p.contains("orgs")).collect();
        assert_eq!(paths, vec!["/api/v1/user/orgs", "/api/v1/orgs", "/api/v1/users/ada/orgs"]);
    }

    /// `create` sends the name as `username`, which is the field the API wants — it is not
    /// `name`, and getting it wrong is a 422 with no useful message.
    #[tokio::test]
    async fn create_posts_username_and_visibility() {
        let fake = Arc::new(FakeTransport::new().on(
            "POST".parse().unwrap(),
            "/api/v1/orgs",
            Canned::json(201, r#"{"username":"acme","visibility":"limited"}"#),
        ));
        let api = api_for(fake.clone());
        let body = CreateOrgOption {
            username: "acme".to_owned(),
            visibility: Some(VisibilityMode::from("limited")),
            ..Default::default()
        };
        let org = api.org().create(&body).await.unwrap();
        assert_eq!(org.username, "acme");
        let sent: serde_json::Value =
            serde_json::from_slice(&fake.calls()[0].body.clone().unwrap()).unwrap();
        assert_eq!(sent["username"], "acme");
        assert_eq!(sent["visibility"], "limited");
    }

    /// Bug this prevents — the expensive one: `org edit --description x` blanking the website,
    /// full name and email, because `EditOrgOption`'s fields are `String` and an omitted flag
    /// serialises as `""`.
    #[test]
    fn edit_preserves_every_field_it_was_not_asked_to_change() {
        let current = Organization {
            username: "acme".into(),
            full_name: "Acme Corp".into(),
            description: "old".into(),
            website: "https://acme.example".into(),
            location: "Toontown".into(),
            email: "hi@acme.example".into(),
            visibility: "limited".into(),
            repo_admin_change_team_access: true,
            ..Default::default()
        };
        let args = EditArgs {
            org: "acme".into(),
            full_name: None,
            description: Some("new".into()),
            visibility: None,
            website: None,
            location: None,
            email: None,
        };
        let body = merge_edit(&args, &current);
        assert_eq!(body.description.as_deref(), Some("new"));
        assert_eq!(body.website.as_deref(), Some("https://acme.example"));
        assert_eq!(body.full_name.as_deref(), Some("Acme Corp"));
        assert_eq!(body.email.as_deref(), Some("hi@acme.example"));
        assert_eq!(body.visibility, Some(VisibilityMode::Limited));
        assert_eq!(body.repo_admin_change_team_access, Some(true));
    }

    /// `member add` without `--team` must explain the API's shape and list the teams, not guess.
    /// Guessing means `Owners`, and silently making someone an owner is a security bug.
    #[tokio::test]
    async fn member_add_without_a_team_lists_the_teams() {
        // A second, empty page: without it the paginator cannot know the collection ended, since
        // Gitea sends no `Link` header here and a repeated full page looks like more data.
        let fake = Arc::new(FakeTransport::new().on_sequence(
            "GET".parse().unwrap(),
            "/api/v1/orgs/acme/teams",
            vec![
                Canned::json(200, r#"[{"id":1,"name":"Owners"},{"id":2,"name":"Dev"}]"#),
                Canned::json(200, "[]"),
            ],
        ));
        let names = crate::cmd::team::names(&api_for(fake), "acme").await.unwrap();
        assert_eq!(names, vec!["Owners", "Dev"]);
    }

    #[test]
    fn org_list_output_goldens() {
        let orgs = vec![
            Organization {
                username: "acme".into(),
                full_name: "Acme Corp".into(),
                visibility: "public".into(),
                description: "makes anvils".into(),
                ..Default::default()
            },
            Organization {
                username: "skunkworks".into(),
                visibility: "private".into(),
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
                GlobalOpts { json: Some("username,visibility".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("orgGetAll"),
                value: serde_json::to_value(&orgs).unwrap(),
                count: orgs.len(),
                total: None,
                noun: "organizations",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["NAME", "FULL NAME", "VISIBILITY", "DESCRIPTION"]);
                for o in &orgs {
                    t.row([
                        o.username.clone(),
                        o.full_name.clone(),
                        o.visibility.to_string(),
                        o.description.clone(),
                    ]);
                }
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

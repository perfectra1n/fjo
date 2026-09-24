//! `gea team` — organization teams, their members and their repositories.
//!
//! Neither `gh` nor `tea` has any team command at all, so today every team operation is a raw API
//! call. That is more painful than it sounds, because **the team API is addressed by numeric id**:
//! `PUT /teams/{id}/members/{username}`, `DELETE /teams/{id}/repos/{org}/{repo}`, and so on. To
//! add someone to the `Dev` team by hand you first list `/orgs/{org}/teams`, find the id, and then
//! use it — every single time.
//!
//! So the one thing this group really provides is [`resolve`]: a team *name* becomes its id, and a
//! name that does not exist becomes an error listing the ones that do. Everything else follows from
//! that.
//!
//! # `edit` is a read-modify-write, deliberately
//!
//! `PATCH /teams/{id}` takes `EditTeamOption`, whose `name` is required and whose other fields are
//! not `Option`s — so a naive `edit --description x` would rename the team to `""`, clear its unit
//! list, and drop its permission to the default. Every field is therefore read back from the team
//! first and only the named ones are changed, which is `docs/porcelain-conventions.md`'s
//! "edit commands mutate; they never replace a whole set" applied to a body that cannot express
//! absence.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoRef;
use gitea_model::{CreateTeamOption, EditTeamOption, PermissionLevel, Team, User};

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// How many teams to walk when resolving a name. An organization with more than this many teams
/// exists, but a name that is not in the first thousand is better reported as "not found" than
/// paged for forever.
const RESOLVE_CAP: usize = 1000;

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List an organization's teams
    List(ListArgs),
    /// Show one team, with its units and permission
    View(TeamArgs),
    /// Create a team
    Create(CreateArgs),
    /// Change a team
    Edit(EditArgs),
    /// Delete a team
    Delete(DeleteArgs),
    /// Team members
    #[command(subcommand)]
    Member(MemberCmd),
    /// Repositories a team can reach
    #[command(subcommand)]
    Repo(RepoCmd),
}

#[derive(Debug, Subcommand)]
pub enum MemberCmd {
    /// List a team's members
    List(TeamArgs),
    /// Add a user to a team (which also makes them an organization member)
    Add(MemberArgs),
    /// Remove a user from a team
    Remove(MemberArgs),
}

#[derive(Debug, Subcommand)]
pub enum RepoCmd {
    /// List the repositories a team can reach
    List(TeamArgs),
    /// Give a team access to a repository
    Add(TeamRepoArgs),
    /// Take a team's access to a repository away
    Remove(TeamRepoArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// Only teams matching this text
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct TeamArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's name
    #[arg(value_name = "TEAM")]
    pub team: String,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's name
    #[arg(value_name = "NAME")]
    pub name: String,
    /// Description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,
    /// What members may do in the team's repositories
    #[arg(long, value_name = "LEVEL", value_parser = PermissionLevel::KNOWN.to_vec())]
    pub permission: Option<String>,
    /// A unit the team can reach, e.g. `repo.code`; repeatable
    #[arg(long = "unit", value_name = "UNIT")]
    pub units: Vec<String>,
    /// Give the team every repository in the organization, including future ones
    #[arg(long = "all-repositories")]
    pub all_repositories: bool,
    /// Let members create repositories in the organization
    #[arg(long = "can-create-repos")]
    pub can_create_repos: bool,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's current name
    #[arg(value_name = "TEAM")]
    pub team: String,
    /// Rename the team
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,
    /// What members may do in the team's repositories
    #[arg(long, value_name = "LEVEL", value_parser = PermissionLevel::KNOWN.to_vec())]
    pub permission: Option<String>,
    /// Replace the team's units, e.g. `--unit repo.code --unit repo.issues`
    #[arg(long = "unit", value_name = "UNIT")]
    pub units: Vec<String>,
    /// Give the team every repository in the organization
    #[arg(long = "all-repositories", conflicts_with = "no_all_repositories")]
    pub all_repositories: bool,
    /// Stop giving the team every repository in the organization
    #[arg(long = "no-all-repositories")]
    pub no_all_repositories: bool,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's name
    #[arg(value_name = "TEAM")]
    pub team: String,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct MemberArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's name
    #[arg(value_name = "TEAM")]
    pub team: String,
    /// The user
    #[arg(value_name = "USER")]
    pub user: String,
}

#[derive(Debug, ClapArgs)]
pub struct TeamRepoArgs {
    /// The organization
    #[arg(value_name = "ORG")]
    pub org: String,
    /// The team's name
    #[arg(value_name = "TEAM")]
    pub team: String,
    /// The repository, as `name` or `owner/name`
    #[arg(value_name = "REPO")]
    pub repo: String,
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
            Cmd::Member(MemberCmd::Add(a)) => member_change(&rt, &api, a, true).await,
            Cmd::Member(MemberCmd::Remove(a)) => member_change(&rt, &api, a, false).await,
            Cmd::Repo(RepoCmd::List(a)) => repo_list(&rt, globals, &api, a).await,
            Cmd::Repo(RepoCmd::Add(a)) => repo_change(&rt, &api, a, true).await,
            Cmd::Repo(RepoCmd::Remove(a)) => repo_change(&rt, &api, a, false).await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Op("orgListTeams"),
        Cmd::View(_) | Cmd::Create(_) | Cmd::Edit(_) => Fields::Op("orgGetTeam"),
        Cmd::Member(MemberCmd::List(_)) => Fields::Op("orgListTeamMembers"),
        Cmd::Repo(RepoCmd::List(_)) => Fields::Op("orgListTeamRepos"),
        Cmd::Delete(_) | Cmd::Member(_) | Cmd::Repo(_) => Fields::None,
    }
}

// -------------------------------------------------------------------------- name → id, once

/// Find a team by name.
///
/// Case-insensitive, because Gitea's own team names are display-cased (`Owners`) while people
/// type `owners`. An exact match wins over a case-insensitive one so that two teams differing only
/// in case still resolve deterministically.
///
/// This is the function the whole group exists for: every write route below takes a numeric id.
pub(crate) async fn resolve(api: &Api, org: &str, name: &str) -> Result<Team> {
    let teams = all(api, org).await?;
    if let Some(exact) = teams.iter().find(|t| t.name == name) {
        return Ok(exact.clone());
    }
    if let Some(loose) = teams.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
        return Ok(loose.clone());
    }
    // `ResourceNotFound` rather than a `Usage` string: it exits 5, and its renderer already knows
    // to suggest `gea team list`.
    Err(Error::new(ErrorKind::ResourceNotFound {
        kind: "team",
        id: name.to_owned(),
        slug: Some(org.to_owned()),
        // Discovered locally: there was no server reply to quote.
        server_message: None,
    }))
}

/// Every team in an organization, capped.
async fn all(api: &Api, org: &str) -> Result<Vec<Team>> {
    let q = query::OrgListTeamsQuery::default();
    api.org().list_teams(org, &q).take(RESOLVE_CAP).try_collect().await
}

/// Team names, for an error message that lists the options.
pub(crate) async fn names(api: &Api, org: &str) -> Result<Vec<String>> {
    Ok(all(api, org).await?.into_iter().map(|t| t.name).collect())
}

// ------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    // `teamSearch` is the only way to filter server-side, and it wraps its results in an envelope;
    // the plain listing is used otherwise so that no query means no envelope to unwrap.
    let teams: Vec<Team> = match &args.query {
        Some(q) => {
            let query = query::TeamSearchQuery {
                q: Some(q.clone()),
                include_desc: Some(true),
                limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
                ..Default::default()
            };
            api.team().search(&args.org, &query).await?.data
        }
        None => {
            let q = query::OrgListTeamsQuery::default();
            api.org().list_teams(&args.org, &q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("orgListTeams"),
        value: serde_json::to_value(&teams).map_err(encode_failed)?,
        count: teams.len(),
        total: None,
        noun: "teams",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "PERMISSION", "UNITS", "ALL REPOS", "DESCRIPTION"]);
        for team in &teams {
            t.row([
                team.id.to_string(),
                team.name.clone(),
                team.permission.as_str().to_owned(),
                team.units.join(", "),
                if team.includes_all_repositories { "yes".to_owned() } else { String::new() },
                team.description.clone(),
            ]);
        }
    })
}

async fn view(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &TeamArgs) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    detail(rt, globals, &team)
}

fn detail(rt: &Runtime, globals: &GlobalOpts, team: &Team) -> Result<()> {
    let mut rows = vec![
        ("name".to_owned(), team.name.clone()),
        ("id".to_owned(), team.id.to_string()),
        ("description".to_owned(), team.description.clone()),
        ("permission".to_owned(), team.permission.as_str().to_owned()),
        ("units".to_owned(), team.units.join(", ")),
        (
            "all repositories".to_owned(),
            if team.includes_all_repositories { "yes".to_owned() } else { "no".to_owned() },
        ),
        (
            "can create repos".to_owned(),
            if team.can_create_org_repo { "yes".to_owned() } else { "no".to_owned() },
        ),
    ];
    // The per-unit access map is the part people actually need when debugging permissions, and it
    // is invisible in the `units` list.
    for (unit, access) in &team.units_map {
        rows.push((format!("unit {unit}"), access.clone()));
    }
    emit::detail(
        rt,
        globals,
        Fields::Op("orgGetTeam"),
        serde_json::to_value(team).map_err(encode_failed)?,
        rows,
    )
}

// ----------------------------------------------------------------------------- create / edit

async fn create(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &CreateArgs) -> Result<()> {
    let body = CreateTeamOption {
        name: args.name.clone(),
        description: args.description.clone(),
        permission: args.permission.as_deref().map(PermissionLevel::from),
        units: Some(args.units.clone()),
        includes_all_repositories: Some(args.all_repositories),
        can_create_org_repo: Some(args.can_create_repos),
        units_map: Default::default(),
        visibility: None,
    };
    let team = api.org().create_team(&args.org, &body).await?;
    support::note(
        rt.term(),
        &format!("created the team {} (id {}) in {}", team.name, team.id, args.org),
    );
    detail(rt, globals, &team)
}

async fn edit(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &EditArgs) -> Result<()> {
    let current = resolve(api, &args.org, &args.team).await?;
    // Gitea declares this one route's `{id}` as int32 where every other team route says int64.
    let id = i32::try_from(current.id.get()).map_err(|_| {
        gitea_core::error::usage(format!(
            "team id {} is too large for Gitea's edit route",
            current.id
        ))
    })?;
    let team = api.org().edit_team(id, &merge_edit(args, &current)).await?;
    detail(rt, globals, &team)
}

/// The named changes on top of the team as it is.
///
/// Note `units`: an empty `--unit` list means "leave them alone", not "remove them all". Removing
/// every unit from a team makes its repositories invisible to its members, which is not something
/// to do by omission.
fn merge_edit(args: &EditArgs, current: &Team) -> EditTeamOption {
    EditTeamOption {
        name: args.name.clone().unwrap_or_else(|| current.name.clone()),
        description: Some(args.description.clone().unwrap_or_else(|| current.description.clone())),
        permission: Some(PermissionLevel::from(
            args.permission.as_deref().unwrap_or(current.permission.as_str()),
        )),
        units: Some(if args.units.is_empty() { current.units.clone() } else { args.units.clone() }),
        includes_all_repositories: if args.all_repositories {
            Some(true)
        } else if args.no_all_repositories {
            Some(false)
        } else {
            Some(current.includes_all_repositories)
        },
        can_create_org_repo: Some(current.can_create_org_repo),
        units_map: Some(current.units_map.clone()),
        visibility: None,
    }
}

async fn delete(rt: &Runtime, api: &Api, args: &DeleteArgs) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete the team {} (id {}) from {}", team.name, team.id, args.org),
    )?;
    api.org().delete_team(team.id.get()).await?;
    support::note(rt.term(), &format!("deleted the team {}", team.name));
    Ok(())
}

// ----------------------------------------------------------------------------------- members

async fn member_list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &TeamArgs) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    let q = query::OrgListTeamMembersQuery::default();
    let members: Vec<User> = api
        .org()
        .list_team_members(team.id.get(), &q)
        .take(support::limit(None, globals))
        .try_collect()
        .await?;

    let listing = Listing {
        fields: Fields::Op("orgListTeamMembers"),
        value: serde_json::to_value(&members).map_err(encode_failed)?,
        count: members.len(),
        total: None,
        noun: "members",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["LOGIN", "NAME", "EMAIL"]);
        for u in &members {
            t.row([u.login.clone(), u.full_name.clone(), u.email.clone()]);
        }
    })
}

async fn member_change(rt: &Runtime, api: &Api, args: &MemberArgs, add: bool) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    if add {
        api.org().add_team_member(team.id.get(), &args.user).await?;
        support::note(rt.term(), &format!("added {} to {}", args.user, team.name));
    } else {
        api.org().remove_team_member(team.id.get(), &args.user).await?;
        support::note(rt.term(), &format!("removed {} from {}", args.user, team.name));
    }
    Ok(())
}

// ------------------------------------------------------------------------------ repositories

async fn repo_list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &TeamArgs) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    let q = query::OrgListTeamReposQuery::default();
    let repos: Vec<gitea_model::Repository> = api
        .org()
        .list_team_repos(team.id.get(), &q)
        .take(support::limit(None, globals))
        .try_collect()
        .await?;

    let listing = Listing {
        fields: Fields::Op("orgListTeamRepos"),
        value: serde_json::to_value(&repos).map_err(encode_failed)?,
        count: repos.len(),
        total: None,
        noun: "repositories",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["NAME", "VISIBILITY", "DESCRIPTION"]);
        for r in &repos {
            t.row([
                r.full_name.clone(),
                if r.private { "private".to_owned() } else { "public".to_owned() },
                r.description.clone(),
            ]);
        }
    })
}

async fn repo_change(rt: &Runtime, api: &Api, args: &TeamRepoArgs, add: bool) -> Result<()> {
    let team = resolve(api, &args.org, &args.team).await?;
    // `PUT /teams/{id}/repos/{org}/{repo}` names the owner separately, and a team can only reach
    // repositories of its *own* organization — so a bare `name` is completed with the org, and an
    // `owner/name` that disagrees is a mistake worth catching here rather than as a 404.
    let (owner, name) = split_repo(&args.repo, &args.org)?;
    if add {
        api.org().add_team_repository(team.id.get(), &owner, &name).await?;
        support::note(rt.term(), &format!("gave {} access to {}/{}", team.name, owner, name));
    } else {
        api.org().remove_team_repository(team.id.get(), &owner, &name).await?;
        support::note(rt.term(), &format!("removed {}'s access to {}/{}", team.name, owner, name));
    }
    Ok(())
}

/// `name` or `owner/name`, where the owner must be the organization.
fn split_repo(given: &str, org: &str) -> Result<(String, String)> {
    if !given.contains('/') {
        return Ok((org.to_owned(), given.to_owned()));
    }
    let repo_ref: RepoRef = given.parse().map_err(|e| support::usage(format!("{e}")))?;
    if !repo_ref.slug.owner.eq_ignore_ascii_case(org) {
        return Err(support::usage(format!(
            "{given} belongs to {}, but a team can only be given access to its own \
             organization's repositories ({org})",
            repo_ref.slug.owner
        )));
    }
    Ok((repo_ref.slug.owner, repo_ref.slug.name))
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use crate::output::Term;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::ids::TeamId;
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

    const TEAMS: &str = r#"[{"id":1,"name":"Owners","permission":"owner"},
                            {"id":7,"name":"Dev","permission":"write","units":["repo.code"]}]"#;

    /// A fake that answers page 1 with the two teams and every later page with `[]`.
    ///
    /// `on_fn` keyed on `?page`, not a canned reply: a canned reply ignores the query and
    /// re-serves a full page for ever. With no `Link` header, no `x-total-count`, no empty page
    /// and no short page, not one of the paginator's termination rules can fire, so [`all`]
    /// walks until its `RESOLVE_CAP` of 1000 items — about 500 round trips through the fake,
    /// yielding 500 duplicate copies of every team. The name lookups below still found the right
    /// team, so the bug was invisible; it is the same shape as the one `admin cron` was fixed
    /// for.
    ///
    /// Answering off the **query** rather than off a call counter is the point. `on_sequence`
    /// would hand `[]` to the second request whatever it asked for, so a client that forgot to
    /// increment `page` — the bug that spins for ever against a real server — would still pass
    /// here. A real server decides from the request it was given, and so does this one.
    fn teams_fake() -> Arc<FakeTransport> {
        Arc::new(FakeTransport::new().on_fn(
            "GET".parse().unwrap(),
            "/api/v1/orgs/acme/teams",
            |call| testing::paged(call, TEAMS),
        ))
    }

    /// The reason this group exists: users type names, the API takes ids.
    #[tokio::test]
    async fn a_team_name_resolves_to_its_id() {
        let api = api_for(teams_fake());
        assert_eq!(resolve(&api, "acme", "Dev").await.unwrap().id, TeamId::new(7));
        // Gitea's names are display-cased and people type lower case.
        assert_eq!(resolve(&api, "acme", "dev").await.unwrap().id, TeamId::new(7));
    }

    /// Bug this prevents: a mistyped team name producing a 404 from `/teams/0/members/x` — or
    /// worse, silently acting on whichever team happened to be first.
    #[tokio::test]
    async fn an_unknown_team_is_a_not_found_naming_the_org() {
        let e = resolve(&api_for(teams_fake()), "acme", "Ops").await.unwrap_err();
        assert_eq!(e.exit_code(), 5, "{e}");
        assert!(e.to_string().contains("Ops"), "{e}");
    }

    /// Bug this prevents — the expensive one: `team edit --description x` renaming the team to the
    /// empty string, clearing its units and dropping its permission, because `EditTeamOption` has
    /// no way to say "unchanged".
    #[test]
    fn edit_preserves_the_name_units_and_permission_it_was_not_given() {
        let current = Team {
            id: TeamId::new(7),
            name: "Dev".into(),
            description: "old".into(),
            permission: "write".into(),
            units: vec!["repo.code".into(), "repo.issues".into()],
            includes_all_repositories: true,
            can_create_org_repo: true,
            ..Default::default()
        };
        let args = EditArgs {
            org: "acme".into(),
            team: "Dev".into(),
            name: None,
            description: Some("new".into()),
            permission: None,
            units: vec![],
            all_repositories: false,
            no_all_repositories: false,
        };
        let body = merge_edit(&args, &current);
        assert_eq!(body.name, "Dev", "an omitted --name must not rename the team to \"\"");
        assert_eq!(body.description.as_deref(), Some("new"));
        assert_eq!(body.permission, Some(PermissionLevel::Write));
        assert_eq!(
            body.units.as_deref(),
            Some(&["repo.code".to_owned(), "repo.issues".to_owned()][..])
        );
        assert_eq!(body.includes_all_repositories, Some(true), "an omitted flag is not `false`");
        // And the explicit negative form does change it.
        let args = EditArgs { no_all_repositories: true, ..args };
        assert_eq!(merge_edit(&args, &current).includes_all_repositories, Some(false));
    }

    /// `team repo add` completes a bare name with the organization, and refuses a name from a
    /// different owner — which the API would answer with a bare 404.
    #[test]
    fn a_team_repository_must_belong_to_the_teams_organization() {
        assert_eq!(split_repo("anvil", "acme").unwrap(), ("acme".to_owned(), "anvil".to_owned()));
        assert_eq!(
            split_repo("acme/anvil", "acme").unwrap(),
            ("acme".to_owned(), "anvil".to_owned())
        );
        let e = split_repo("other/anvil", "acme").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("its own"), "{e}");
    }

    /// Adding a member goes to `/teams/{id}/members/{user}` — the id, not the name.
    #[tokio::test]
    async fn adding_a_member_uses_the_resolved_id() {
        let fake = Arc::new(
            FakeTransport::new()
                .on_fn("GET".parse().unwrap(), "/api/v1/orgs/acme/teams", |call| {
                    testing::paged(call, r#"[{"id":7,"name":"Dev"}]"#)
                })
                .on("PUT".parse().unwrap(), "/api/v1/teams/7/members/ada", Canned::new(204)),
        );
        let api = api_for(fake.clone());
        let team = resolve(&api, "acme", "Dev").await.unwrap();
        api.org().add_team_member(team.id.get(), "ada").await.unwrap();
        assert_eq!(fake.calls().last().unwrap().path, "/api/v1/teams/7/members/ada");

        // And the walk terminates on the empty second page rather than running to `RESOLVE_CAP`.
        // Against the static fixture this used to be ~1000 round trips serving 1000 copies of the
        // same team; the lookups still found it, which is why the waste was invisible.
        let listings = fake.calls().iter().filter(|c| c.path == "/api/v1/orgs/acme/teams").count();
        assert_eq!(listings, 2, "page 1, then the empty page that ends the collection");
    }

    #[test]
    fn team_list_output_goldens() {
        let teams = vec![
            Team {
                id: TeamId::new(1),
                name: "Owners".into(),
                permission: "owner".into(),
                units: vec!["repo.code".into()],
                includes_all_repositories: true,
                description: "the org's owners".into(),
                ..Default::default()
            },
            Team {
                id: TeamId::new(7),
                name: "Dev".into(),
                permission: "write".into(),
                units: vec!["repo.code".into(), "repo.issues".into()],
                ..Default::default()
            },
        ];
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(100), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts { json: Some("id,name,permission".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("orgListTeams"),
                value: serde_json::to_value(&teams).unwrap(),
                count: teams.len(),
                total: None,
                noun: "teams",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["ID", "NAME", "PERMISSION", "UNITS", "ALL REPOS", "DESCRIPTION"]);
                for team in &teams {
                    t.row([
                        team.id.to_string(),
                        team.name.clone(),
                        team.permission.as_str().to_owned(),
                        team.units.join(", "),
                        if team.includes_all_repositories {
                            "yes".to_owned()
                        } else {
                            String::new()
                        },
                        team.description.clone(),
                    ]);
                }
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

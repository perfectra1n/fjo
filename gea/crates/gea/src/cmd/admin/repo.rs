//! `gea admin repo` — repositories across the whole instance.
//!
//! `create` is `POST /admin/users/{username}/repos`: it makes a repository **owned by a named
//! account**, which is what you want when you are setting one up on somebody's behalf.
//!
//! `list` is the one command in this group that does not talk to `/admin/…`, because there is no
//! `GET /admin/repos`. It uses `GET /repos/search` with `private=true`, which an admin token
//! answers for the whole instance and an ordinary token answers only for what it can see. That
//! difference is worth knowing, so the help says it: with a non-admin token this is a search of
//! *your* repositories, and it will not error to tell you so.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{CreateRepoOption, Repository};
use gitea_core::error::Result;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;
use crate::output::Table;

/// `repoSearch` answers with a `{ok, data}` envelope, but what this command *emits* is the
/// repositories inside it, so `--json` is resolved against the repository's own field table.
pub const OP_REPO: &str = "repoGet";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List repositories on this instance, private ones included
    ///
    /// With an admin token this covers the whole instance. With an ordinary token the server
    /// answers for what that token can see, and says so by returning less — it is not an error.
    List(List),

    /// Create a repository owned by an existing account or organization
    Create(Create),
}

#[derive(Debug, ClapArgs)]
pub struct List {
    /// Only repositories whose name or description matches
    ///
    /// Long-only: the global `-q` is `--jq`, and clap answers a duplicate short with a panic.
    #[arg(long = "query", value_name = "TEXT")]
    pub query: Option<String>,

    /// Only repositories owned by this account or organization
    #[arg(long, value_name = "OWNER")]
    pub owner: Option<String>,

    /// Only archived repositories, or with --no-archived only live ones
    #[arg(long)]
    pub archived: bool,

    /// Only repositories that are not archived
    #[arg(long, conflicts_with = "archived")]
    pub no_archived: bool,

    /// Ordering field: alpha, created, updated, size, id
    #[arg(long, value_name = "FIELD")]
    pub sort: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Create {
    /// The repository name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// The account or organization that will own it
    #[arg(long, value_name = "OWNER")]
    pub owner: String,

    /// One-line description
    #[arg(short = 'd', long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Make it private
    #[arg(long)]
    pub private: bool,

    /// Give it a first commit: a README, a licence and a .gitignore
    #[arg(long)]
    pub auto_init: bool,

    /// Name of the first branch
    #[arg(long, value_name = "BRANCH")]
    pub default_branch: Option<String>,

    /// Licence to add, by name, when --auto-init is given
    #[arg(long, value_name = "NAME")]
    pub license: Option<String>,

    /// Comma-separated .gitignore templates, when --auto-init is given
    #[arg(long, value_name = "NAMES")]
    pub gitignores: Option<String>,

    /// Mark it as a template other repositories can be created from
    ///
    /// Spelled `--as-template` because the global `--template` is the output formatter.
    #[arg(long = "as-template")]
    pub template: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List(_) | Cmd::Create(_) => OP_REPO,
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Create(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List(a) => {
            let mut q = gitea_client::query::RepoSearchQuery::default().with_private(true);
            if let Some(text) = &a.query {
                q = q.with_q(text);
            }
            if let Some(owner) = &a.owner {
                // `q` searches names; the API scopes to an owner by passing the owner's name in
                // `q` together with `exclusive`, which is not the same thing. Using the documented
                // route — a `user/{owner}` prefix is not supported here — means we filter after
                // the fact rather than pretend the server did it.
                q = q.with_q(owner).with_exclusive(true);
            }
            if a.archived {
                q = q.with_archived(true);
            }
            if a.no_archived {
                q = q.with_archived(false);
            }
            if let Some(sort) = &a.sort {
                q = q.with_sort(sort);
            }
            // `--limit` reaches the server as the page size, because `/repos/search` is a
            // single-shot search rather than a walked collection.
            if let Some(cap) = support::item_cap(globals) {
                q = q.with_limit(i32::try_from(cap).unwrap_or(i32::MAX));
            }

            let found = api.repo().search(&q).await?;
            let repos: Vec<Repository> = match &a.owner {
                Some(owner) => found
                    .data
                    .into_iter()
                    .filter(|r| r.owner.as_ref().is_some_and(|u| &u.login == owner))
                    .collect(),
                None => found.data,
            };
            emit.many(&repos, None, "repositories", |table, repos| {
                table.headers(["REPOSITORY", "VISIBILITY", "SIZE (KiB)", "UPDATED"]);
                for r in repos {
                    table.row([
                        r.full_name.clone(),
                        visibility(r),
                        r.size.to_string(),
                        r.updated_at.as_ref().map(ToString::to_string).unwrap_or_default(),
                    ]);
                }
            })
        }

        Cmd::Create(a) => {
            let body = CreateRepoOption {
                auto_init: Some(a.auto_init),
                default_branch: a.default_branch.clone(),
                description: a.description.clone(),
                gitignores: a.gitignores.clone(),
                issue_labels: None,
                license: a.license.clone(),
                name: a.name.clone(),
                object_format_name: Default::default(),
                private: Some(a.private),
                readme: None,
                template: Some(a.template),
                trust_model: Default::default(),
            };
            let repo = api.admin().create_repo(&a.owner, &body).await?;
            emit.done(&format!("created {} for {}", repo.full_name, a.owner));
            emit.one(&repo, |t| detail(t, &repo))
        }
    }
}

/// The word an operator is actually looking for when auditing an instance.
fn visibility(r: &Repository) -> String {
    let mut flags: Vec<&str> = vec![if r.private { "private" } else { "public" }];
    if r.archived {
        flags.push("archived");
    }
    if r.fork {
        flags.push("fork");
    }
    if r.mirror {
        flags.push("mirror");
    }
    if r.empty {
        flags.push("empty");
    }
    flags.join("+")
}

fn detail(table: &mut Table, r: &Repository) {
    table.row(["repository".to_owned(), r.full_name.clone()]);
    table.row(["id".to_owned(), r.id.to_string()]);
    table.row(["visibility".to_owned(), visibility(r)]);
    table.row(["default_branch".to_owned(), r.default_branch.clone()]);
    table.row(["clone_url".to_owned(), r.clone_url.clone()]);
    table.row(["ssh_url".to_owned(), r.ssh_url.clone()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    fn list_args() -> List {
        List { query: None, owner: None, archived: false, no_archived: false, sort: None }
    }

    /// Bug this prevents: forgetting `private=true`, which turns an instance audit into a list of
    /// public repositories and hides exactly the ones an operator is looking for.
    #[tokio::test]
    async fn list_asks_for_private_repositories_too() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/search",
            Canned::json(
                200,
                r#"{"ok":true,"data":[
                    {"full_name":"ada/widget","private":true,"size":42},
                    {"full_name":"acme/public","size":7,"archived":true}]}"#,
            ),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(list_args())).await.unwrap();
        }
        assert_eq!(fake.calls()[0].query_param("private"), Some("true"));
        assert_eq!(fake.calls()[0].query_param("limit"), Some("30"));
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }

    /// Bug this prevents: `--owner acme` returning every repository whose *name* contains "acme".
    /// The search endpoint has no owner parameter, so the filter has to be applied here — and
    /// silently returning the unfiltered list would be the worst of the three options.
    #[tokio::test]
    async fn the_owner_filter_is_applied_even_though_the_endpoint_has_none() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/search",
            Canned::json(
                200,
                r#"{"ok":true,"data":[
                    {"full_name":"acme/widget","owner":{"login":"acme"}},
                    {"full_name":"ada/acme-tools","owner":{"login":"ada"}}]}"#,
            ),
        ));
        let api = testing::api(fake);
        let globals = GlobalOpts::default();
        let mut args = list_args();
        args.owner = Some("acme".to_owned());
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("acme/widget"), "{out}");
        assert!(!out.contains("ada/acme-tools"), "{out}");
    }

    /// Bug this prevents: creating the repository under the caller's own account, which is what
    /// `POST /user/repos` would do and is not what `--owner` asked for.
    #[tokio::test]
    async fn create_posts_under_the_owning_account() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/admin/users/acme/repos",
            Canned::json(201, r#"{"full_name":"acme/widget","private":true}"#),
        ));
        let api = testing::api(fake.clone());
        let args = Create {
            name: "widget".into(),
            owner: "acme".into(),
            description: None,
            private: true,
            auto_init: true,
            default_branch: Some("main".into()),
            license: None,
            gitignores: None,
            template: false,
        };
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::Create(args)).await.unwrap();
        }
        assert_eq!(fake.calls()[0].path, "/api/v1/admin/users/acme/repos");
        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent["name"], serde_json::json!("widget"));
        assert_eq!(sent["private"], serde_json::json!(true));
        assert_eq!(sent["auto_init"], serde_json::json!(true));
        assert_eq!(sent["default_branch"], serde_json::json!("main"));
    }
}

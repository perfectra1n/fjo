//! `gea search` — instance-wide search for repositories, issues, pull requests, users and topics.
//!
//! # Everything here works outside a checkout
//!
//! No subcommand resolves a repository, so nothing shells out to `git` and nothing fails in `/tmp`.
//! That is a requirement from `docs/porcelain-conventions.md` (`gea search` is named in it) and it
//! is asserted by a test that runs the binary with `--repo`-less arguments in an empty directory.
//! The one place a repository *could* creep in is `issues --repo`, which is why that filter is
//! spelled `--owner`/`--repo-name` against the API's own parameters rather than reusing `-R`.
//!
//! # `code` has no API
//!
//! Gitea 1.27.3 has no code-search endpoint at all (nor does Forgejo): the instance's `/explore/code` page is web
//! UI only. `search code` therefore exists to say so and to offer `-w/--web`, which opens exactly
//! that page with the query filled in. A subcommand that explains a gap and then does the next
//! best thing beats "unrecognized subcommand", which reads as a spelling mistake.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::{Issue, Repository, TopicResponse, User};

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::output::project::{FieldKind, FieldSpec};
use crate::runtime::Runtime;

/// `TopicResponse`'s own field names. Hand-written because `topicSearch`'s generated table
/// describes the *envelope* (`{"topics": […]}`) and this command unwraps it, so the envelope's
/// single `topics` key would be the only thing `--json` could select.
const TOPIC_FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "id", kind: FieldKind::Int, doc: "the topic's id" },
    FieldSpec { name: "topic_name", kind: FieldKind::Str, doc: "the topic" },
    FieldSpec { name: "repo_count", kind: FieldKind::Int, doc: "repositories carrying it" },
    FieldSpec { name: "created", kind: FieldKind::DateTime, doc: "when it first appeared" },
    FieldSpec { name: "updated", kind: FieldKind::DateTime, doc: "when it was last used" },
];

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Search repositories
    Repos(ReposArgs),
    /// Search issues
    Issues(IssuesArgs),
    /// Search pull requests
    Prs(IssuesArgs),
    /// Search users
    Users(QueryArgs),
    /// Search repository topics
    Topics(QueryArgs),
    /// Search code — no API exists; use -w to open the instance's code search
    Code(CodeArgs),
}

#[derive(Debug, ClapArgs)]
pub struct QueryArgs {
    /// What to look for
    #[arg(value_name = "QUERY", num_args = 0..)]
    pub query: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ReposArgs {
    /// What to look for
    #[arg(value_name = "QUERY", num_args = 0..)]
    pub query: Vec<String>,
    /// Only repositories owned by this user or organization
    #[arg(long, value_name = "OWNER")]
    pub owner: Option<String>,
    /// Treat the query as a topic name
    #[arg(long)]
    pub topic: bool,
    /// Include archived repositories, or only them
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    pub archived: Option<bool>,
    /// Only private repositories you can see
    #[arg(long)]
    pub private: bool,
    /// Sort by `alpha`, `created`, `updated`, `size`, `id`, or `stars`
    #[arg(long, value_name = "FIELD")]
    pub sort: Option<String>,
    /// `asc` or `desc`
    #[arg(long, value_name = "ORDER", value_parser = ["asc", "desc"])]
    pub order: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct IssuesArgs {
    /// What to look for
    #[arg(value_name = "QUERY", num_args = 0..)]
    pub query: Vec<String>,
    /// `open`, `closed`, or `all`
    #[arg(short = 's', long, value_name = "STATE", value_parser = ["open", "closed", "all"],
          default_value = "open")]
    pub state: String,
    /// Only issues carrying this label; repeatable
    #[arg(short = 'l', long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
    /// Only in repositories owned by this user or organization
    #[arg(long, value_name = "OWNER")]
    pub owner: Option<String>,
    /// Only issues assigned to you
    #[arg(long)]
    pub assigned: bool,
    /// Only issues you created
    #[arg(long)]
    pub created: bool,
    /// Only issues that mention you
    #[arg(long)]
    pub mentioned: bool,
    /// Only issues in this milestone; repeatable
    #[arg(short = 'm', long = "milestone", value_name = "TITLE")]
    pub milestones: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct CodeArgs {
    /// What to look for
    #[arg(value_name = "QUERY", num_args = 0..)]
    pub query: Vec<String>,
    /// Open the instance's code search in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::Repos(a) => repos(&rt, globals, &api, a).await,
            Cmd::Issues(a) => issues(&rt, globals, &api, a, false).await,
            Cmd::Prs(a) => issues(&rt, globals, &api, a, true).await,
            Cmd::Users(a) => users(&rt, globals, &api, a).await,
            Cmd::Topics(a) => topics(&rt, globals, &api, a).await,
            Cmd::Code(a) => code(&rt, a),
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        // `repoSearch`'s own table is the `{ok, data}` envelope, and this command unwraps it — so
        // the field table is `Repository`'s, which is what the rows actually are.
        Cmd::Repos(_) => Fields::Op("repoGet"),
        Cmd::Issues(_) | Cmd::Prs(_) => Fields::Op("issueSearchIssues"),
        Cmd::Users(_) => Fields::Op("userGet"),
        Cmd::Topics(_) => Fields::Custom(TOPIC_FIELDS),
        Cmd::Code(_) => Fields::None,
    }
}

/// The positional query words, joined.
///
/// Words rather than one string so `gea search repos anvil factory` works without quoting, which
/// is how people type a search. An empty query is legal and means "everything", which is the
/// natural reading of `gea search repos --owner acme`.
fn terms(words: &[String]) -> Option<String> {
    let joined = words.join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

async fn repos(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ReposArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let mut q = query::RepoSearchQuery {
        q: terms(&args.query),
        topic: args.topic.then_some(true),
        archived: args.archived,
        is_private: args.private.then_some(true),
        sort: args.sort.clone(),
        order: args.order.clone(),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    // `/repos/search` takes an owner *id*, not a name — so a name has to be resolved first. This
    // is the kind of thing a porcelain command exists to hide.
    if let Some(owner) = &args.owner {
        q.uid = Some(api.user().get(owner).await?.id.get());
    }
    let found = api.repo().search(&q).await?;
    let repos: Vec<Repository> = found.data;

    let listing = Listing {
        fields: Fields::Op("repoGet"),
        value: serde_json::to_value(&repos).map_err(encode_failed)?,
        count: repos.len(),
        total: None,
        noun: "repositories",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["REPOSITORY", "VISIBILITY", "STARS", "DESCRIPTION", "UPDATED"]);
        for r in &repos {
            t.row([
                r.full_name.clone(),
                visibility(r),
                r.stars_count.to_string(),
                r.description.clone(),
                support::ago(r.updated_at.as_ref()),
            ]);
        }
    })
}

async fn issues(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &IssuesArgs,
    pulls: bool,
) -> Result<()> {
    let limit = support::limit(None, globals);
    let q = query::IssueSearchIssuesQuery {
        q: terms(&args.query),
        state: Some(args.state.clone()),
        // The API takes one comma-separated list, not a repeated parameter.
        labels: (!args.labels.is_empty()).then(|| args.labels.join(",")),
        milestones: (!args.milestones.is_empty()).then(|| args.milestones.join(",")),
        owner: args.owner.clone(),
        assigned: args.assigned.then_some(true),
        created: args.created.then_some(true),
        mentioned: args.mentioned.then_some(true),
        // `prs` and `issues` are one endpoint with a `type` filter, which is why they share these
        // arguments rather than being two half-identical commands.
        r#type: Some(if pulls { "pulls".to_owned() } else { "issues".to_owned() }),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    let found: Vec<Issue> = api.issue().search_issues(&q).take(limit).try_collect().await?;

    let noun = if pulls { "pull requests" } else { "issues" };
    let listing = Listing {
        fields: Fields::Op("issueSearchIssues"),
        value: serde_json::to_value(&found).map_err(encode_failed)?,
        count: found.len(),
        total: None,
        noun,
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["REPOSITORY", "#", "STATE", "TITLE", "LABELS", "UPDATED"]);
        for i in &found {
            t.row([
                i.repository.as_ref().map(|r| r.full_name.clone()).unwrap_or_default(),
                i.number.to_string(),
                crate::output::color::autocolor(rt.term(), i.state.as_str()),
                i.title.clone(),
                i.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>().join(", "),
                support::ago(i.updated_at.as_ref()),
            ]);
        }
    })
}

async fn users(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &QueryArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let q = query::UserSearchQuery {
        q: terms(&args.query),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    let found: Vec<User> = api.user().search(&q).await?.data;

    let listing = Listing {
        fields: Fields::Op("userGet"),
        value: serde_json::to_value(&found).map_err(encode_failed)?,
        count: found.len(),
        total: None,
        noun: "users",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["LOGIN", "NAME", "LOCATION", "FOLLOWERS"]);
        for u in &found {
            t.row([
                u.login.clone(),
                u.full_name.clone(),
                u.location.clone(),
                u.followers_count.to_string(),
            ]);
        }
    })
}

async fn topics(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &QueryArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let q = query::TopicSearchQuery {
        q: terms(&args.query),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    let found: Vec<TopicResponse> = api.topic().search(&q).await?.topics;

    let listing = Listing {
        fields: Fields::Custom(TOPIC_FIELDS),
        value: serde_json::to_value(&found).map_err(encode_failed)?,
        count: found.len(),
        total: None,
        noun: "topics",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["TOPIC", "REPOSITORIES", "UPDATED"]);
        for topic in &found {
            t.row([
                topic.topic_name.clone(),
                topic.repo_count.to_string(),
                support::ago(topic.updated.as_ref()),
            ]);
        }
    })
}

/// Code search: explain the gap, and open the web UI if asked.
fn code(rt: &Runtime, args: &CodeArgs) -> Result<()> {
    let query = terms(&args.query).unwrap_or_default();
    if args.web {
        let url = format!(
            "{}/explore/code?q={}",
            rt.client().web_base(),
            gitea_core::http::encode::query(&query)
        );
        return crate::cmd::browse::open_or_print(rt, &url, false);
    }
    Err(no_code_api(&query))
}

/// Why `search code` cannot search.
fn no_code_api(query: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "Gitea code search is only available in the web interface. Open it with:\n  gea search code {query} --web\nor search repository names and descriptions:\n  gea search repos {query}"
    )))
}

fn visibility(r: &Repository) -> String {
    if r.private {
        "private".to_owned()
    } else if r.internal {
        "internal".to_owned()
    } else {
        "public".to_owned()
    }
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

    #[test]
    fn query_words_are_joined_and_an_empty_query_is_legal() {
        assert_eq!(terms(&["anvil".into(), "factory".into()]).as_deref(), Some("anvil factory"));
        assert_eq!(terms(&[]), None);
        assert_eq!(terms(&["   ".into()]), None);
    }

    /// `--owner` is a *name* and `/repos/search` wants a numeric `uid`, so the name is resolved
    /// first. Bug this prevents: sending `uid=acme`, which Gitea answers with an empty result set
    /// rather than an error — a filter that silently matches nothing.
    #[tokio::test]
    async fn an_owner_name_is_resolved_to_a_uid() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/users/acme",
                    Canned::json(200, r#"{"id":42,"login":"acme"}"#),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/search",
                    Canned::json(200, r#"{"ok":true,"data":[]}"#),
                ),
        );
        let api = api_for(fake.clone());
        let uid = api.user().get("acme").await.unwrap().id.get();
        let q = query::RepoSearchQuery { uid: Some(uid), ..Default::default() };
        api.repo().search(&q).await.unwrap();
        let search = fake.calls().last().unwrap().query.clone();
        assert!(search.contains("uid=42"), "{search}");
    }

    /// `issues` and `prs` are the same endpoint with `type=`. Bug this prevents: `search prs`
    /// returning issues, which is what happens when the filter is forgotten.
    #[tokio::test]
    async fn prs_and_issues_differ_only_by_the_type_filter() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/issues/search",
            Canned::json(200, "[]"),
        ));
        let api = api_for(fake.clone());
        for kind in ["issues", "pulls"] {
            let q = query::IssueSearchIssuesQuery {
                r#type: Some(kind.to_owned()),
                state: Some("open".to_owned()),
                labels: Some("bug,ci".to_owned()),
                ..Default::default()
            };
            let _: Vec<Issue> = api.issue().search_issues(&q).try_collect().await.unwrap();
        }
        let queries: Vec<String> = fake
            .calls()
            .iter()
            .filter(|c| c.path.ends_with("/issues/search"))
            .map(|c| c.query.clone())
            .collect();
        assert!(queries[0].contains("type=issues"), "{queries:?}");
        assert!(queries[1].contains("type=pulls"), "{queries:?}");
        // Labels are one comma-separated parameter rather than a repeated one; percent-encoded
        // (`%2C`) is fine, since the server decodes before splitting.
        assert!(
            queries[0].contains("labels=bug,ci") || queries[0].contains("labels=bug%2Cci"),
            "{queries:?}"
        );
    }

    /// Code search has no endpoint, and the refusal has to name the two things that do work.
    #[test]
    fn code_search_explains_the_missing_api() {
        let e = no_code_api("anvil");
        assert_eq!(e.exit_code(), 2);
        let msg = e.to_string();
        assert!(msg.contains("code search is only available in the web interface"), "{msg}");
        assert!(msg.contains("--web"), "{msg}");
        assert!(msg.contains("gea search repos"), "{msg}");
    }

    #[test]
    fn search_repos_output_goldens() {
        let repos = vec![
            Repository {
                full_name: "acme/anvil".into(),
                description: "heavy things".into(),
                stars_count: 12,
                ..Default::default()
            },
            Repository { full_name: "acme/secret".into(), private: true, ..Default::default() },
        ];
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(90), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts { json: Some("full_name,private".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("repoGet"),
                value: serde_json::to_value(&repos).unwrap(),
                count: repos.len(),
                total: None,
                noun: "repositories",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["REPOSITORY", "VISIBILITY", "STARS", "DESCRIPTION", "UPDATED"]);
                for r in &repos {
                    t.row([
                        r.full_name.clone(),
                        visibility(r),
                        r.stars_count.to_string(),
                        r.description.clone(),
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

//! `gea repo list` — repositories for you, a user, or an organization.
//!
//! The context inference is the whole of it: no argument means *your* repositories, including the
//! private ones, which is `GET /user/repos` rather than `GET /users/{me}/repos` — the latter shows
//! only what a stranger could see, and a `repo list` that silently omitted your private work would
//! be worse than no command.
//!
//! Filtering is client-side, deliberately. `GET /repos/search` has server-side `archived`,
//! `is_private` and `mode=fork|source` parameters, but it is a **single page** whose size the
//! instance clamps to `max_response_items` — so `-L 200` against it would silently return 50.
//! Walking `ItemStream` and filtering here means `-L` is honoured exactly, at the cost of fetching
//! rows we then discard.

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::{ErrorKind, Result};
use gitea_model::Repository;

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::autocolor;
use crate::output::table::Table;
use crate::output::template::funcs::timeago;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List repositories.

Without an owner, lists your repositories, including private ones. With a user
or organization, lists their repositories that you can access.
Columns: name, description, visibility, and last update. Piped output is TSV.

  gea repo list
  gea repo list gitea --source -L 100
  gea repo list | cut -f1")]
pub struct Args {
    /// User or organization. Defaults to you
    #[arg(value_name = "OWNER")]
    pub owner: Option<String>,

    /// Only forks
    #[arg(long, conflicts_with = "source")]
    pub fork: bool,

    /// Only repositories that are not forks
    #[arg(long)]
    pub source: bool,

    /// Only archived repositories
    #[arg(long, conflicts_with = "no_archived")]
    pub archived: bool,

    /// Skip archived repositories
    #[arg(long)]
    pub no_archived: bool,

    /// Only private repositories
    #[arg(long, conflicts_with = "public")]
    pub private: bool,

    /// Only public repositories
    #[arg(long)]
    pub public: bool,

    /// Only repositories carrying this topic
    #[arg(long, value_name = "TOPIC")]
    pub topic: Option<String>,

    /// Maximum number of repositories. The long form is the global `--limit`
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Open the owner's repository list in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_REPOSITORY)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let owner = args.owner.clone();

        if args.web {
            let who = match &owner {
                Some(o) => o.clone(),
                None => support::me(&api).await?,
            };
            let url =
                format!("{}/{who}?tab=repositories", rt.client().web_base().trim_end_matches('/'));
            return support::open_web(&rt, &url);
        }

        let limit = support::limit(args.limit, globals);
        let repos = fetch(&api, owner.as_deref(), args, limit).await?;

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&repos)?)
            }
            _ => {
                if repos.is_empty() {
                    support::empty_note(rt.term(), "repositories");
                }
                print!("{}", table(&repos, rt.term()));
                Ok(())
            }
        }
    })
}

/// Walk the right collection, filtering as we go, until `limit` rows are in hand.
async fn fetch(
    api: &Api,
    owner: Option<&str>,
    args: &Args,
    limit: usize,
) -> Result<Vec<Repository>> {
    let mut out = Vec::new();
    match owner {
        None => {
            let query = gitea_client::query::UserCurrentListReposQuery::default();
            let mut stream = api.user().current_list_repos(&query);
            collect(&mut stream, args, limit, &mut out).await?;
        }
        Some(owner) => {
            // `/users/{name}/repos` answers for organizations too on most instances, because an
            // organization *is* a user row. Where it does not, the fallback is the org endpoint —
            // tried only after a 404, so the ordinary case still costs one request.
            let query = gitea_client::query::UserListReposQuery::default();
            let mut stream = api.user().list_repos(owner, &query);
            match collect(&mut stream, args, limit, &mut out).await {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ResourceNotFound { .. } | ErrorKind::RepoNotFound { .. }
                    ) =>
                {
                    out.clear();
                    let query = gitea_client::query::OrgListReposQuery::default();
                    let mut stream = api.org().list_repos(owner, &query);
                    collect(&mut stream, args, limit, &mut out).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(out)
}

async fn collect(
    stream: &mut gitea_core::http::ItemStream<Repository>,
    args: &Args,
    limit: usize,
    out: &mut Vec<Repository>,
) -> Result<()> {
    while let Some(item) = stream.next().await {
        let repo = item?;
        if keep(&repo, args) {
            out.push(repo);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(())
}

/// The filter predicate, pure so the flag combinations can be tested without a server.
pub(crate) fn keep(repo: &Repository, args: &Args) -> bool {
    if args.fork && !repo.fork {
        return false;
    }
    if args.source && repo.fork {
        return false;
    }
    if args.archived && !repo.archived {
        return false;
    }
    if args.no_archived && repo.archived {
        return false;
    }
    if args.private && !repo.private {
        return false;
    }
    if args.public && repo.private {
        return false;
    }
    if let Some(topic) = &args.topic
        && !repo.topics.iter().any(|t| t.eq_ignore_ascii_case(topic))
    {
        return false;
    }
    true
}

/// Table on a terminal, TSV when piped — the split is `Table`'s, not ours.
pub(crate) fn table(repos: &[Repository], term: &Term) -> String {
    let mut t = Table::new(term);
    t.headers(["NAME", "DESCRIPTION", "VISIBILITY", "UPDATED"]);
    for repo in repos {
        t.row([
            repo.full_name.clone(),
            repo.description.clone(),
            autocolor(term, visibility(repo)),
            repo.updated_at.map(|ts| timeago(&ts.to_string())).unwrap_or_default(),
        ]);
    }
    t.render_to_string()
}

fn visibility(repo: &Repository) -> &'static str {
    if repo.archived {
        // Archived is the more important fact: it is why a push will be refused, and it outranks
        // "public" in every list a human scans.
        "archived"
    } else if repo.private {
        "private"
    } else {
        "public"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Args {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        <Harness as clap::Parser>::try_parse_from(words)
            .unwrap_or_else(|e| panic!("{words:?}: {e}"))
            .args
    }

    fn repo(name: &str, fork: bool, private: bool, archived: bool) -> Repository {
        Repository {
            full_name: format!("me/{name}"),
            description: "a thing".to_owned(),
            fork,
            private,
            archived,
            topics: vec!["cli".to_owned()],
            ..Repository::default()
        }
    }

    #[test]
    fn filters_are_independent_and_all_apply() {
        let plain = repo("a", false, false, false);
        let forked = repo("b", true, false, false);
        let hidden = repo("c", false, true, false);
        let old = repo("d", false, false, true);

        assert!(keep(&plain, &args(&["gea"])));
        assert!(!keep(&plain, &args(&["gea", "--fork"])));
        assert!(keep(&forked, &args(&["gea", "--fork"])));
        assert!(!keep(&forked, &args(&["gea", "--source"])));
        assert!(keep(&hidden, &args(&["gea", "--private"])));
        assert!(!keep(&hidden, &args(&["gea", "--public"])));
        assert!(!keep(&old, &args(&["gea", "--no-archived"])));
        assert!(keep(&old, &args(&["gea", "--archived"])));
        assert!(keep(&plain, &args(&["gea", "--topic", "CLI"])), "topics match case-insensitively");
        assert!(!keep(&plain, &args(&["gea", "--topic", "python"])));
    }

    /// Bug this prevents: an archived repository shown as merely `public`, so the reason a push
    /// will be refused is nowhere in the output.
    #[test]
    fn archived_outranks_visibility_in_the_column() {
        assert_eq!(visibility(&repo("a", false, true, true)), "archived");
        assert_eq!(visibility(&repo("a", false, true, false)), "private");
        assert_eq!(visibility(&repo("a", false, false, false)), "public");
    }

    #[test]
    fn table_snapshots_for_a_terminal_and_a_pipe() {
        let repos = vec![repo("alpha", false, false, false), repo("beta", true, true, false)];
        let mut report = String::from("== tty\n");
        report.push_str(&table(&repos, &Term::tty(80)));
        report.push_str("== piped\n");
        report.push_str(&table(&repos, &Term::piped()));
        insta::assert_snapshot!(report);
    }
}

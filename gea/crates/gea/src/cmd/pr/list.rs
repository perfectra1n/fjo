//! `gea pr list` — the table people will look at most.
//!
//! Two filters are done here rather than by the server, and both for the same reason: the endpoint
//! does not have them.
//!
//! * **`-s merged`.** Gitea's `state` is `open`/`closed`/`all` and nothing else; a merged pull
//!   request is a *closed* one with `merged: true`. So `merged` asks the server for `closed` and
//!   keeps the merged ones, and `closed` asks for `closed` and keeps the rest. Passing `merged`
//!   through to the API would return an empty list with no error.
//! * **`-a/--assignee` and `-S/--search`.** `GET /repos/{owner}/{repo}/pulls` has neither. The
//!   issue endpoint does, but it answers with `Issue` objects rather than `PullRequest` ones, which
//!   would change every `--json` field name for one filter — see `docs/output.md` on never
//!   inventing field names.

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::Result;
use gitea_core::types::RepoSlug;
use gitea_model::PullRequest;

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::table::Table;
use crate::runtime::Runtime;

/// How many rows to look at per row kept, at most, when a client-side filter is in play.
///
/// Without a bound, `-a nobody -L 30` on a busy repository would walk every pull request ever
/// opened looking for a thirtieth match. Ten pages' worth of slack is generous for a real filter and
/// finite for an empty one.
const SCAN_FACTOR: usize = 10;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List pull requests (open by default).

-s merged lists merged pull requests; -s closed lists closed, unmerged requests.
Piped output is tab-separated. Use -L or --limit to cap the result count.

  gea pr list
  gea pr list -s merged -L 10
  gea pr list -A @me --json number,title
  gea pr list -B main -l bug")]
pub struct Args {
    /// open, closed, merged, or all
    #[arg(short = 's', long, value_name = "STATE", default_value = "open",
          value_parser = ["open", "closed", "merged", "all"])]
    pub state: String,

    /// Only pull requests targeting this branch
    #[arg(short = 'B', long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Only pull requests from this branch
    #[arg(short = 'H', long, value_name = "BRANCH")]
    pub head: Option<String>,

    /// Only pull requests assigned to this user; `@me` is you
    #[arg(short = 'a', long, value_name = "USER")]
    pub assignee: Option<String>,

    /// Only pull requests opened by this user; `@me` is you
    #[arg(short = 'A', long, value_name = "USER")]
    pub author: Option<String>,

    /// Only pull requests carrying this label. Repeatable
    #[arg(short = 'l', long, value_name = "NAME")]
    pub label: Vec<String>,

    /// Only pull requests whose title or body contains this text
    #[arg(short = 'S', long, value_name = "TEXT")]
    pub search: Option<String>,

    /// Maximum number of pull requests. The long form is the global `--limit`
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Open the pull request list in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = rt.repo(globals)?.slug.clone();

        if args.web {
            let url = format!(
                "{}/{slug}/pulls?state={}",
                rt.client().web_base().trim_end_matches('/'),
                // The web UI has no `merged` tab either; `closed` is where merged ones live.
                if args.state == "merged" { "closed" } else { args.state.as_str() }
            );
            return support::open_web(&rt, &url);
        }

        let limit = support::limit(args.limit, globals);
        let prs = fetch(&api, &slug, args, limit).await?;

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&prs)?)
            }
            _ => {
                if prs.is_empty() {
                    support::empty_note(rt.term(), &format!("{} pull requests", args.state));
                }
                print!("{}", table(&prs, rt.term()));
                Ok(())
            }
        }
    })
}

async fn fetch(api: &Api, slug: &RepoSlug, args: &Args, limit: usize) -> Result<Vec<PullRequest>> {
    let mut query = gitea_client::query::RepoListPullRequestsQuery::default()
        .with_state(server_state(&args.state));
    if let Some(base) = &args.base {
        query = query.with_base_branch(base);
    }
    // There is no `head` filter on Gitea's list route; `--head` is applied in `keep`.
    // `--author @me --assignee @me` used to be two identical `GET /user` requests one after the
    // other, with the label lookup in between; all three depend on nothing but the command line,
    // so they run at the same time. `join!` with a fixed unwrap order rather than `try_join!`, so
    // a command with two bad names reports the same one it always did.
    let resolve_author = async {
        match &args.author {
            Some(a) => support::resolve_me(api, std::slice::from_ref(a))
                .await
                .map(|mut who| Some(who.remove(0))),
            None => Ok(None),
        }
    };
    let resolve_labels = async {
        if args.label.is_empty() {
            return Ok(None);
        }
        common::label_ids(api, slug, &args.label).await.map(Some)
    };
    let resolve_assignee = async {
        match &args.assignee {
            Some(a) => support::resolve_me(api, std::slice::from_ref(a))
                .await
                .map(|mut who| Some(who.remove(0))),
            None => Ok(None),
        }
    };
    let resolved = futures::join!(resolve_author, resolve_labels, resolve_assignee);
    let author = resolved.0?;
    let labels = resolved.1?;
    let assignee = resolved.2?;

    // Applied in the order the serial version applied them, so the query string is unchanged.
    if let Some(poster) = &author {
        query = query.with_poster(poster);
    }
    if let Some(ids) = labels {
        query = query.with_labels(ids);
    }
    let client_side = assignee.is_some()
        || args.head.is_some()
        || args.search.is_some()
        || args.state == "merged"
        || args.state == "closed";
    let scan = if client_side { limit.saturating_mul(SCAN_FACTOR) } else { limit };

    let mut out = Vec::new();
    let mut stream = api.repo().list_pull_requests(&slug.owner, &slug.name, &query).take(scan);
    while let Some(item) = stream.next().await {
        let pr = item?;
        if keep(&pr, args, assignee.as_deref()) {
            out.push(pr);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// The `state` value the API understands.
///
/// `merged` is not one of them. See the module docs.
pub(crate) fn server_state(state: &str) -> &str {
    match state {
        "merged" => "closed",
        other => other,
    }
}

/// The client-side half of the filter.
pub(crate) fn keep(pr: &PullRequest, args: &Args, assignee: Option<&str>) -> bool {
    match args.state.as_str() {
        "merged" if !pr.merged => return false,
        // `-s closed` meaning "closed *and not merged*" is the only reading that makes `closed` and
        // `merged` two useful, disjoint answers rather than one being a superset of the other.
        "closed" if pr.merged => return false,
        _ => {}
    }
    if let Some(who) = assignee {
        let assigned = pr.assignees.iter().any(|u| u.login.eq_ignore_ascii_case(who))
            || pr.assignee.as_ref().is_some_and(|u| u.login.eq_ignore_ascii_case(who));
        if !assigned {
            return false;
        }
    }
    if let Some(head) = &args.head
        && !common::head_matches(pr, head)
    {
        return false;
    }
    if let Some(text) = &args.search {
        let needle = text.to_lowercase();
        if !pr.title.to_lowercase().contains(&needle) && !pr.body.to_lowercase().contains(&needle) {
            return false;
        }
    }
    true
}

pub(crate) fn table(prs: &[PullRequest], term: &Term) -> String {
    let mut t = Table::new(term);
    t.headers(["NUMBER", "TITLE", "BRANCH", "STATE", "UPDATED"]);
    for pr in prs {
        t.row(common::row(pr, term));
    }
    t.render_to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::types::ids::IssueIndex;
    use gitea_model::{PrBranchInfo, StateType, User};

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

    fn pr(number: i64, state: &str, merged: bool) -> PullRequest {
        PullRequest {
            number: IssueIndex::new(number),
            title: format!("change {number}"),
            state: StateType::from(state),
            merged,
            head: Some(PrBranchInfo { label: "feature".to_owned(), ..PrBranchInfo::default() }),
            ..PullRequest::default()
        }
    }

    /// Bug this prevents: passing `-s merged` to the API, which answers with an empty list and no
    /// error — so the command reports "no pull requests matched" for a repository full of them.
    #[test]
    fn merged_is_translated_for_the_server_and_filtered_here() {
        assert_eq!(server_state("merged"), "closed");
        assert_eq!(server_state("open"), "open");
        assert_eq!(server_state("all"), "all");

        let merged = pr(1, "closed", true);
        let abandoned = pr(2, "closed", false);
        assert!(keep(&merged, &args(&["gea", "-s", "merged"]), None));
        assert!(!keep(&abandoned, &args(&["gea", "-s", "merged"]), None));
        // ...and `closed` is the complement, so the two answers are disjoint.
        assert!(!keep(&merged, &args(&["gea", "-s", "closed"]), None));
        assert!(keep(&abandoned, &args(&["gea", "-s", "closed"]), None));
        // `all` keeps both.
        assert!(keep(&merged, &args(&["gea", "-s", "all"]), None));
        assert!(keep(&abandoned, &args(&["gea", "-s", "all"]), None));
    }

    #[test]
    fn the_assignee_filter_looks_at_both_shapes_the_api_sends() {
        let mut one = pr(1, "open", false);
        one.assignee = Some(User { login: "alice".to_owned(), ..User::default() });
        let mut many = pr(2, "open", false);
        many.assignees = vec![User { login: "Bob".to_owned(), ..User::default() }];

        assert!(keep(&one, &args(&["gea"]), Some("alice")));
        // Logins are compared case-insensitively, as Gitea treats them.
        assert!(keep(&many, &args(&["gea"]), Some("bob")));
        assert!(!keep(&one, &args(&["gea"]), Some("bob")));
    }

    #[test]
    fn the_search_filter_covers_the_title_and_the_body() {
        let mut p = pr(1, "open", false);
        p.body = "fixes the PARSER".to_owned();
        assert!(keep(&p, &args(&["gea", "-S", "parser"]), None));
        assert!(keep(&p, &args(&["gea", "-S", "change 1"]), None));
        assert!(!keep(&p, &args(&["gea", "-S", "unrelated"]), None));
    }

    #[test]
    fn table_snapshots_for_a_terminal_and_a_pipe() {
        let prs = vec![pr(3, "open", false), pr(1, "closed", true)];
        let mut report = String::from("== tty\n");
        report.push_str(&table(&prs, &Term::tty(80)));
        report.push_str("== piped\n");
        report.push_str(&table(&prs, &Term::piped()));
        insta::assert_snapshot!(report);
    }

    #[test]
    fn the_default_state_is_open() {
        assert_eq!(args(&["gea"]).state, "open");
    }
}

//! Finding a pull request, and the words used to describe one.
//!
//! # How a pull request is named
//!
//! Every `gea pr` verb takes the same optional selector, and it accepts four shapes because those
//! are the four things people have to hand:
//!
//! | you type | what happens |
//! | --- | --- |
//! | `42`, `#42` | that number |
//! | `https://…/pulls/42` | that number, in the repository the URL names |
//! | `my-branch` | the pull request whose head is that branch |
//! | *nothing* | the pull request for the branch you are on |
//!
//! The last one is the whole reason this group exists. `gea pr merge` in a checkout, with no
//! argument, is the single most common thing anybody will type at this tool.
//!
//! # Drafts are a title prefix
//!
//! Gitea has **no draft field and no `ready` endpoint**. A pull request is a draft when its title
//! starts with one of the instance's work-in-progress prefixes — `WIP:` or `[WIP]` by default,
//! configurable as `repository.pull-request.WORK_IN_PROGRESS_PREFIXES`. `PullRequest.draft` is
//! derived from the title by the server, and `--draft` and `gea pr ready` are implemented by
//! editing the title. That is surprising enough to be worth stating once, here, rather than in five
//! places.

use futures::StreamExt;
use gitea_client::Api;
use gitea_core::types::RepoSlug;
use gitea_core::types::ids::IssueIndex;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::PullRequest;

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::{autocolor, paint, style_by_name};
use crate::output::template::funcs::timeago;
use crate::runtime::Runtime;

/// Gitea's default work-in-progress title prefixes.
///
/// Matched case-insensitively, and the instance may be configured with others; a title we do not
/// recognise simply is not treated as a draft, which is the safe direction — the server's own
/// `draft` flag is what the *display* uses, and this list only drives the two commands that have to
/// *write* a prefix.
pub const WIP_PREFIXES: &[&str] = &["WIP:", "[WIP]"];

/// How many pull requests to scan when matching a branch name.
///
/// A branch lookup that walked the whole collection would make `gea pr view` on a repository with
/// 4 000 pull requests take a minute. The server-side `head` filter is tried first and this is only
/// the fallback's bound.
const BRANCH_SCAN: usize = 100;

/// A pull request, and the repository it belongs to.
///
/// The slug is carried alongside because a URL selector can name a *different* repository from the
/// resolved context, and every follow-up call needs the one the pull request actually lives in.
pub struct Found {
    pub slug: RepoSlug,
    pub pr: PullRequest,
}

impl Found {
    pub fn index(&self) -> i64 {
        self.pr.number.get()
    }
}

/// Resolve the selector. See the module docs for the four shapes.
pub async fn find(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    selector: Option<&str>,
) -> Result<Found> {
    let context = rt.repo(globals);

    if let Some(raw) = selector {
        if let Some((slug, index)) = parse_url(raw) {
            let pr = api.repo().get_pull_request(&slug.owner, &slug.name, index.get()).await?;
            return Ok(Found { slug, pr });
        }
        let slug = context?.slug.clone();
        if let Ok(index) = raw.parse::<IssueIndex>() {
            let pr = api.repo().get_pull_request(&slug.owner, &slug.name, index.get()).await?;
            return Ok(Found { slug, pr });
        }
        let pr = by_branch(api, &slug, raw).await?;
        return Ok(Found { slug, pr });
    }

    let slug = context?.slug.clone();
    let branch = rt.git().current_branch()?.ok_or_else(|| {
        Error::new(ErrorKind::Usage(
            "no pull request was named and HEAD is detached, so there is no branch to look one up \
             by; pass a number, a URL, or a branch name"
                .to_owned(),
        ))
    })?;
    let pr = by_branch(api, &slug, &branch).await?;
    Ok(Found { slug, pr })
}

/// `https://host/owner/name/pulls/42`, in any of the shapes people paste.
///
/// Parsed here rather than through `RepoRef` because a pull request URL has three path segments
/// after the slug and `RepoRef` would read `pulls` as the repository name.
pub fn parse_url(raw: &str) -> Option<(RepoSlug, IssueIndex)> {
    let rest = raw.split_once("://")?.1;
    let (_authority, path) = rest.split_once('/')?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // `owner/name/pulls/42`, possibly with a subpath install in front of it, and possibly with
    // `/files` or `/commits` on the end.
    let at = segments.iter().position(|s| *s == "pulls" || *s == "pull")?;
    let index: IssueIndex = segments.get(at + 1)?.parse().ok()?;
    let owner = segments.get(at.checked_sub(2)?)?;
    let name = segments.get(at - 1)?;
    Some((RepoSlug::new(*owner, *name), index))
}

/// The pull request whose head branch is `branch`.
///
/// A scan, because Gitea's list route has no `head` filter — and a pull request opened from a
/// *fork* records its head as `owner:branch` on some paths, so an exact-match lookup would miss
/// it anyway.
async fn by_branch(api: &Api, slug: &RepoSlug, branch: &str) -> Result<PullRequest> {
    let mut candidates: Vec<PullRequest> = Vec::new();
    let all = gitea_client::query::RepoListPullRequestsQuery::default().with_state("all");
    let mut stream = api.repo().list_pull_requests(&slug.owner, &slug.name, &all).take(BRANCH_SCAN);
    while let Some(item) = stream.next().await {
        let pr = item?;
        if head_matches(&pr, branch) {
            candidates.push(pr);
        }
    }

    pick_best(candidates, slug, branch)
}

/// Whether the pull request arrived over AGit, with no branch in any repository.
///
/// Gitea's API has no field for this. What it does is blank the head's `label` (the branch
/// name) for an AGit pull request and point `ref` at `refs/pull/<n>/head`; an ordinary pull
/// request whose branch was since deleted keeps its `label`, which is what tells the two apart.
pub fn is_agit(pr: &PullRequest) -> bool {
    pr.head.as_ref().is_some_and(|h| h.label.is_empty() && h.r#ref.starts_with("refs/pull/"))
}

/// Whether a pull request's head is `branch`, in either of the two forms Gitea reports.
pub fn head_matches(pr: &PullRequest, branch: &str) -> bool {
    let Some(head) = &pr.head else { return false };
    head.r#ref == branch
        || head.label == branch
        // `owner:branch`, which is how a fork's head is labelled.
        || head.label.rsplit_once(':').is_some_and(|(_, b)| b == branch)
}

/// Prefer an open pull request over a closed one; among equals, the highest number.
///
/// A branch that has been reused — merged once, then reopened for a follow-up — has several pull
/// requests, and acting on the merged one is a silent mistake. Newest-open-first is what a human
/// means by "the pull request for this branch".
fn pick_best(
    mut candidates: Vec<PullRequest>,
    slug: &RepoSlug,
    branch: &str,
) -> Result<PullRequest> {
    candidates.retain(|pr| head_matches(pr, branch));
    candidates.sort_by_key(|pr| {
        let openness = if pr.state.as_str() == "open" { 0 } else { 1 };
        (openness, -pr.number.get())
    });
    candidates.into_iter().next().ok_or_else(|| {
        Error::new(ErrorKind::ResourceNotFound {
            kind: "pull request",
            id: format!("head branch {branch}"),
            slug: Some(slug.to_string()),
            // Discovered locally: the listing succeeded and nothing in it matched.
            server_message: None,
        })
    })
}

// -------------------------------------------------------------------------------------- vocabulary

/// `open`, `draft`, `merged` or `closed`.
///
/// **`merged` is not a state Gitea has.** `PullRequest.state` is `open`/`closed`, and a merged
/// pull request is a closed one with `merged: true`. Collapsing that into one word here is what lets
/// the table column, `-s/--state merged`, and the detail view all agree.
pub fn state_of(pr: &PullRequest) -> &'static str {
    if pr.merged {
        "merged"
    } else if pr.state.as_str() == "closed" {
        "closed"
    } else if pr.draft {
        "draft"
    } else {
        "open"
    }
}

/// Whether a title carries a work-in-progress prefix.
pub fn is_wip(title: &str) -> bool {
    let lower = title.trim_start().to_ascii_lowercase();
    WIP_PREFIXES.iter().any(|p| lower.starts_with(&p.to_ascii_lowercase()))
}

/// The title with any work-in-progress prefix removed — `gea pr ready`.
pub fn strip_wip(title: &str) -> String {
    let trimmed = title.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in WIP_PREFIXES {
        if lower.starts_with(&prefix.to_ascii_lowercase()) {
            return trimmed[prefix.len()..].trim_start().to_owned();
        }
    }
    trimmed.to_owned()
}

/// The title with a work-in-progress prefix added — `--draft`, and `gea pr ready --undo`.
pub fn add_wip(title: &str) -> String {
    if is_wip(title) { title.trim_start().to_owned() } else { format!("WIP: {}", title.trim()) }
}

/// The `NUMBER  TITLE  BRANCH  STATE  UPDATED` row shared by `pr list` and `pr status`.
pub fn row(pr: &PullRequest, term: &Term) -> Vec<String> {
    vec![
        format!("#{}", pr.number),
        pr.title.clone(),
        pr.head.as_ref().map(|h| h.label.clone()).unwrap_or_default(),
        autocolor(term, state_of(pr)),
        pr.updated_at.map(|ts| timeago(&ts.to_string())).unwrap_or_default(),
    ]
}

/// The detail view, shared by `pr view` and the tail of `pr create`.
pub fn detail(pr: &PullRequest, term: &Term, body: bool) -> String {
    let bold = style_by_name("bold").unwrap_or_default();
    let dim = style_by_name("gray").unwrap_or_default();
    let mut out = String::new();

    out.push_str(&paint(term, bold, &pr.title));
    out.push_str(&paint(term, dim, &format!(" #{}", pr.number)));
    out.push('\n');

    let author = pr.user.as_ref().map(|u| u.login.clone()).unwrap_or_default();
    let base = pr.base.as_ref().map(|b| b.r#ref.clone()).unwrap_or_default();
    let head = pr.head.as_ref().map(|h| h.label.clone()).unwrap_or_default();
    out.push_str(&paint(
        term,
        dim,
        &format!(
            "{} • {author} wants to merge into {base} from {head} • +{} -{}\n",
            state_of(pr),
            pr.additions,
            pr.deletions
        ),
    ));

    let mut facts: Vec<(&str, String)> = Vec::new();
    if !pr.labels.is_empty() {
        facts.push((
            "labels",
            pr.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>().join(", "),
        ));
    }
    if !pr.assignees.is_empty() {
        facts.push((
            "assignees",
            pr.assignees.iter().map(|u| u.login.clone()).collect::<Vec<_>>().join(", "),
        ));
    }
    if !pr.requested_reviewers.is_empty() {
        facts.push((
            "reviewers",
            pr.requested_reviewers.iter().map(|u| u.login.clone()).collect::<Vec<_>>().join(", "),
        ));
    }
    if let Some(m) = &pr.milestone {
        facts.push(("milestone", m.title.clone()));
    }
    // An AGit pull request has no branch in the repository and no fork. Worth saying, because it
    // changes what `pr checkout` and `pr merge -d` can do.
    if is_agit(pr) {
        facts.push(("flow", "AGit (no head branch in the repository)".to_owned()));
    }
    if !facts.is_empty() {
        let mut table = crate::output::table::Table::new(term);
        for (k, v) in facts {
            table.row([k.to_owned(), v]);
        }
        out.push('\n');
        out.push_str(&table.render_to_string());
    }

    if body {
        out.push('\n');
        if pr.body.trim().is_empty() {
            out.push_str(&paint(term, dim, "No description provided.\n"));
        } else {
            for line in pr.body.lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
    }

    out.push_str(&paint(
        term,
        dim,
        &format!("\nView this pull request on the web: {}\n", pr.html_url),
    ));
    out
}

/// Turn `-l/--label` names into the ids `CreatePullRequestOption.labels` wants.
///
/// One resolver, in the issue group, because Gitea models a pull request **as** an issue: the
/// labels are the same objects on the same routes. The copy that used to live here searched only
/// the repository's own labels, so `-l <an org label>` was refused for a label the server would
/// have taken — the divergence that having two of these produces.
pub use crate::cmd::issue::shared::label_ids;

/// Turn a `-m/--milestone` title into the id the API wants.
pub async fn milestone_id(api: &Api, slug: &RepoSlug, title: &str) -> Result<i64> {
    // A number is accepted as itself: somebody who already has the id should not be forced to look
    // up a title for it.
    if let Ok(id) = title.parse::<i64>() {
        return Ok(id);
    }
    let query = gitea_client::query::IssueGetMilestonesListQuery::default().with_state("all");
    let mut stream = api.issue().get_milestones_list(&slug.owner, &slug.name, &query);
    let mut titles: Vec<String> = Vec::new();
    while let Some(item) = stream.next().await {
        let m = item?;
        if m.title.eq_ignore_ascii_case(title) {
            return Ok(m.id.get());
        }
        titles.push(m.title);
    }
    Err(Error::new(ErrorKind::Usage(format!(
        "{slug} has no milestone called {title:?}.\nit has: {}",
        if titles.is_empty() { "none at all".to_owned() } else { titles.join(", ") }
    ))))
}

/// Do not print the human view when `--json`/`--jq`/`--template` asked for machine output.
pub fn emit_or(
    rt: &Runtime,
    globals: &GlobalOpts,
    wanted: &support::machine::Wanted,
    pr: &PullRequest,
    human: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match wanted {
        support::machine::Wanted::Machine(m) => {
            support::machine::emit(rt, globals, m, support::to_value(pr)?)
        }
        _ => human(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_model::{PrBranchInfo, StateType};

    fn pr(number: i64, state: &str, merged: bool, head: &str) -> PullRequest {
        PullRequest {
            number: IssueIndex::new(number),
            state: StateType::from(state),
            merged,
            head: Some(PrBranchInfo {
                r#ref: head.to_owned(),
                label: head.to_owned(),
                ..PrBranchInfo::default()
            }),
            ..PullRequest::default()
        }
    }

    /// Bug this prevents: `gea pr merge` acting on the *merged* pull request for a reused branch
    /// instead of the open one. That is silent — the command succeeds against the wrong object.
    #[test]
    fn an_open_pull_request_wins_over_a_merged_one_on_the_same_branch() {
        let candidates = vec![pr(9, "closed", true, "feature"), pr(4, "open", false, "feature")];
        let chosen = pick_best(candidates, &RepoSlug::new("o", "r"), "feature").expect("a match");
        assert_eq!(chosen.number.get(), 4);
    }

    /// With nothing open, the newest closed one is the answer — not the oldest.
    #[test]
    fn among_closed_ones_the_newest_wins() {
        let candidates = vec![pr(4, "closed", true, "feature"), pr(9, "closed", false, "feature")];
        let chosen = pick_best(candidates, &RepoSlug::new("o", "r"), "feature").expect("a match");
        assert_eq!(chosen.number.get(), 9);
    }

    #[test]
    fn no_match_is_a_not_found_with_exit_code_five() {
        let e = pick_best(Vec::new(), &RepoSlug::new("o", "r"), "nope").unwrap_err();
        assert_eq!(e.exit_code(), 5);
        assert!(e.to_string().contains("nope"), "{e}");
    }

    /// Bug this prevents: a pull request from a fork not being found by its branch name, because
    /// its head is labelled `contributor:branch` rather than `branch`.
    #[test]
    fn a_forks_head_label_still_matches_the_branch_name() {
        let mut forked = pr(1, "open", false, "feature");
        forked.head.as_mut().expect("head").label = "contributor:feature".to_owned();
        assert!(head_matches(&forked, "feature"));
        assert!(!head_matches(&forked, "other"));
    }

    #[test]
    fn pull_request_urls_are_parsed_in_the_shapes_people_paste() {
        let cases = [
            "https://git.example.org/them/proj/pulls/42",
            "https://git.example.org/them/proj/pulls/42/files",
            // A subpath install: the prefix is not part of the slug.
            "https://example.org/gitea/them/proj/pulls/42",
            // GitHub's spelling, because somebody will paste it out of habit.
            "https://git.example.org/them/proj/pull/42",
        ];
        for raw in cases {
            let (slug, index) = parse_url(raw).unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(slug.to_string(), "them/proj", "{raw}");
            assert_eq!(index.get(), 42, "{raw}");
        }
        assert_eq!(parse_url("them/proj"), None);
        assert_eq!(parse_url("https://git.example.org/them/proj"), None);
    }

    /// Bug this prevents: `merged` being reported as `closed`, which is technically what Gitea
    /// stores and completely wrong for a human reading a list.
    #[test]
    fn merged_is_distinguished_from_closed() {
        assert_eq!(state_of(&pr(1, "closed", true, "b")), "merged");
        assert_eq!(state_of(&pr(1, "closed", false, "b")), "closed");
        assert_eq!(state_of(&pr(1, "open", false, "b")), "open");
        let draft = PullRequest { draft: true, ..pr(1, "open", false, "b") };
        assert_eq!(state_of(&draft), "draft");
    }

    /// The draft-is-a-title-prefix rule, in both directions. Getting `strip_wip` wrong leaves a
    /// pull request marked draft after `gea pr ready` reported success.
    #[test]
    fn work_in_progress_prefixes_round_trip() {
        for wip in ["WIP: add the thing", "wip: add the thing", "[WIP] add the thing"] {
            assert!(is_wip(wip), "{wip}");
            assert_eq!(strip_wip(wip), "add the thing", "{wip}");
        }
        assert!(!is_wip("add the thing"));
        assert_eq!(strip_wip("add the thing"), "add the thing");
        assert_eq!(add_wip("add the thing"), "WIP: add the thing");
        // Idempotent: marking a draft twice must not produce `WIP: WIP: …`.
        assert_eq!(add_wip("WIP: add the thing"), "WIP: add the thing");
    }
}

//! `gea pr view` — one pull request, in words.

use std::io::Write;

use clap::Args as ClapArgs;
use gitea_core::Result;

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::color::{paint, style_by_name};
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a pull request.

Without an argument, uses the pull request for the current branch.

  gea pr view
  gea pr view 42 --comments
  gea pr view my-branch
  gea pr view -w
  gea pr view --json state,mergeable,head")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Open in a browser instead of printing
    #[arg(short = 'w', long)]
    pub web: bool,

    /// Print the comments too
    #[arg(short = 'c', long)]
    pub comments: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        if args.web {
            return support::open_web(&rt, &found.pr.html_url);
        }

        let comments = comments_for(&api, &found, args.comments && !is_machine(&wanted)).await?;

        common::emit_or(&rt, globals, &wanted, &found.pr, || {
            let mut out = std::io::stdout().lock();
            out.write_all(common::detail(&found.pr, rt.term(), true).as_bytes())?;
            if args.comments {
                out.write_all(render_comments(&comments, rt.term()).as_bytes())?;
            }
            out.flush()?;
            Ok(())
        })
    })
}

/// Whether the machine path is what will be printed.
///
/// `Wanted::Listed` never reaches here — `run` returns on it — so the only question left is
/// whether `emit_or` will render the pull request as JSON instead of calling the human closure.
fn is_machine(wanted: &support::machine::Wanted) -> bool {
    matches!(wanted, support::machine::Wanted::Machine(_))
}

/// The comments, but only when something is going to print them.
///
/// Bug this prevents: `gea pr view 42 -c --json state` paying for a comments request whose answer
/// is then discarded. `emit_or` renders the pull request alone on the machine path, so `-c` is a
/// display flag and nothing more — and `pr view --json` is the shape a script polls in a loop.
/// `gea issue view` has always returned before its comments fetch; this is the same guard.
///
/// Takes a bare [`Api`] rather than the [`Runtime`] it comes from, so a `FakeTransport` test can
/// assert the request this does *not* make without a network or a config file.
async fn comments_for(
    api: &gitea_client::Api,
    found: &common::Found,
    wanted: bool,
) -> Result<Vec<gitea_model::Comment>> {
    if !wanted {
        return Ok(Vec::new());
    }
    let query = gitea_client::query::IssueGetCommentsQuery::default();
    api.issue().get_comments(&found.slug.owner, &found.slug.name, found.index(), &query).await
}

/// Comments, oldest first, each with its author and age.
///
/// Not routed through `Table`: a comment body is prose of arbitrary length, and a table would
/// truncate it to fit a column — which is the one thing that must not happen to the text somebody is
/// reading the command to see.
fn render_comments(comments: &[gitea_model::Comment], term: &crate::output::Term) -> String {
    let dim = style_by_name("gray").unwrap_or_default();
    let bold = style_by_name("bold").unwrap_or_default();
    if comments.is_empty() {
        return paint(term, dim, "\nNo comments.\n");
    }
    let mut out = String::new();
    for comment in comments {
        let who = comment.user.as_ref().map(|u| u.login.clone()).unwrap_or_default();
        let when = comment
            .created_at
            .map(|ts| crate::output::template::funcs::timeago(&ts.to_string()))
            .unwrap_or_default();
        out.push('\n');
        out.push_str(&paint(term, bold, &who));
        out.push_str(&paint(term, dim, &format!(" • {when}\n")));
        for line in comment.body.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use crate::output::Term;
    use gitea_core::types::ids::{CommentId, IssueIndex};
    use gitea_model::{Comment, PrBranchInfo, PullRequest, StateType, User};

    fn pr() -> PullRequest {
        PullRequest {
            number: IssueIndex::new(42),
            title: "Teach the parser about tabs".to_owned(),
            body: "Fixes #12\n\nThe lexer treated a tab as one column.".to_owned(),
            state: StateType::Open,
            additions: 24,
            deletions: 3,
            user: Some(User { login: "alice".to_owned(), ..User::default() }),
            base: Some(PrBranchInfo { r#ref: "main".to_owned(), ..PrBranchInfo::default() }),
            head: Some(PrBranchInfo {
                r#ref: "tabs".to_owned(),
                label: "alice:tabs".to_owned(),
                ..PrBranchInfo::default()
            }),
            html_url: "https://git.example.org/them/proj/pulls/42".to_owned(),
            ..PullRequest::default()
        }
    }

    #[test]
    fn detail_view_snapshot() {
        insta::assert_snapshot!(common::detail(&pr(), &Term::tty(80), true));
    }

    /// An AGit pull request has no head branch in the repository, which changes what `pr checkout`
    /// and `pr merge -d` can do — so the view has to say so.
    #[test]
    fn an_agit_pull_request_is_labelled_as_one() {
        let agit = PullRequest {
            head: Some(PrBranchInfo {
                r#ref: "refs/pull/42/head".to_owned(),
                ..PrBranchInfo::default()
            }),
            ..pr()
        };
        let out = common::detail(&agit, &Term::tty(80), false);
        assert!(out.contains("AGit"), "{out}");
    }

    /// Bug this prevents: an empty description rendering as a blank gap, so the reader cannot tell
    /// it apart from output that got cut off.
    #[test]
    fn an_empty_body_says_so() {
        let empty = PullRequest { body: String::new(), ..pr() };
        let out = common::detail(&empty, &Term::tty(80), true);
        assert!(out.contains("No description provided."), "{out}");
    }

    fn found() -> common::Found {
        common::Found { slug: gitea_core::types::RepoSlug::new("them", "proj"), pr: pr() }
    }

    /// Bug this prevents — the reported one. `gea pr view 42 -c --json state` fetched the
    /// comments and then threw them away: `emit_or` prints the pull request alone on the machine
    /// path, so `-c` never reaches a renderer. Two requests for one answer, in the shape a script
    /// polls in a loop. `gea issue view` returns before its comments fetch; this is the same
    /// guard, and this test is what keeps it.
    #[tokio::test]
    async fn the_machine_path_does_not_fetch_comments_it_will_not_print() {
        let fake = std::sync::Arc::new(testing::on(
            testing::transport(),
            "GET",
            "/api/v1/repos/them/proj/issues/42/comments",
            gitea_core::http::transport::Canned::json(200, "[]"),
        ));
        let api = testing::api_at(testing::EXAMPLE, fake.clone());

        // `-c --json state`: the comments are not wanted, so nothing is asked for.
        assert!(comments_for(&api, &found(), false).await.unwrap().is_empty());
        assert_eq!(fake.call_count(), 0, "--json must not pay for comments: {:?}", fake.calls());

        // ...and the human `-c` still gets them, in one request.
        comments_for(&api, &found(), true).await.unwrap();
        assert_eq!(fake.call_count(), 1);
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/them/proj/issues/42/comments");
    }

    /// The gate `run` feeds `comments_for`, checked for polarity: reading it backwards would
    /// swap the two paths and make the human `-c` the one that prints nothing.
    #[test]
    fn only_machine_output_suppresses_the_comments() {
        assert!(!is_machine(&support::machine::Wanted::Human), "the human view prints comments");
        let machine = support::machine::Wanted::Machine(
            support::machine::Triad::compile(&GlobalOpts::default(), None).unwrap(),
        );
        assert!(is_machine(&machine), "--json/--jq/--template discard them");
    }

    #[test]
    fn comments_render_with_author_and_age() {
        let comments = vec![Comment {
            id: CommentId::new(1),
            body: "Looks right to me.".to_owned(),
            user: Some(User { login: "bob".to_owned(), ..User::default() }),
            ..Comment::default()
        }];
        let out = render_comments(&comments, &Term::tty(80));
        assert!(out.contains("bob"), "{out}");
        assert!(out.contains("  Looks right to me."), "{out}");
        assert!(render_comments(&[], &Term::tty(80)).contains("No comments"));
    }
}

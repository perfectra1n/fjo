//! `fjo pr review` — approve, comment, or request changes.
//!
//! Forgejo takes the verdict as one `event` value (`APPROVED`, `COMMENT`, `REQUEST_CHANGES`), so the
//! three `gh`-shaped flags are aliases for it and are mutually exclusive by declaration rather than
//! by a runtime check.
//!
//! One rule the API enforces and nothing in the flags suggests: **`REQUEST_CHANGES` and `COMMENT`
//! require a body.** An empty one is a 422 whose message is not obvious, so it is caught here with
//! a prompt on a terminal and an error naming the flag without one. Inline comments stand in for
//! the words of a `COMMENT` review but not of a `REQUEST_CHANGES` one: measured on 16.0.5, Forgejo
//! accepts the first with an empty body and comments, and refuses the second with a 422.
//!
//! # Inline comments
//!
//! `--line PATH:LINE=TEXT` (and `--old-line` for a removed line) travel in the review's own
//! `comments` array, so the review and its comments arrive together. With no verdict flag they
//! make a `COMMENT` review, the one verdict that cannot be a mistake.
//!
//! Each path is checked against the files the pull request changes before anything is sent.
//! Forgejo answers 200 to a comment on a path the diff does not contain, and files it where no
//! view will ever show it (measured on 16.0.5), so without the check a typo is a comment that
//! silently never existed.

use clap::Args as ClapArgs;
use forgejo_core::{Error, ErrorKind, Result};
use forgejo_model::{CreatePullReviewComment, CreatePullReviewOptions, ReviewStateType};
use futures::StreamExt;

use super::common;
use crate::cmd::support::{self, BodyOpts};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Review a pull request.

Choose one of --approve, --comment, or --request-changes. Change requests require a
body; comments require a body or at least one inline comment. Supply a body with -b,
-F, or the interactive prompt.

Add inline comments with --line PATH:LINE=TEXT, or --old-line for a removed line.
Both can be repeated. With inline comments and no verdict, the review is a comment.

  fjo pr review --approve
  fjo pr review 42 --request-changes -b 'the lexer still mishandles CRLF'
  fjo pr review --comment -F notes.md
  fjo pr review --approve -F -
  fjo pr review --line src/lex.rs:42='CRLF is dropped here' --line src/lex.rs:57='and here'
  fjo pr review -r -b 'see inline' --old-line README.md:3='why remove this?'")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Approve it
    #[arg(short = 'a', long, conflicts_with_all = ["comment", "request_changes"])]
    pub approve: bool,

    /// Leave a review comment without a verdict
    #[arg(short = 'c', long, conflicts_with = "request_changes")]
    pub comment: bool,

    /// Request changes
    #[arg(short = 'r', long)]
    pub request_changes: bool,

    /// Comment on a line of the new version: PATH:LINE=TEXT. Repeatable
    #[arg(long = "line", value_name = "PATH:LINE=TEXT", value_parser = parse_line_comment)]
    pub lines: Vec<LineComment>,

    /// Comment on a line of the old version, for a removed line: PATH:LINE=TEXT. Repeatable
    #[arg(long = "old-line", value_name = "PATH:LINE=TEXT", value_parser = parse_line_comment)]
    pub old_lines: Vec<LineComment>,

    #[command(flatten)]
    pub body: BodyOpts,
}

/// One `--line` or `--old-line` value, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    pub path: String,
    pub line: i64,
    pub body: String,
}

/// `PATH:LINE=TEXT`, split at the first `=` and then at the last `:` before it.
///
/// That order is what lets the text contain both characters freely, which review prose does; the
/// cost is a path containing `=`, which nobody has.
fn parse_line_comment(raw: &str) -> std::result::Result<LineComment, String> {
    let shape = || format!("{raw:?} is not PATH:LINE=TEXT");
    let (spec, body) = raw.split_once('=').ok_or_else(shape)?;
    let (path, line) = spec.rsplit_once(':').ok_or_else(shape)?;
    let line: i64 = line.trim().parse().map_err(|_| shape())?;
    if path.is_empty() || line < 1 {
        return Err(format!("{raw:?}: the path must be non-empty and the line at least 1"));
    }
    if body.trim().is_empty() {
        return Err(format!("{raw:?}: an inline comment needs some text after the ="));
    }
    Ok(LineComment { path: path.to_owned(), line, body: body.to_owned() })
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, forgejo_client::fields::FIELDS_PULL_REVIEW)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let event = event_of(args)?;
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        let comments = inline_comments(&api, &found, args).await?;
        let needs_body = needs_body(&event, !comments.is_empty());

        let supplied = args.body.resolve(&rt, "")?;
        let body = match supplied.body {
            Some(b) => b,
            None if !needs_body => String::new(),
            None if support::can_prompt(&rt) => support::interact::ask("Review body", None)?,
            None => {
                return Err(Error::new(ErrorKind::Usage(format!(
                    "Forgejo requires a body for a {} review; pass -b/--body, -F/--body-file, or \
                     -e/--editor",
                    event.as_str()
                ))));
            }
        };
        if needs_body && body.trim().is_empty() {
            return Err(Error::new(ErrorKind::Cancelled));
        }

        let options = CreatePullReviewOptions {
            body: Some(body),
            event: Some(event.clone()),
            comments: (!comments.is_empty()).then_some(comments),
            ..CreatePullReviewOptions::default()
        };
        let review = api
            .repo()
            .create_pull_review(&found.slug.owner, &found.slug.name, found.index(), &options)
            .await?;

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&review)?)
            }
            _ => {
                support::note(
                    rt.term(),
                    &format!("{} review submitted on #{}", event.as_str(), found.pr.number),
                );
                Ok(())
            }
        }
    })
}

/// The `--line` and `--old-line` comments, once every path is known to be in the diff.
///
/// Costs a file listing only when there are inline comments to check.
async fn inline_comments(
    api: &forgejo_client::Api,
    found: &common::Found,
    args: &Args,
) -> Result<Vec<CreatePullReviewComment>> {
    if args.lines.is_empty() && args.old_lines.is_empty() {
        return Ok(Vec::new());
    }
    let query = forgejo_client::query::RepoGetPullRequestFilesQuery::default();
    let mut stream = api.repo().get_pull_request_files(
        &found.slug.owner,
        &found.slug.name,
        found.index(),
        &query,
    );
    let mut changed = Vec::new();
    while let Some(file) = stream.next().await {
        let file = file?;
        changed.push(file.filename);
        if !file.previous_filename.is_empty() {
            changed.push(file.previous_filename);
        }
    }
    let new = args.lines.iter().map(|c| (c, true));
    let old = args.old_lines.iter().map(|c| (c, false));
    new.chain(old)
        .map(|(c, new_side)| {
            if !changed.contains(&c.path) {
                return Err(support::usage(format!(
                    "{} is not among the files #{} changes, and Forgejo would file a comment on \
                     it where nobody can see it; `fjo pr files` lists the ones it does",
                    c.path, found.pr.number
                )));
            }
            // The other side is left out rather than sent as 0, which is what the API reads an
            // absent position as.
            Ok(CreatePullReviewComment {
                path: Some(c.path.clone()),
                body: Some(c.body.clone()),
                new_position: new_side.then_some(c.line),
                old_position: (!new_side).then_some(c.line),
                extra_lines_count: None,
            })
        })
        .collect()
}

/// Which verdict was asked for.
///
/// No default when there is nothing but a body. A `fjo pr review` that quietly approved would be
/// the worst possible default, and one that quietly commented would be a review nobody meant to
/// leave. Inline comments are different: they *are* a comment review, so they imply one.
pub(crate) fn event_of(args: &Args) -> Result<ReviewStateType> {
    let inline = !args.lines.is_empty() || !args.old_lines.is_empty();
    match (args.approve, args.comment || inline && !args.request_changes, args.request_changes) {
        (true, _, _) => Ok(ReviewStateType::Approved),
        (_, true, _) => Ok(ReviewStateType::Comment),
        (_, _, true) => Ok(ReviewStateType::RequestChanges),
        _ => Err(Error::new(ErrorKind::Usage(
            "say what the review is: -a/--approve, -c/--comment, or -r/--request-changes"
                .to_owned(),
        ))),
    }
}

/// Whether Forgejo will reject this review without a body.
///
/// Inline comments are enough for a `COMMENT` review and never for a `REQUEST_CHANGES` one: Forgejo
/// 16.0.5 answers the second `review event REQUEST_CHANGES requires a body` regardless.
pub(crate) fn needs_body(event: &ReviewStateType, inline: bool) -> bool {
    match event {
        ReviewStateType::Comment => !inline,
        ReviewStateType::RequestChanges => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;

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

    #[test]
    fn each_flag_maps_to_the_apis_event_value() {
        assert_eq!(event_of(&args(&["fjo", "-a"])).unwrap().as_str(), "APPROVED");
        assert_eq!(event_of(&args(&["fjo", "-c"])).unwrap().as_str(), "COMMENT");
        assert_eq!(event_of(&args(&["fjo", "-r"])).unwrap().as_str(), "REQUEST_CHANGES");
    }

    /// Bug this prevents: defaulting the verdict. Approving a pull request nobody meant to approve is
    /// not a recoverable mistake — a merge may follow automatically.
    #[test]
    fn a_review_with_no_verdict_is_a_usage_error() {
        let e = event_of(&args(&["fjo"])).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--approve"), "{e}");
    }

    #[test]
    fn two_verdicts_at_once_are_refused() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        for words in [&["fjo", "-a", "-c"][..], &["fjo", "-a", "-r"][..], &["fjo", "-c", "-r"][..]]
        {
            assert!(<Harness as clap::Parser>::try_parse_from(words).is_err(), "{words:?}");
        }
    }

    /// Bug this prevents: sending an empty body for a `COMMENT` review and surfacing Forgejo's 422
    /// instead of asking. Approval, by contrast, is legitimately wordless.
    #[test]
    fn only_a_comment_or_a_request_for_changes_needs_words() {
        assert!(!needs_body(&ReviewStateType::Approved, false));
        assert!(needs_body(&ReviewStateType::Comment, false));
        assert!(needs_body(&ReviewStateType::RequestChanges, false));
    }

    /// Bug this prevents — measured on Forgejo 16.0.5, where it differs from Gitea: a change request
    /// with inline comments and no body is a 422, so the prompt must still fire for one.
    #[test]
    fn inline_comments_excuse_a_comment_review_from_a_body_but_not_a_change_request() {
        assert!(!needs_body(&ReviewStateType::Comment, true));
        assert!(needs_body(&ReviewStateType::RequestChanges, true));
    }

    #[test]
    fn a_line_comment_splits_at_the_first_equals_and_the_last_colon_before_it() {
        let c = parse_line_comment("src/a:b.rs:12=use x == y: it reads better").unwrap();
        assert_eq!(c.path, "src/a:b.rs");
        assert_eq!(c.line, 12);
        assert_eq!(c.body, "use x == y: it reads better");
        for bad in ["src/a.rs=no line", "src/a.rs:x=text", "src/a.rs:0=text", ":3=t", "a.rs:3="] {
            assert!(parse_line_comment(bad).is_err(), "{bad}");
        }
    }

    /// Inline comments are a comment review on their own, and never turn into an approval; an
    /// explicit verdict still wins.
    #[test]
    fn inline_comments_imply_a_comment_review_unless_a_verdict_is_given() {
        let e = |words: &[&str]| event_of(&args(words)).unwrap().as_str().to_owned();
        assert_eq!(e(&["fjo", "--line", "a.rs:1=x"]), "COMMENT");
        assert_eq!(e(&["fjo", "--old-line", "a.rs:1=x", "-r"]), "REQUEST_CHANGES");
        assert_eq!(e(&["fjo", "--line", "a.rs:1=x", "-a"]), "APPROVED");
    }

    fn found() -> common::Found {
        let pr = forgejo_model::PullRequest {
            number: forgejo_core::types::ids::IssueIndex::new(42),
            ..forgejo_model::PullRequest::default()
        };
        common::Found { slug: forgejo_core::types::RepoSlug::new("o", "r"), pr }
    }

    fn files_api() -> forgejo_client::Api {
        let files = r#"[{"filename":"src/new.rs","previous_filename":"src/old.rs","status":"renamed"},
                        {"filename":"README.md","status":"modified"}]"#;
        let fake = testing::on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/pulls/42/files",
            testing::one_page(files),
        );
        testing::api(std::sync::Arc::new(fake))
    }

    #[tokio::test]
    async fn inline_comments_carry_the_side_in_the_position_the_api_reads() {
        let a = args(&["fjo", "--line", "src/new.rs:3=new", "--old-line", "src/old.rs:9=old"]);
        let comments = inline_comments(&files_api(), &found(), &a).await.unwrap();
        let body = serde_json::to_value(&comments).unwrap();
        assert_eq!(
            body,
            serde_json::json!([
                {"path": "src/new.rs", "body": "new", "new_position": 3},
                {"path": "src/old.rs", "body": "old", "old_position": 9},
            ])
        );
    }

    /// Bug this prevents — measured on Gitea 28.1: a comment on a path outside the diff is accepted
    /// with a 200 and shown nowhere. The typo has to fail here, before the review is sent.
    #[tokio::test]
    async fn a_path_the_pull_request_does_not_change_is_refused_before_sending() {
        let a = args(&["fjo", "--line", "src/nwe.rs:3=typo"]);
        let e = inline_comments(&files_api(), &found(), &a).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("src/nwe.rs"), "{e}");
    }

    #[tokio::test]
    async fn a_review_without_inline_comments_lists_no_files() {
        let fake = std::sync::Arc::new(testing::transport());
        let api = testing::api(fake.clone());
        assert!(inline_comments(&api, &found(), &args(&["fjo", "-a"])).await.unwrap().is_empty());
        assert_eq!(fake.call_count(), 0);
    }
}

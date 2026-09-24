//! `gea pr review` — approve, comment, or request changes.
//!
//! Gitea takes the verdict as one `event` value (`APPROVED`, `COMMENT`, `REQUEST_CHANGES`), so the
//! three `gh`-shaped flags are aliases for it and are mutually exclusive by declaration rather than
//! by a runtime check.
//!
//! One rule the API enforces and nothing in the flags suggests: **`REQUEST_CHANGES` and `COMMENT`
//! require a body.** An empty one is a 422 whose message is not obvious, so it is caught here with
//! a prompt on a terminal and an error naming the flag without one.

use clap::Args as ClapArgs;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{CreatePullReviewOptions, ReviewStateType};

use super::common;
use crate::cmd::support::{self, BodyOpts};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Review a pull request.

Choose one of --approve, --comment, or --request-changes. Comments and change
requests require a body. Supply one with -b, -F, or the interactive prompt.

  gea pr review --approve
  gea pr review 42 --request-changes -b 'the lexer still mishandles CRLF'
  gea pr review --comment -F notes.md
  gea pr review --approve -F -")]
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

    #[command(flatten)]
    pub body: BodyOpts,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_PULL_REVIEW)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let event = event_of(args)?;
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        let supplied = args.body.resolve(&rt, "")?;
        let body = match supplied.body {
            Some(b) => b,
            None if !needs_body(&event) => String::new(),
            None if support::can_prompt(&rt) => support::interact::ask("Review body", None)?,
            None => {
                return Err(Error::new(ErrorKind::Usage(format!(
                    "Gitea requires a body for a {} review; pass -b/--body, -F/--body-file, or \
                     -e/--editor",
                    event.as_str()
                ))));
            }
        };
        if needs_body(&event) && body.trim().is_empty() {
            return Err(Error::new(ErrorKind::Cancelled));
        }

        let options = CreatePullReviewOptions {
            body: Some(body),
            event: Some(event.clone()),
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

/// Which verdict was asked for.
///
/// No default. A `gea pr review` that quietly approved would be the worst possible default, and one
/// that quietly commented would be a review nobody meant to leave.
pub(crate) fn event_of(args: &Args) -> Result<ReviewStateType> {
    match (args.approve, args.comment, args.request_changes) {
        (true, _, _) => Ok(ReviewStateType::Approved),
        (_, true, _) => Ok(ReviewStateType::Comment),
        (_, _, true) => Ok(ReviewStateType::RequestChanges),
        _ => Err(Error::new(ErrorKind::Usage(
            "say what the review is: -a/--approve, -c/--comment, or -r/--request-changes"
                .to_owned(),
        ))),
    }
}

/// Whether Gitea will reject this review without a body.
pub(crate) fn needs_body(event: &ReviewStateType) -> bool {
    matches!(event, ReviewStateType::Comment | ReviewStateType::RequestChanges)
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

    #[test]
    fn each_flag_maps_to_the_apis_event_value() {
        assert_eq!(event_of(&args(&["gea", "-a"])).unwrap().as_str(), "APPROVED");
        assert_eq!(event_of(&args(&["gea", "-c"])).unwrap().as_str(), "COMMENT");
        assert_eq!(event_of(&args(&["gea", "-r"])).unwrap().as_str(), "REQUEST_CHANGES");
    }

    /// Bug this prevents: defaulting the verdict. Approving a pull request nobody meant to approve is
    /// not a recoverable mistake — a merge may follow automatically.
    #[test]
    fn a_review_with_no_verdict_is_a_usage_error() {
        let e = event_of(&args(&["gea"])).unwrap_err();
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
        for words in [&["gea", "-a", "-c"][..], &["gea", "-a", "-r"][..], &["gea", "-c", "-r"][..]]
        {
            assert!(<Harness as clap::Parser>::try_parse_from(words).is_err(), "{words:?}");
        }
    }

    /// Bug this prevents: sending an empty body for a `COMMENT` review and surfacing Gitea's 422
    /// instead of asking. Approval, by contrast, is legitimately wordless.
    #[test]
    fn only_a_comment_or_a_request_for_changes_needs_words() {
        assert!(!needs_body(&ReviewStateType::Approved));
        assert!(needs_body(&ReviewStateType::Comment));
        assert!(needs_body(&ReviewStateType::RequestChanges));
    }
}

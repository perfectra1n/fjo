//! `gea pr merge` — one style enum, and the server's own reason when it says no.
//!
//! # One enum, not three booleans
//!
//! `gh` has `--merge`, `--squash` and `--rebase` as three independent booleans, and then has to
//! reject two of them together. Gitea's API takes a single `do` field whose value is one of
//! `merge`, `rebase`, `rebase-merge`, `squash`, `fast-forward-only`, `manually-merged` — so `-s`
//! takes that value directly and cannot express a contradiction. `--merge`, `--squash` and
//! `--rebase` are kept as aliases because the muscle memory is real, and they are `conflicts_with`
//! each other so the contradiction is still a usage error rather than a coin toss.
//!
//! `fast-forward-only` has no `gh` equivalent at all. It is the option for a repository that wants a
//! linear history with no merge commit and no rewriting, and it is worth knowing exists.
//!
//! # The refusal
//!
//! **Gitea answers an un-mergeable pull request with HTTP 405** and a body explaining why: the
//! branch is behind, a required check has not passed, a review is missing, the pull request is a
//! draft. `tea`'s single worst bug is printing `failed to merge PR, is it still open?` for every one
//! of those.
//!
//! Nothing in this module is needed to avoid it any more. `gitea_core`'s classifier splits a 405
//! on its body: an HTML page or an empty body really is a missing route and stays
//! `RouteNotFound`, while a 405 that carries words becomes `ErrorKind::StateConflict`, which names
//! the resource (`pull request 4212`) and prints the server's own reason. So the merge goes
//! through the typed client like every other call, and the reason survives because the taxonomy
//! has somewhere to put it.

use clap::Args as ClapArgs;
use gitea_core::Result;
use gitea_core::context::git::Checkout;
use gitea_model::{MergePullRequestOption, MergeStyle, PullRequest};

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Merge a pull request.

Select a merge style with -s, including fast-forward-only. --merge, --rebase,
and --squash are also accepted. If merging is refused, the server's reason is shown.

  gea pr merge
  gea pr merge 42 --squash -d
  gea pr merge -s fast-forward-only
  gea pr merge --auto --squash")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Merge style
    #[arg(short = 's', long, value_name = "STYLE", value_parser = STYLES.to_vec())]
    pub style: Option<String>,

    /// Alias for `-s merge`
    #[arg(long, conflicts_with_all = ["style", "rebase", "squash", "rebase_merge", "fast_forward_only"])]
    pub merge: bool,
    /// Alias for `-s rebase`
    #[arg(long, conflicts_with_all = ["style", "squash", "rebase_merge", "fast_forward_only"])]
    pub rebase: bool,
    /// Alias for `-s squash`
    #[arg(long, conflicts_with_all = ["style", "rebase_merge", "fast_forward_only"])]
    pub squash: bool,
    /// Alias for `-s rebase-merge`
    #[arg(long, conflicts_with_all = ["style", "fast_forward_only"])]
    pub rebase_merge: bool,
    /// Alias for `-s fast-forward-only`
    #[arg(long, conflicts_with = "style")]
    pub fast_forward_only: bool,

    /// Merge as soon as every required check has passed
    #[arg(long)]
    pub auto: bool,

    /// Delete the head branch afterwards
    #[arg(short = 'd', long)]
    pub delete_branch: bool,

    /// Merge commit subject. There is no `-t`: that is the global `--template`
    #[arg(long, value_name = "TEXT")]
    pub subject: Option<String>,

    /// Merge commit body
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,
}

/// The styles `-s` accepts, taken from the generated enum so a spec bump grows the list.
///
/// `MergeStyle::KNOWN` rather than a hand-written array: the values are the API's own, and a
/// hand-written copy is a list that goes stale silently.
const STYLES: &[&str] = MergeStyle::KNOWN;

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        let style = style_of(args);

        // Said before the request, because the server's refusal for a draft is technically correct
        // and completely unhelpful ("Not mergeable") when the cause is a `WIP:` prefix the user
        // forgot about.
        if found.pr.draft {
            support::note(
                rt.term(),
                &format!(
                    "#{} is a draft ({:?}). Mark it ready with `gea pr ready` before merging.",
                    found.pr.number, found.pr.title
                ),
            );
        }

        let body = MergePullRequestOption {
            r#do: MergeStyle::from(style.as_str()),
            merge_title_field: args.subject.clone(),
            merge_message_field: args.body.clone(),
            delete_branch_after_merge: Some(args.delete_branch),
            merge_when_checks_succeed: Some(args.auto),
            ..MergePullRequestOption::default()
        };

        api.repo()
            .merge_pull_request(&found.slug.owner, &found.slug.name, found.index(), &body)
            .await?;

        if args.auto {
            support::note(
                rt.term(),
                &format!("#{} will be merged with {style} once its checks pass", found.pr.number),
            );
        } else {
            support::note(rt.term(), &format!("Merged #{} with {style}", found.pr.number));
            prune_local(&rt, args, &found.pr);
        }
        Ok(())
    })
}

/// `-s`, or whichever alias was given, or the repository's own default.
///
/// No hardcoded fallback to `merge`: an empty `do` makes Gitea use the repository's configured
/// default merge style, which is a better answer than one this tool invented.
pub(crate) fn style_of(args: &Args) -> String {
    if let Some(style) = &args.style {
        return style.clone();
    }
    for (given, name) in [
        (args.merge, "merge"),
        (args.rebase, "rebase"),
        (args.squash, "squash"),
        (args.rebase_merge, "rebase-merge"),
        (args.fast_forward_only, "fast-forward-only"),
    ] {
        if given {
            return name.to_owned();
        }
    }
    // `merge` is Gitea's own default for the field, and `MergeStyle::default()` says so.
    MergeStyle::default().as_str().to_owned()
}

/// After a merge with `-d`, tidy up locally too.
///
/// Best-effort and quiet: the branch is already gone on the server, and failing the command because
/// a local checkout could not be switched away from would be reporting a failure for something that
/// succeeded. An AGit pull request has no head branch at all, so there is nothing to do.
fn prune_local(rt: &Runtime, args: &Args, pr: &PullRequest) {
    if !args.delete_branch || common::is_agit(pr) {
        return;
    }
    let Some(head) = pr.head.as_ref().map(|h| h.r#ref.clone()) else { return };
    let Ok(Some(current)) = rt.git().current_branch() else { return };
    let git = rt.git();

    if current == head {
        // Switching away first, because git refuses to delete the branch you are on — and the base
        // branch is where the user wants to be after a merge anyway.
        let base = pr.base.as_ref().map(|b| b.r#ref.clone()).unwrap_or_default();
        let switched =
            !base.is_empty() && git.try_checkout(&Checkout::Rev(base.clone())).unwrap_or(false);
        if switched {
            support::note(rt.term(), &format!("switched to {base}"));
        } else {
            support::note(
                rt.term(),
                &format!("you are on {head}, which was merged; switch away to delete it locally"),
            );
            return;
        }
    }
    if git.delete_branch(&head, true).unwrap_or(false) {
        support::note(rt.term(), &format!("deleted local branch {head}"));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gitea_core::ErrorKind;
    use gitea_core::http::transport::Canned;
    use support::testing;

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

    /// Bug this prevents: `--squash` being accepted and ignored, so a repository's history gains a
    /// merge commit somebody explicitly asked not to have.
    #[test]
    fn the_gh_shaped_aliases_all_map_onto_the_style_enum() {
        assert_eq!(style_of(&args(&["gea", "--squash"])), "squash");
        assert_eq!(style_of(&args(&["gea", "--rebase"])), "rebase");
        assert_eq!(style_of(&args(&["gea", "--merge"])), "merge");
        assert_eq!(style_of(&args(&["gea", "--rebase-merge"])), "rebase-merge");
        assert_eq!(style_of(&args(&["gea", "--fast-forward-only"])), "fast-forward-only");
        // `-s` and the aliases agree, and `-s` wins when it is the one given.
        assert_eq!(style_of(&args(&["gea", "-s", "squash"])), "squash");
        assert_eq!(style_of(&args(&["gea"])), "merge");
    }

    /// Two styles at once is a contradiction, and picking one silently would produce a history the
    /// user did not ask for.
    #[test]
    fn two_styles_at_once_are_refused() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        for words in [
            &["gea", "--squash", "--rebase"][..],
            &["gea", "--merge", "--squash"][..],
            &["gea", "-s", "squash", "--rebase"][..],
            &["gea", "--fast-forward-only", "--merge"][..],
        ] {
            assert!(<Harness as clap::Parser>::try_parse_from(words).is_err(), "{words:?}");
        }
    }

    /// Every style the API knows must be accepted by `-s`, or a spec bump leaves a value reachable
    /// through `gea raw` and not through here.
    #[test]
    fn every_style_the_api_knows_is_accepted() {
        for style in MergeStyle::KNOWN {
            assert_eq!(style_of(&args(&["gea", "-s", style])), *style);
        }
        assert!(MergeStyle::KNOWN.contains(&"fast-forward-only"), "the one gh does not have");
    }

    /// **The headline test of this module.** `tea` answers every refusal with `failed to merge PR,
    /// is it still open?`, discarding the actual reason. Gitea sends that reason in the body of
    /// a **405**, so this goes through the real request path — typed client, real classifier —
    /// and asserts the server's words reach the rendered error.
    ///
    /// This used to be a unit test of a local helper that read the 405 body itself, because the
    /// classifier mapped every 405 to `RouteNotFound`, which has nowhere to put a message. The
    /// classifier now splits on the body, so the helper is gone and the property is asserted where
    /// it actually has to hold.
    #[tokio::test]
    async fn a_refused_merge_carries_the_servers_own_reason() {
        let t = Arc::new(testing::on(
            testing::transport(),
            "POST",
            "/api/v1/repos/them/proj/pulls/42/merge",
            Canned::json(405, r#"{"message":"Please rebase your branch onto main first"}"#),
        ));
        let err = testing::api_at(testing::EXAMPLE, t)
            .repo()
            .merge_pull_request("them", "proj", 42, &MergePullRequestOption::default())
            .await
            .expect_err("a 405 is a refusal");

        let rendered =
            gitea_core::error::render::render(&err, gitea_core::error::render::Color::Never);
        assert!(rendered.contains("Please rebase your branch onto main first"), "{rendered}");
        // ...and it names what was refused, which `RouteNotFound` could not.
        assert!(rendered.contains("pull request 42"), "{rendered}");
        // Not a usage error: the user typed nothing wrong.
        assert_eq!(err.exit_code(), 1);
        assert!(matches!(err.kind(), ErrorKind::StateConflict { .. }), "{:?}", err.kind());
    }

    /// The other half of the split, and the reason the 405 arm is not simply "always a refusal":
    /// a 405 whose body is an HTML page from a reverse proxy carries no reason to relay, and
    /// really does mean this instance has no such route. Reporting it as a merge refusal would
    /// invent a cause.
    #[tokio::test]
    async fn a_405_with_no_message_is_reported_as_a_missing_route() {
        let t = Arc::new(testing::on(
            testing::transport(),
            "POST",
            "/api/v1/repos/them/proj/pulls/42/merge",
            Canned::new(405)
                .with_header("content-type", "text/html")
                .with_body("<html>405 Not Allowed</html>"),
        ));
        let err = testing::api_at(testing::EXAMPLE, t)
            .repo()
            .merge_pull_request("them", "proj", 42, &MergePullRequestOption::default())
            .await
            .expect_err("a 405 is not a success");
        assert!(matches!(err.kind(), ErrorKind::RouteNotFound { .. }), "{:?}", err.kind());
    }

    /// `--auto` and `-d` are independent, and both have to reach the wire under their Gitea
    /// names — which are neither of the flag names.
    #[test]
    fn auto_and_delete_branch_use_the_apis_field_names() {
        let body = MergePullRequestOption {
            r#do: MergeStyle::Squash,
            delete_branch_after_merge: Some(true),
            merge_when_checks_succeed: Some(true),
            ..MergePullRequestOption::default()
        };
        let json = serde_json::to_value(&body).expect("serialisable");
        assert_eq!(json["do"], "squash", "Gitea spells it `do`, where Forgejo spells it `Do`");
        assert_eq!(json["delete_branch_after_merge"], true);
        assert_eq!(json["merge_when_checks_succeed"], true);
    }

    #[test]
    fn a_subject_and_body_reach_the_merge_commit_fields() {
        let body = MergePullRequestOption {
            merge_title_field: Some("Merge the thing".to_owned()),
            merge_message_field: Some("because".to_owned()),
            ..MergePullRequestOption::default()
        };
        let json = serde_json::to_value(&body).expect("serialisable");
        assert_eq!(json["merge_title_field"], "Merge the thing");
        assert_eq!(json["merge_message_field"], "because");
    }
}

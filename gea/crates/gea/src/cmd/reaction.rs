//! `gea reaction` — reactions on issues, pull requests and comments.
//!
//! `gh` has no reaction command at all: its `reactionGroups` JSON field is read-only, so with
//! `gh` you can see that eight people reacted and you cannot join them. Gitea has a full
//! read/write collection, and this is it.
//!
//! # Two targets, two id kinds, two endpoints
//!
//! This is the whole reason the group needs care:
//!
//! ```text
//! …/issues/{index}/reactions            index — the #42 users see, per repository
//! …/issues/comments/{id}/reactions      id    — a global database row id, e.g. 918273
//! ```
//!
//! They are different endpoints, and the numbers are different *kinds* of number. `42` is a valid
//! comment id and a valid issue index at the same time, so a mix-up does not 404 — it reacts to a
//! real, unrelated object. [`IssueIndex`] and [`CommentId`] make that a compile error here, and
//! `--issue` / `--comment` make it explicit on the command line. There is deliberately no bare
//! positional target: a single number cannot say which of the two it is.
//!
//! A pull request is an issue as far as this collection is concerned, so `--issue 42` reaches a
//! pull request numbered 42 too.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{EditReactionOption, Reaction};
use gitea_core::error::Result;
use gitea_core::http::Paging;
use gitea_core::types::RepoSlug;
use gitea_core::types::ids::{CommentId, IssueIndex};

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

const OP_LIST: &str = "issueGetIssueReactions";
const OP_ONE: &str = "issuePostIssueReaction";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List who reacted, and with what
    List(Target),
    /// Add your reaction
    Add(Content),
    /// Take your reaction back
    Remove(Content),
}

/// Which object to act on. Exactly one of the two, always spelled out.
#[derive(Debug, Clone, ClapArgs)]
#[group(required = true, multiple = false)]
pub struct Target {
    /// The issue or pull request number, as in `#42`
    #[arg(long, value_name = "NUMBER")]
    pub issue: Option<IssueIndex>,

    /// A comment's database id, as shown by `gea issue view --comments`
    #[arg(long, value_name = "ID")]
    pub comment: Option<CommentId>,
}

#[derive(Debug, ClapArgs)]
pub struct Content {
    /// The reaction, as Gitea names it: +1, -1, laugh, confused, heart, hooray, rocket, eyes
    ///
    /// `allow_hyphen_values` because `-1` is one of the two most-used reactions and would
    /// otherwise be parsed as a short option, making it unreachable.
    #[arg(value_name = "REACTION", allow_hyphen_values = true)]
    pub content: String,

    #[command(flatten)]
    pub target: Target,
}

/// The resolved target, so no code below this point can reach for the wrong id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    Issue(IssueIndex),
    Comment(CommentId),
}

impl On {
    /// clap's argument group has already guaranteed exactly one, but the group is a *parser*
    /// constraint and this function is called from tests too, so the impossible case is named
    /// rather than unwrapped.
    fn resolve(target: &Target) -> Result<Self> {
        match (target.issue, target.comment) {
            (Some(index), None) => Ok(Self::Issue(index)),
            (None, Some(id)) => Ok(Self::Comment(id)),
            _ => Err(support::usage(
                "specify one target: --issue <NUMBER> for an issue or pull request, or --comment <ID> for a comment.",
            )),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Issue(i) => format!("issue #{i}"),
            Self::Comment(c) => format!("comment {c}"),
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let op = match &args.command {
        Cmd::List(_) => OP_LIST,
        Cmd::Add(_) => OP_ONE,
        // `remove` answers 200 with no body worth selecting.
        Cmd::Remove(_) => "",
    };
    let fields = if op.is_empty() {
        None
    } else {
        match Json::resolve(globals, op)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        }
    };

    let on = match &args.command {
        Cmd::List(t) => On::resolve(t)?,
        Cmd::Add(c) | Cmd::Remove(c) => On::resolve(&c.target)?,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List(_) => {
                let (items, total) = list(&api, &slug, on, globals).await?;
                emit.many(&items, total, "reactions", |table, items| {
                    table.headers(["REACTION", "USER", "WHEN"]);
                    for r in items {
                        table.row([
                            r.content.clone(),
                            r.user.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
                            r.created_at.as_ref().map(ToString::to_string).unwrap_or_default(),
                        ]);
                    }
                })
            }
            Cmd::Add(c) => {
                let body = EditReactionOption { content: Some(c.content.clone()) };
                let reaction = add(&api, &slug, on, &body).await?;
                emit.done(&format!("reacted {} to {}", reaction.content, on.label()));
                emit.one(&reaction, |t| {
                    t.row(["reaction".to_owned(), reaction.content.clone()]);
                    if let Some(u) = &reaction.user {
                        t.row(["user".to_owned(), u.login.clone()]);
                    }
                })
            }
            Cmd::Remove(c) => {
                let body = EditReactionOption { content: Some(c.content.clone()) };
                remove(&api, &slug, on, &body).await?;
                emit.done(&format!("removed {} from {}", c.content, on.label()));
                Ok(())
            }
        }
    })
}

/// `--paginate` only exists for the issue route: the comment route is not paginated in the API,
/// which is why there is no `get_comment_reactions_page` to call.
async fn list(
    api: &Api,
    slug: &RepoSlug,
    on: On,
    globals: &GlobalOpts,
) -> Result<(Vec<Reaction>, Option<u64>)> {
    match on {
        On::Issue(index) => {
            let cap = support::item_cap(globals);
            let q = gitea_client::query::IssueGetIssueReactionsQuery::default();
            if globals.paginate {
                let items = support::drain(
                    api.issue().get_issue_reactions(&slug.owner, &slug.name, index.get(), &q),
                    cap,
                )
                .await?;
                let n = items.len() as u64;
                Ok((items, Some(n)))
            } else {
                let (items, info) = api
                    .issue()
                    .get_issue_reactions_page(
                        &slug.owner,
                        &slug.name,
                        index.get(),
                        &q,
                        Paging { limit: cap, per_page: None },
                    )
                    .await?;
                Ok((items, info.total_count))
            }
        }
        On::Comment(id) => {
            let items =
                api.issue().get_comment_reactions(&slug.owner, &slug.name, id.get()).await?;
            let n = items.len() as u64;
            Ok((items, Some(n)))
        }
    }
}

async fn add(api: &Api, slug: &RepoSlug, on: On, body: &EditReactionOption) -> Result<Reaction> {
    match on {
        On::Issue(index) => {
            api.issue().post_issue_reaction(&slug.owner, &slug.name, index.get(), body).await
        }
        On::Comment(id) => {
            api.issue().post_comment_reaction(&slug.owner, &slug.name, id.get(), body).await
        }
    }
}

async fn remove(api: &Api, slug: &RepoSlug, on: On, body: &EditReactionOption) -> Result<()> {
    match on {
        On::Issue(index) => {
            api.issue().delete_issue_reaction(&slug.owner, &slug.name, index.get(), body).await
        }
        On::Comment(id) => {
            api.issue().delete_comment_reaction(&slug.owner, &slug.name, id.get(), body).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    fn slug() -> RepoSlug {
        "acme/widget".parse().unwrap()
    }

    /// The bug this exists to prevent, and the reason the two ids are separate newtypes: `42` is
    /// simultaneously a plausible issue index and a plausible comment id, so sending one to the
    /// other's endpoint does not fail — it reacts to a real, unrelated object. Both paths are
    /// asserted on the wire, in one test, so they cannot drift apart.
    #[tokio::test]
    async fn an_issue_and_a_comment_reach_different_endpoints() {
        let reply = Canned::json(201, r#"{"content":"+1","user":{"login":"ada"}}"#);
        let fake = testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/acme/widget/issues/42/reactions",
            reply.clone(),
        );
        let fake = Arc::new(testing::on(
            fake,
            "POST",
            "/api/v1/repos/acme/widget/issues/comments/42/reactions",
            reply,
        ));
        let api = testing::api(fake.clone());
        let body = EditReactionOption { content: Some("+1".to_owned()) };

        add(&api, &slug(), On::Issue(IssueIndex::new(42)), &body).await.unwrap();
        add(&api, &slug(), On::Comment(CommentId::new(42)), &body).await.unwrap();

        let paths: Vec<String> = fake.calls().into_iter().map(|c| c.path).collect();
        assert_eq!(
            paths,
            vec![
                "/api/v1/repos/acme/widget/issues/42/reactions".to_owned(),
                "/api/v1/repos/acme/widget/issues/comments/42/reactions".to_owned(),
            ]
        );
    }

    /// Same property for the read and the delete halves: a `remove` aimed at the wrong endpoint
    /// would remove a reaction from somebody else's comment.
    #[tokio::test]
    async fn list_and_remove_also_split_by_target() {
        let fake = testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/acme/widget/issues/7/reactions",
            Canned::json(200, "[]"),
        );
        let fake = testing::on(
            fake,
            "GET",
            "/api/v1/repos/acme/widget/issues/comments/7/reactions",
            Canned::json(200, "[]"),
        );
        let fake = testing::on(
            fake,
            "DELETE",
            "/api/v1/repos/acme/widget/issues/7/reactions",
            testing::empty(),
        );
        let fake = Arc::new(testing::on(
            fake,
            "DELETE",
            "/api/v1/repos/acme/widget/issues/comments/7/reactions",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let body = EditReactionOption { content: Some("rocket".to_owned()) };

        list(&api, &slug(), On::Issue(IssueIndex::new(7)), &globals).await.unwrap();
        list(&api, &slug(), On::Comment(CommentId::new(7)), &globals).await.unwrap();
        remove(&api, &slug(), On::Issue(IssueIndex::new(7)), &body).await.unwrap();
        remove(&api, &slug(), On::Comment(CommentId::new(7)), &body).await.unwrap();

        let seen: Vec<String> =
            fake.calls().into_iter().map(|c| format!("{} {}", c.method.as_str(), c.path)).collect();
        insta::assert_snapshot!(seen.join("\n"));
    }

    /// Bug this prevents: a bare positional target, or defaulting to one of the two, which would
    /// make an ambiguous command line silently pick an endpoint.
    #[test]
    fn a_target_must_be_named_exactly_once() {
        let both = Target { issue: Some(IssueIndex::new(1)), comment: Some(CommentId::new(2)) };
        assert_eq!(On::resolve(&both).unwrap_err().exit_code(), 2);
        let neither = Target { issue: None, comment: None };
        let e = On::resolve(&neither).unwrap_err();
        assert!(e.to_string().contains("--issue"), "{e}");
        assert!(e.to_string().contains("--comment"), "{e}");
    }

    /// The reaction body is a single `content` string, and the *delete* carries one too — a
    /// `DELETE` with a body is unusual enough that dropping it is an easy mistake, and it would
    /// silently remove nothing.
    #[tokio::test]
    async fn remove_sends_the_content_in_the_delete_body() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "DELETE",
            "/api/v1/repos/acme/widget/issues/3/reactions",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        let body = EditReactionOption { content: Some("heart".to_owned()) };
        remove(&api, &slug(), On::Issue(IssueIndex::new(3)), &body).await.unwrap();
        assert_eq!(fake.calls()[0].body_str(), r#"{"content":"heart"}"#);
    }

    #[tokio::test]
    async fn the_list_view_shows_who_reacted() {
        let items: Vec<Reaction> = serde_json::from_str(
            r#"[{"content":"+1","user":{"login":"ada"},"created_at":"2024-05-01T10:00:00Z"},
                {"content":"rocket","user":{"login":"grace"},"created_at":"2024-05-02T11:30:00Z"}]"#,
        )
        .unwrap();
        let out =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::tty(60), |e| {
                e.many(&items, Some(2), "reactions", |table, items| {
                    table.headers(["REACTION", "USER", "WHEN"]);
                    for r in items {
                        table.row([
                            r.content.clone(),
                            r.user.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
                            r.created_at.as_ref().map(ToString::to_string).unwrap_or_default(),
                        ]);
                    }
                })
            });
        insta::assert_snapshot!(out);
    }
}

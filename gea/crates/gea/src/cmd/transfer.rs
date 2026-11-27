//! `gea transfer` — moving a repository to a new owner, as an **offer**.
//!
//! `gh` has no `repo transfer` at all, and the reason it would not port cleanly is that Gitea's
//! model is genuinely a different shape from GitHub's. On Gitea a transfer is a *pending offer*:
//!
//! ```text
//!  the current owner            the new owner
//!  ─────────────────            ─────────────
//!  gea transfer start them  ─▶  (repository still lives at you/repo,
//!                                 marked as pending transfer to `them`)
//!                                gea transfer accept  -R you/repo
//!                                gea transfer reject  -R you/repo
//! ```
//!
//! **The two commands are run by two different people, with two different tokens.** That is the
//! single most important fact about this group, so it is in the `--help` of every subcommand and
//! not only here.
//!
//! Two consequences that are easy to get wrong:
//!
//! * `accept` and `reject` address the repository by its **current** path — the *old* owner's —
//!   because the repository has not moved yet. `-R them/repo` 404s; `-R you/repo` is right.
//! * A transfer into an organization can be scoped to teams with `--team`, and only an
//!   organization-owned repository can have teams, which is why `--team` is not the default.
//!
//! `status` has no endpoint of its own: the pending offer arrives as `repo_transfer` on the
//! repository itself, which is why `status` is a `GET /repos/{owner}/{repo}` and reads the
//! `doer`/`recipient` pair out of it. Reporting "no transfer pending" from a successful read is
//! deliberate — a missing `repo_transfer` is an answer, not a 404.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{Repository, TransferRepoOption};
use gitea_core::error::Result;
use gitea_core::types::RepoSlug;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

const OP_REPO: &str = "repoGet";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Offer the repository to a new owner (run by the current owner)
    ///
    /// This does not move the repository. It records an offer that the new owner then has to
    /// accept with `gea transfer accept`, or refuse with `gea transfer reject` — usually from a
    /// different account and a different token.
    Start(Start),

    /// Accept a repository offered to you (run by the new owner)
    ///
    /// Address the repository by its *current* path, under the owner who offered it: the
    /// repository has not moved yet, so `-R old-owner/name` is what resolves.
    Accept(Decide),

    /// Refuse a repository offered to you (run by the new owner)
    ///
    /// As with `accept`, the repository is still at its old path until the offer is settled.
    Reject(Decide),

    /// Show whether a transfer is pending, who offered it, and to whom
    Status,
}

#[derive(Debug, ClapArgs)]
pub struct Start {
    /// The user or organization that should own the repository
    #[arg(value_name = "NEW-OWNER")]
    pub new_owner: String,

    /// Team id to grant access to; repeatable. Only organization-owned repositories have teams.
    #[arg(long = "team", value_name = "TEAM-ID")]
    pub team: Vec<i64>,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct Decide {
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let fields = match Json::resolve(globals, OP_REPO)? {
        Json::Listed => return Ok(()),
        Json::Fields(f) => f,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::Start(a) => {
                support::confirm_term(
                    emit.term(),
                    a.yes,
                    &format!("offer {slug} to {} — they must accept it", a.new_owner),
                )?;
                let body = TransferRepoOption {
                    new_owner: a.new_owner.clone(),
                    team_ids: Some(a.team.clone()),
                };
                let repo = api.repo().transfer(&slug.owner, &slug.name, &body).await?;
                // A transfer to a user the doer administers (or one's own account) is applied
                // immediately and comes back with no pending offer; anything else is now waiting.
                // Saying which happened is the difference between "done" and "now go and tell
                // them".
                match &repo.repo_transfer {
                    Some(_) => emit.done(&format!(
                        "offered {slug} to {}. It stays at {slug} until they run \
                         `gea transfer accept -R {slug}`.",
                        a.new_owner
                    )),
                    None => emit.done(&format!(
                        "transferred {slug} to {} — no acceptance was needed",
                        a.new_owner
                    )),
                }
                emit.one(&repo, |t| detail(t, &repo, &slug))
            }

            Cmd::Accept(a) => {
                support::confirm_term(emit.term(), a.yes, &format!("accept ownership of {slug}"))?;
                let repo = api.repo().accept_repo_transfer(&slug.owner, &slug.name).await?;
                emit.done(&format!("accepted transfer of {slug} to {}", repo.full_name));
                emit.one(&repo, |t| detail(t, &repo, &slug))
            }

            Cmd::Reject(a) => {
                support::confirm_term(emit.term(), a.yes, &format!("reject ownership of {slug}"))?;
                let repo = api.repo().reject_repo_transfer(&slug.owner, &slug.name).await?;
                emit.done(&format!("rejected transfer of {slug}; ownership unchanged"));
                emit.one(&repo, |t| detail(t, &repo, &slug))
            }

            Cmd::Status => {
                let repo = api.repo().get(&slug.owner, &slug.name).await?;
                if repo.repo_transfer.is_none() {
                    support::note(emit.term(), &format!("no transfer pending for {slug}"));
                }
                emit.one(&repo, |t| detail(t, &repo, &slug))
            }
        }
    })
}

/// The two-sided view: who is offering, who is being offered to, and what is still needed.
fn detail(table: &mut Table, repo: &Repository, slug: &RepoSlug) {
    table.row(["repository".to_owned(), repo.full_name.clone()]);
    match &repo.repo_transfer {
        None => {
            table.row(["transfer".to_owned(), "none pending".to_owned()]);
        }
        Some(t) => {
            table.row(["transfer".to_owned(), "pending".to_owned()]);
            table.row([
                "offered by".to_owned(),
                t.doer.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
            ]);
            table.row([
                "offered to".to_owned(),
                t.recipient.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
            ]);
            if !t.teams.is_empty() {
                table.row([
                    "teams".to_owned(),
                    t.teams.iter().map(|team| team.name.clone()).collect::<Vec<_>>().join(", "),
                ]);
            }
            table.row([
                "next step".to_owned(),
                format!(
                    "the recipient runs `gea transfer accept -R {slug}` (or `reject`) with \
                     their own token"
                ),
            ]);
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
        "ada/widget".parse().unwrap()
    }

    const PENDING: &str = r#"{"full_name":"ada/widget",
        "repo_transfer":{"doer":{"login":"ada"},"recipient":{"login":"grace"},"teams":[]}}"#;

    /// Bug this prevents: `start` posting to the *new* owner's path. The repository has not moved,
    /// so the offer is recorded against the current owner's path and nothing else resolves.
    #[tokio::test]
    async fn start_posts_to_the_current_owners_path() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/ada/widget/transfer",
            Canned::json(202, PENDING),
        ));
        let api = testing::api(fake.clone());
        let body = TransferRepoOption { new_owner: "grace".to_owned(), team_ids: Some(vec![3]) };
        let repo = api.repo().transfer("ada", "widget", &body).await.unwrap();
        assert!(repo.repo_transfer.is_some());
        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent["new_owner"], serde_json::json!("grace"));
        assert_eq!(sent["team_ids"], serde_json::json!([3]));
    }

    /// The bug this exists to prevent, and the single most likely mistake in this group: the
    /// recipient addressing the repository by their *own* name. `accept` and `reject` both go to
    /// the old owner's path.
    #[tokio::test]
    async fn accept_and_reject_address_the_old_owners_path() {
        let fake = testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/ada/widget/transfer/accept",
            Canned::json(202, r#"{"full_name":"grace/widget"}"#),
        );
        let fake = Arc::new(testing::on(
            fake,
            "POST",
            "/api/v1/repos/ada/widget/transfer/reject",
            Canned::json(202, r#"{"full_name":"ada/widget"}"#),
        ));
        let api = testing::api(fake.clone());

        let accepted = api.repo().accept_repo_transfer("ada", "widget").await.unwrap();
        assert_eq!(accepted.full_name, "grace/widget", "acceptance is what moves it");
        let rejected = api.repo().reject_repo_transfer("ada", "widget").await.unwrap();
        assert_eq!(rejected.full_name, "ada/widget", "rejection leaves it where it was");

        let seen: Vec<String> = fake.calls().into_iter().map(|c| c.path).collect();
        assert_eq!(
            seen,
            vec![
                "/api/v1/repos/ada/widget/transfer/accept".to_owned(),
                "/api/v1/repos/ada/widget/transfer/reject".to_owned(),
            ]
        );
    }

    /// `status` has no endpoint of its own; it reads `repo_transfer` off the repository. The human
    /// view has to name *both* sides and the next step, because the person reading it is usually
    /// not the person who has to act.
    #[tokio::test]
    async fn status_names_both_sides_and_the_next_step() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/ada/widget",
            Canned::json(200, PENDING),
        ));
        let api = testing::api(fake);
        let repo = api.repo().get("ada", "widget").await.unwrap();
        let out =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::tty(100), |e| {
                e.one(&repo, |t| detail(t, &repo, &slug()))
            });
        insta::assert_snapshot!(out);
    }

    /// Bug this prevents: treating "no transfer pending" as an error. It is a successful read with
    /// a `None`, and a script asking "is one pending?" must be able to tell the two apart by
    /// content rather than by exit code.
    #[tokio::test]
    async fn no_pending_transfer_is_a_successful_answer() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/ada/widget",
            Canned::json(200, r#"{"full_name":"ada/widget"}"#),
        ));
        let api = testing::api(fake);
        let repo = api.repo().get("ada", "widget").await.unwrap();
        assert!(repo.repo_transfer.is_none());
        let out =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::piped(), |e| {
                e.one(&repo, |t| detail(t, &repo, &slug()))
            });
        assert_eq!(out, "repository\tada/widget\ntransfer\tnone pending\n");
    }
}

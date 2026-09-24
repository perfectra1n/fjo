//! `gea block` — the users you (or an organization you run) have blocked.
//!
//! `gh` has nothing here at all. Gitea has two parallel collections, and `--org` is what picks
//! between them:
//!
//! ```text
//! GET /user/blocks  ·  PUT /user/blocks/{username}  ·  DELETE /user/blocks/{username}
//! GET /orgs/{org}/blocks  ·  PUT /orgs/{org}/blocks/{u}  ·  DELETE /orgs/{org}/blocks/{u}
//! ```
//!
//! The listing answers with full `User` objects, so `list` needs no follow-up lookups and its
//! `--json` fields are `User`'s own. Blocking takes an optional `--note`, which Gitea stores
//! alongside the block and shows in the web UI's blocklist.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::User;
use gitea_core::error::Result;
use gitea_core::http::Paging;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

const OP_LIST: &str = "userListBlocks";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List blocked accounts
    List(Scope),
    /// Block an account
    Add(Block),
    /// Unblock an account
    Remove(Who),
}

/// Whose blocklist: yours, or an organization's.
#[derive(Debug, Clone, ClapArgs)]
pub struct Scope {
    /// Act on this organization's blocklist instead of your own account's
    #[arg(long, value_name = "NAME")]
    pub org: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Who {
    /// The account to block or unblock
    #[arg(value_name = "USER")]
    pub user: String,

    #[command(flatten)]
    pub scope: Scope,
}

#[derive(Debug, ClapArgs)]
pub struct Block {
    #[command(flatten)]
    pub who: Who,

    /// A private note to keep with the block, shown in the web UI's blocklist
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let fields = match &args.command {
        Cmd::List(_) => match Json::resolve(globals, OP_LIST)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        },
        // Blocking and unblocking answer 204.
        _ => None,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List(scope) => list(&api, scope.org.as_deref(), globals, &mut emit).await,
            Cmd::Add(b) => {
                block(&api, &b.who, b.note.as_deref()).await?;
                let w = &b.who;
                emit.done(&format!("blocked {} {}", w.user, whose(w.scope.org.as_deref())));
                Ok(())
            }
            Cmd::Remove(w) => {
                match &w.scope.org {
                    Some(org) => api.org().organization_unblock_user(org, &w.user).await?,
                    None => api.user().unblock_user(&w.user).await?,
                }
                emit.done(&format!("unblocked {} {}", w.user, whose(w.scope.org.as_deref())));
                Ok(())
            }
        }
    })
}

async fn block(api: &Api, w: &Who, note: Option<&str>) -> Result<()> {
    match &w.scope.org {
        Some(org) => {
            let mut q = gitea_client::query::OrganizationBlockUserQuery::default();
            if let Some(n) = note {
                q = q.with_note(n);
            }
            api.org().organization_block_user(org, &w.user, &q).await
        }
        None => {
            let mut q = gitea_client::query::UserBlockUserQuery::default();
            if let Some(n) = note {
                q = q.with_note(n);
            }
            api.user().block_user(&w.user, &q).await
        }
    }
}

async fn list(
    api: &Api,
    org: Option<&str>,
    globals: &GlobalOpts,
    emit: &mut Emit<'_>,
) -> Result<()> {
    let cap = support::item_cap(globals);
    let paging = Paging { limit: cap, per_page: None };
    let (blocked, total): (Vec<User>, Option<u64>) = match (org, globals.paginate) {
        (Some(org), true) => {
            let q = gitea_client::query::OrganizationListBlocksQuery::default();
            let v = support::drain(api.org().organization_list_blocks(org, &q), cap).await?;
            let n = v.len() as u64;
            (v, Some(n))
        }
        (Some(org), false) => {
            let q = gitea_client::query::OrganizationListBlocksQuery::default();
            let (v, info) = api.org().organization_list_blocks_page(org, &q, paging).await?;
            (v, info.total_count)
        }
        (None, true) => {
            let q = gitea_client::query::UserListBlocksQuery::default();
            let v = support::drain(api.user().list_blocks(&q), cap).await?;
            let n = v.len() as u64;
            (v, Some(n))
        }
        (None, false) => {
            let q = gitea_client::query::UserListBlocksQuery::default();
            let (v, info) = api.user().list_blocks_page(&q, paging).await?;
            (v, info.total_count)
        }
    };

    emit.many(&blocked, total, "blocked accounts", |table, users| {
        table.headers(["USER", "NAME"]);
        for u in users {
            table.row([u.login.clone(), u.full_name.clone()]);
        }
    })
}

fn whose(org: Option<&str>) -> String {
    match org {
        Some(org) => format!("for {org}"),
        None => "for your account".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    /// Bug this prevents: `--org` and the bare form sharing a path, so blocking somebody for an
    /// organization blocks them for you instead. Also pins that unblocking is a `DELETE` on the
    /// same path blocking `PUT`s to, and that `--note` travels as a query parameter.
    #[tokio::test]
    async fn the_org_and_account_scopes_use_different_paths() {
        let fake = testing::on(
            FakeTransport::new(),
            "PUT",
            "/api/v1/user/blocks/mallory",
            testing::empty(),
        );
        let fake = testing::on(fake, "DELETE", "/api/v1/user/blocks/mallory", testing::empty());
        let fake = testing::on(fake, "PUT", "/api/v1/orgs/acme/blocks/mallory", testing::empty());
        let fake = Arc::new(testing::on(
            fake,
            "DELETE",
            "/api/v1/orgs/acme/blocks/mallory",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());

        let me = Who { user: "mallory".into(), scope: Scope { org: None } };
        let acme = Who { user: "mallory".into(), scope: Scope { org: Some("acme".into()) } };
        block(&api, &me, Some("spam")).await.unwrap();
        api.user().unblock_user("mallory").await.unwrap();
        block(&api, &acme, None).await.unwrap();
        api.org().organization_unblock_user("acme", "mallory").await.unwrap();

        let seen: Vec<String> = fake
            .calls()
            .into_iter()
            .map(|c| {
                let note = c.query_param("note").map(|n| format!(" note={n}")).unwrap_or_default();
                format!("{} {}{note}", c.method.as_str(), c.path)
            })
            .collect();
        insta::assert_snapshot!(seen.join("\n"));
    }

    /// The listing is already `User`s, so the human table reads logins straight off it with no
    /// per-row lookups.
    #[tokio::test]
    async fn list_prints_logins_without_extra_requests() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/user/blocks",
            Canned::json(
                200,
                r#"[{"id":4,"login":"spammer","full_name":"Spam Bot"},
                    {"id":9,"login":"troll","full_name":""}]"#,
            ),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            list(&api, None, &globals, &mut emit).await.unwrap();
        }
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
        assert_eq!(fake.call_count(), 1);
    }
}

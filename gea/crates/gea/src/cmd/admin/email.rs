//! `gea admin email` — find an account by its email address.
//!
//! One verb, because there is one useful question: somebody wrote in from
//! `alice@example.invalid` and you need to know which account that is. `GET /admin/emails/search`
//! answers it, and it is the only route on the instance that maps an address back to a login —
//! `/admin/users` cannot be searched by email at all.
//!
//! With no query it falls back to `GET /admin/emails`, which lists every address on the instance.
//! That is the same command with the argument left off rather than a second verb, because "list
//! them all" and "find one" differ only in whether you know what you are looking for.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_core::error::Result;
use gitea_core::http::Paging;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;

pub const OP_EMAIL: &str = "adminSearchEmails";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Find which account owns an email address
    ///
    /// Without QUERY, lists all email addresses on the server.
    Search(Search),
}

#[derive(Debug, ClapArgs)]
pub struct Search {
    /// Part of an email address, or a whole one
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Search(_) => OP_EMAIL,
    }
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    let Cmd::Search(args) = cmd;
    let cap = support::item_cap(globals);

    // Two endpoints rather than one with an empty `q`: `/admin/emails/search` with no keyword
    // returns nothing on some releases, which would look like "no such address" for a command that
    // was asked to list everything.
    let (emails, total) = match &args.query {
        Some(q) => {
            let query = gitea_client::query::AdminSearchEmailsQuery::default().with_q(q);
            if globals.paginate {
                let v = support::drain(api.admin().search_emails(&query), cap).await?;
                let n = v.len() as u64;
                (v, Some(n))
            } else {
                let (v, info) = api
                    .admin()
                    .search_emails_page(&query, Paging { limit: cap, per_page: None })
                    .await?;
                (v, info.total_count)
            }
        }
        None => {
            let query = gitea_client::query::AdminGetAllEmailsQuery::default();
            if globals.paginate {
                let v = support::drain(api.admin().get_all_emails(&query), cap).await?;
                let n = v.len() as u64;
                (v, Some(n))
            } else {
                let (v, info) = api
                    .admin()
                    .get_all_emails_page(&query, Paging { limit: cap, per_page: None })
                    .await?;
                (v, info.total_count)
            }
        }
    };

    emit.many(&emails, total, "email addresses", |table, emails| {
        table.headers(["EMAIL", "ACCOUNT", "PRIMARY", "VERIFIED"]);
        for e in emails {
            table.row([
                e.email.clone(),
                e.username.clone(),
                e.primary.to_string(),
                e.verified.to_string(),
            ]);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    const FOUND: &str = r#"[{"email":"ada@example.invalid","username":"ada","primary":true,
                             "verified":true}]"#;

    fn instance() -> Arc<FakeTransport> {
        let fake = testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/emails/search",
            Canned::json(200, FOUND),
        );
        Arc::new(testing::on(fake, "GET", "/api/v1/admin/emails", Canned::json(200, FOUND)))
    }

    /// Bug this prevents: sending an empty `q` to the search route when no query was given. Some
    /// Gitea releases answer that with nothing at all, which reads as "no such account" for a
    /// command that was asked to list every address.
    #[tokio::test]
    async fn a_query_searches_and_no_query_lists_everything() {
        let fake = instance();
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::Search(Search { query: Some("ada".into()) }))
                .await
                .unwrap();
            run(&api, &globals, &mut emit, &Cmd::Search(Search { query: None })).await.unwrap();
        }
        let paths: Vec<String> = fake.calls().into_iter().map(|c| c.path).collect();
        assert_eq!(
            paths,
            vec!["/api/v1/admin/emails/search".to_owned(), "/api/v1/admin/emails".to_owned(),]
        );
        assert_eq!(fake.calls()[0].query_param("q"), Some("ada"));
        assert_eq!(fake.calls()[1].query_param("q"), None);
    }

    /// The whole reason this command exists is the `ACCOUNT` column: an address on its own is not
    /// an answer to "who is this?".
    #[tokio::test]
    async fn the_view_names_the_account_that_owns_the_address() {
        let api = testing::api(instance());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::tty(80), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::Search(Search { query: Some("ada".into()) }))
                .await
                .unwrap();
        }
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }
}

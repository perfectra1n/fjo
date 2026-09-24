//! `gea admin adopt` — git directories on disk that the instance has no record of.
//!
//! **There is no equivalent to this anywhere else** — not in `gh`, not in `tea`. It exists because
//! Gitea's repositories are ordinary bare git directories under its data root, so they can get
//! out of step with the database: a restore that copied files but not rows, a `git clone --bare`
//! somebody dropped in by hand, a repository deleted from the database while its files stayed.
//! Gitea calls those *unadopted*, and gives an admin two ways out:
//!
//! * **adopt** — register the directory as a real repository owned by an existing account.
//! * **delete** — erase the directory.
//!
//! `delete` removes files from the server's disk and nothing in Gitea will undo it, so it
//! confirms on a terminal and demands `--yes` off one. It is the most destructive command in this
//! whole wave, and it is the reason the confirmation names the path rather than asking "are you
//! sure?".
//!
//! # Why the listing has no `--json <fields>`
//!
//! `GET /admin/unadopted` answers with a bare JSON array of `owner/name` strings — there is no
//! object and therefore nothing to select. `--json` says so and points at `--jq`, which works;
//! inventing an `{"owner": …, "name": …}` shape would break the promise that `--json` field names
//! are the API's own (`docs/output.md`).

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_core::error::Result;
use gitea_core::http::Paging;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Show git directories on the server's disk that Gitea has no record of
    List(List),

    /// Register an unadopted directory as a repository owned by an existing account
    ///
    /// The owner in OWNER/NAME must already exist as an account or organization; adoption does
    /// not create one.
    Adopt(One),

    /// Delete an unadopted directory from the server's disk
    ///
    /// This erases files. Nothing in Gitea undoes it.
    Delete(One),
}

#[derive(Debug, ClapArgs)]
pub struct List {
    /// Only paths matching this substring
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct One {
    /// The directory, as `gea admin adopt list` prints it: owner/name
    #[arg(value_name = "OWNER/NAME")]
    pub path: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

/// Empty for every subcommand: the listing is untyped and the two mutations answer 204.
pub fn op(_cmd: &Cmd) -> &'static str {
    ""
}

pub fn writes(cmd: &Cmd) -> bool {
    !matches!(cmd, Cmd::List(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List(a) => {
            let mut q = gitea_client::query::AdminUnadoptedListQuery::default();
            if let Some(p) = &a.pattern {
                q = q.with_pattern(p);
            }
            let cap = support::item_cap(globals);
            let (paths, total) = if globals.paginate {
                let paths = support::drain(api.admin().unadopted_list(&q), cap).await?;
                let n = paths.len() as u64;
                (paths, Some(n))
            } else {
                let (paths, info) = api
                    .admin()
                    .unadopted_list_page(&q, Paging { limit: cap, per_page: None })
                    .await?;
                (paths, info.total_count)
            };
            emit.many(&paths, total, "unadopted directories", |table, paths| {
                for p in paths {
                    table.row([p.clone()]);
                }
            })
        }

        Cmd::Adopt(a) => {
            let (owner, name) = split(&a.path)?;
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!(
                    "adopt the directory {}/{} as a repository owned by {}",
                    owner, name, owner
                ),
            )?;
            api.admin().adopt_repository(owner, name).await?;
            emit.done(&format!("adopted repository {owner}/{name}"));
            Ok(())
        }

        Cmd::Delete(a) => {
            let (owner, name) = split(&a.path)?;
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!(
                    "permanently delete the git directory {owner}/{name} from this server's disk"
                ),
            )?;
            api.admin().delete_unadopted_repository(owner, name).await?;
            emit.done(&format!("deleted the unadopted directory {owner}/{name}"));
            Ok(())
        }
    }
}

/// `owner/name`, split once and validated.
///
/// The API takes the two halves as separate path segments, so a value with no `/` — or with two —
/// would be silently mis-routed. Refusing it by name is the difference between "that is not a
/// repository path" and a 404 nobody can explain.
fn split(path: &str) -> Result<(&str, &str)> {
    let trimmed = path.trim().trim_matches('/');
    match trimmed.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok((owner, name))
        }
        _ => Err(support::usage(format!(
            "{path:?} is not an owner/name pair; `gea admin adopt list` prints them in exactly \
             the form this command wants"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    #[test]
    fn an_owner_name_pair_is_validated_before_anything_is_sent() {
        assert_eq!(split("ada/widget").unwrap(), ("ada", "widget"));
        assert_eq!(split(" /ada/widget/ ").unwrap(), ("ada", "widget"));
        for bad in ["widget", "ada/", "/widget", "a/b/c", ""] {
            let e = split(bad).unwrap_err();
            assert_eq!(e.exit_code(), 2, "{bad:?} should be a usage error");
        }
    }

    #[tokio::test]
    async fn list_prints_one_path_per_line() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/unadopted",
            Canned::json(200, r#"["ada/restored","acme/leftover"]"#),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(List { pattern: None })).await.unwrap();
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "ada/restored\nacme/leftover\n");
    }

    /// Bug this prevents: `adopt` and `delete` sharing a path or a method. They differ only by
    /// verb on the same URL, so a copy-paste slip turns "register this repository" into "erase it".
    #[tokio::test]
    async fn adopt_and_delete_differ_only_by_method_and_both_are_asserted() {
        let fake = testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/admin/unadopted/ada/restored",
            testing::empty(),
        );
        let fake = Arc::new(testing::on(
            fake,
            "DELETE",
            "/api/v1/admin/unadopted/ada/restored",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();

        let one = |yes| One { path: "ada/restored".to_owned(), yes };
        run(&api, &globals, &mut emit, &Cmd::Adopt(one(true))).await.unwrap();
        run(&api, &globals, &mut emit, &Cmd::Delete(one(true))).await.unwrap();

        let seen: Vec<String> =
            fake.calls().into_iter().map(|c| format!("{} {}", c.method.as_str(), c.path)).collect();
        assert_eq!(
            seen,
            vec![
                "POST /api/v1/admin/unadopted/ada/restored".to_owned(),
                "DELETE /api/v1/admin/unadopted/ada/restored".to_owned(),
            ]
        );
    }

    /// The most destructive command in this wave must not run unconfirmed off a terminal, and the
    /// refusal has to say what would have been erased.
    #[tokio::test]
    async fn delete_refuses_without_yes_when_there_is_no_terminal() {
        let api = testing::api(Arc::new(FakeTransport::new()));
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let cmd = Cmd::Delete(One { path: "ada/restored".into(), yes: false });
        let e = run(&api, &globals, &mut emit, &cmd).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("ada/restored"), "{e}");
        assert!(e.to_string().contains("--yes"), "{e}");
    }
}

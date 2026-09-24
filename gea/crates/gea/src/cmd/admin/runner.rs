//! `gea admin runner` — Actions runners registered anywhere on the instance.
//!
//! `GET /admin/actions/runners` is the only view that sees *all* of them at once: a runner can be
//! scoped to the instance, to an organization, to a user, or to a single repository, and the
//! per-scope endpoints each show only the runners registered directly against that scope. When a
//! job is stuck in `queued`, "which runners exist and are any of them online" is the first
//! question, and this is the command that answers it.
//!
//! Gitea does not say which scope a runner belongs to — its runner object carries no owner or
//! repository — so this view cannot either. `gea run runners --org`/`--user` and the per-repository
//! listing answer "whose is it" one scope at a time.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::ActionRunner;
use gitea_core::error::Result;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;

/// The item, not `getAdminRunners`'s `{total_count, runners}` envelope: the items are what print.
pub const OP_RUNNER: &str = "getAdminRunner";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List every runner on this instance, whatever it is scoped to
    List(List),

    /// Remove a runner's registration, so it can no longer pick up jobs
    ///
    /// This does not stop the runner process; it revokes its registration. A running job is not
    /// interrupted, but nothing new is handed to it.
    Delete(Delete),
}

#[derive(Debug, ClapArgs)]
pub struct List {
    /// Only runners an administrator has disabled
    #[arg(long)]
    pub disabled: bool,

    /// Only runners whose status matches: online or offline
    #[arg(long, value_name = "STATUS")]
    pub status: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Delete {
    /// The runner's numeric id, as shown by `gea admin runner list`
    #[arg(value_name = "RUNNER-ID")]
    pub id: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List(_) => OP_RUNNER,
        Cmd::Delete(_) => "",
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Delete(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List(a) => {
            let mut q = gitea_client::query::GetAdminRunnersQuery::default();
            if a.disabled {
                q = q.with_disabled(true);
            }
            let response = api.admin().get_admin_runners(&q).await?;
            let total = u64::try_from(response.total_count).ok();
            let mut runners = response.runners;
            if let Some(cap) = support::item_cap(globals) {
                runners.truncate(cap);
            }
            // Filtered here because the endpoint has no status parameter.
            let runners: Vec<ActionRunner> = match &a.status {
                Some(want) => {
                    runners.into_iter().filter(|r| r.status.eq_ignore_ascii_case(want)).collect()
                }
                None => runners,
            };
            let total = if a.status.is_some() { None } else { total };
            emit.many(&runners, total, "runners", |table, runners| {
                table.headers(["ID", "NAME", "STATUS", "BUSY", "DISABLED", "LABELS"]);
                for r in runners {
                    table.row([
                        r.id.to_string(),
                        r.name.clone(),
                        r.status.clone(),
                        yes(r.busy),
                        yes(r.disabled),
                        r.labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(","),
                    ]);
                }
            })
        }

        Cmd::Delete(a) => {
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!("unregister runner {} from this instance", a.id),
            )?;
            api.admin().delete_admin_runner(&a.id).await?;
            emit.done(&format!(
                "unregistered runner {}; the runner process is still running but will get no new \
                 jobs",
                a.id
            ));
            Ok(())
        }
    }
}

fn yes(b: bool) -> String {
    if b { "yes".to_owned() } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    const RUNNERS: &str = r#"{"total_count":3,"runners":[
        {"id":1,"name":"builder","status":"online","busy":true,
         "labels":[{"id":0,"name":"docker","type":"custom"},{"id":1,"name":"ubuntu-latest","type":"custom"}]},
        {"id":2,"name":"repo-local","status":"offline","labels":[{"name":"self-hosted"}]},
        {"id":3,"name":"org-wide","status":"online","disabled":true,"labels":[{"name":"arm64"}]}]}"#;

    #[tokio::test]
    async fn list_shows_every_scope_by_default() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, RUNNERS),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let args = List { disabled: false, status: None };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::tty(100), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        // No filter unless one was asked for.
        assert_eq!(fake.calls()[0].query_param("disabled"), None);
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }

    /// Bug this prevents: `--disabled` parsing and then never reaching the wire, so the operator
    /// reads every runner believing they are the disabled ones.
    #[tokio::test]
    async fn disabled_is_sent_as_the_servers_own_filter() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, r#"{"total_count":0,"runners":[]}"#),
        ));
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let args = List { disabled: true, status: None };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        assert_eq!(fake.calls()[0].query_param("disabled"), Some("true"));
    }

    #[tokio::test]
    async fn status_filtering_happens_here_because_the_endpoint_has_no_such_parameter() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/admin/actions/runners",
            Canned::json(200, RUNNERS),
        ));
        let api = testing::api(fake);
        let globals = GlobalOpts::default();
        let args = List { disabled: false, status: Some("OFFLINE".into()) };
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List(args)).await.unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("repo-local"), "{out}");
        assert!(!out.contains("builder"), "{out}");
    }
}

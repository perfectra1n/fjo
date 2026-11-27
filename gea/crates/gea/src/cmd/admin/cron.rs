//! `gea admin cron` — the instance's scheduled maintenance tasks.
//!
//! Gitea runs a set of internal cron jobs: garbage collection, mirror updates, expiring stale
//! sessions, checking repository health. `list` shows their schedules and when each last ran;
//! `run` triggers one **now**, which is the thing an operator wants when a mirror is stale or a
//! repository's statistics are wrong and they do not want to wait for the next tick.
//!
//! `run` answers `204` whether or not the task exists on the instance, so a typo in a task name
//! looks like success. `list`ing first and matching the name is therefore not a nicety — it is the
//! only way this command can tell the operator the truth, and one extra `GET` is a cheap price for
//! not lying about a maintenance job.

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::Result;
use gitea_core::http::Paging;

use crate::cmd::support::{self, Emit};
use crate::global::GlobalOpts;

pub const OP_CRON: &str = "adminCronList";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Show every scheduled task, its schedule, and when it last ran
    List,

    /// Run one scheduled task immediately
    ///
    /// The task runs in the background on the server; this command returns as soon as the
    /// instance has accepted it, not when it has finished.
    Run(Run),
}

#[derive(Debug, ClapArgs)]
pub struct Run {
    /// The task's name, exactly as `gea admin cron list` prints it
    #[arg(value_name = "TASK")]
    pub task: String,

    /// Do not confirm before running the task
    #[arg(long)]
    pub yes: bool,
}

pub fn op(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::List => OP_CRON,
        // Triggering a task answers 204.
        Cmd::Run(_) => "",
    }
}

pub fn writes(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Run(_))
}

pub async fn run(api: &Api, globals: &GlobalOpts, emit: &mut Emit<'_>, cmd: &Cmd) -> Result<()> {
    match cmd {
        Cmd::List => {
            let q = gitea_client::query::AdminCronListQuery::default();
            let cap = support::item_cap(globals);
            let (tasks, total) = if globals.paginate {
                let tasks = support::drain(api.admin().cron_list(&q), cap).await?;
                let n = tasks.len() as u64;
                (tasks, Some(n))
            } else {
                let (tasks, info) =
                    api.admin().cron_list_page(&q, Paging { limit: cap, per_page: None }).await?;
                (tasks, info.total_count)
            };
            emit.many(&tasks, total, "scheduled tasks", |table, tasks| {
                table.headers(["TASK", "SCHEDULE", "RUNS", "LAST", "NEXT"]);
                for c in tasks {
                    table.row([
                        c.name.clone(),
                        c.schedule.clone(),
                        c.exec_times.to_string(),
                        stamp(&c.prev),
                        stamp(&c.next),
                    ]);
                }
            })
        }

        Cmd::Run(a) => {
            let known = task_names(api).await?;
            if !known.iter().any(|n| n == &a.task) {
                return Err(support::usage(format!(
                    "this instance has no scheduled task called {:?}.\nit knows: {}",
                    a.task,
                    known.join(", ")
                )));
            }
            support::confirm_term(
                emit.term(),
                a.yes,
                &format!("run the scheduled task {} on this instance now", a.task),
            )?;
            api.admin().cron_run(&a.task).await?;
            emit.done(&format!("queued {}. Check the server log for the result.", a.task));
            Ok(())
        }
    }
}

/// Every task name the instance admits to, walked to the end.
///
/// Walked rather than one page: the check would otherwise reject a real task that happened to sit
/// on page two, which is a worse failure than not checking at all.
async fn task_names(api: &Api) -> Result<Vec<String>> {
    let q = gitea_client::query::AdminCronListQuery::default();
    let mut stream = std::pin::pin!(api.admin().cron_list(&q));
    let mut names = Vec::new();
    while let Some(task) = stream.next().await {
        names.push(task?.name);
    }
    Ok(names)
}

fn stamp(t: &Option<gitea_core::types::Timestamp>) -> String {
    t.as_ref().map(ToString::to_string).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    const TASKS: &str = r#"[
        {"name":"update_mirrors","schedule":"@every 10m","exec_times":12,
         "prev":"2024-05-01T10:00:00Z","next":"2024-05-01T10:10:00Z"},
        {"name":"git_gc_repos","schedule":"@every 72h","exec_times":1,
         "prev":"2024-04-30T02:00:00Z","next":"2024-05-03T02:00:00Z"}]"#;

    /// A fake that answers page 1 with the two tasks and every later page with `[]`.
    ///
    /// `on_fn`, not `on`: a canned reply ignores the query, so it re-serves a full page for
    /// ever. With no `Link` header, no `x-total-count`, no empty page and no short page, not one
    /// of the paginator's termination rules can fire, and `task_names` walks until `MAX_PAGES`
    /// and errors — which is exactly what a `?page`-ignoring instance would do to a user.
    ///
    /// Answering off the **query** rather than off a call counter is the point. `on_sequence`
    /// would hand out `[]` to the second request whatever it asked for, so a client that forgot
    /// to increment `page` — the bug that hangs against a real server — would still pass here.
    /// A real server decides from the request it was given, and so does this one.
    fn instance() -> Arc<FakeTransport> {
        let fake = testing::on_fn(FakeTransport::new(), "GET", "/api/v1/admin/cron", |call| {
            let page: usize = call.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
            Canned::json(200, if page == 1 { TASKS } else { "[]" })
        });
        let fake = testing::on(fake, "POST", "/api/v1/admin/cron/update_mirrors", testing::empty());
        Arc::new(fake)
    }

    #[tokio::test]
    async fn list_shows_the_schedule_and_the_last_run() {
        let api = testing::api(instance());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut emit =
                Emit::new(&globals, None, &crate::output::Term::tty(100), &mut buf).unwrap();
            run(&api, &globals, &mut emit, &Cmd::List).await.unwrap();
        }
        insta::assert_snapshot!(String::from_utf8(buf).unwrap());
    }

    /// The bug this exists to prevent: `POST /admin/cron/nosuchtask` answers `204`, so a mistyped
    /// task name reports success and nothing runs. An operator waiting for stale mirrors to
    /// refresh would have no way to know.
    #[tokio::test]
    async fn an_unknown_task_name_is_refused_and_the_real_ones_are_listed() {
        let fake = instance();
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let cmd = Cmd::Run(Run { task: "update_mirror".into(), yes: true });
        let e = run(&api, &globals, &mut emit, &cmd).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("update_mirrors"), "{e}");
        assert!(
            fake.calls().iter().all(|c| c.method == "GET"),
            "nothing may be triggered: {:?}",
            fake.calls()
        );
    }

    #[tokio::test]
    async fn a_known_task_is_triggered() {
        let fake = instance();
        let api = testing::api(fake.clone());
        let globals = GlobalOpts::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut emit = Emit::new(&globals, None, &crate::output::Term::piped(), &mut buf).unwrap();
        let cmd = Cmd::Run(Run { task: "update_mirrors".into(), yes: true });
        run(&api, &globals, &mut emit, &cmd).await.unwrap();
        assert!(
            fake.calls()
                .iter()
                .any(|c| c.method == "POST" && c.path == "/api/v1/admin/cron/update_mirrors"),
            "{:?}",
            fake.calls()
        );
    }
}

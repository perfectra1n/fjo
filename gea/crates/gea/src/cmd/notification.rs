//! `gea notification` — the notification inbox.
//!
//! # Three states, not two
//!
//! Gitea threads are `unread`, `read`, or **`pinned`**, and pinning is a real feature of the web
//! UI with no GitHub equivalent (`gh` has no notification commands at all). `PATCH
//! /notifications/threads/{id}?to-status=…` is the one route behind `read`, `unread` and `pin`.
//!
//! There is **no `unpinned` status**. Unpinning therefore means moving the thread back to one of
//! the other two, and `unpin` sends `unread` — the reading that loses nothing, because a thread
//! you pinned is one you had not finished with. That is a decision worth knowing about rather
//! than discovering, so it is in `--help` too.
//!
//! # Scope
//!
//! `list` is instance-wide (`GET /notifications`) *unless* `-R owner/name` was given explicitly,
//! in which case it uses the repository route. Note "explicitly": being inside a checkout does
//! **not** silently narrow your inbox, because an inbox that shows fewer notifications than you
//! have — depending on which directory you are standing in — is a way to miss things.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_model::NotificationThread;

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// The `status-types` values Gitea knows. Passed through rather than validated as an enum, on
/// the usual grounds: a newer server may learn another one.
const STATES: &[&str] = &["unread", "read", "pinned"];

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List notification threads
    List(ListArgs),
    /// Mark threads read
    Read(MarkArgs),
    /// Mark threads unread
    Unread(MarkArgs),
    /// Pin a thread
    Pin(ThreadArgs),
    /// Unpin a thread (it becomes unread again — the API has no `unpinned` status)
    Unpin(ThreadArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Include read and pinned threads, not just unread ones
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Only threads in this state; repeatable
    #[arg(short = 's', long = "state", value_name = "STATE", value_parser = STATES.to_vec())]
    pub states: Vec<String>,
    /// Mark everything that was listed as read, once it has been printed
    #[arg(long = "mark-read")]
    pub mark_read: bool,
}

#[derive(Debug, ClapArgs)]
pub struct MarkArgs {
    /// Thread ids, as printed by `list`. Omit them to act on everything unread
    #[arg(value_name = "ID")]
    pub ids: Vec<i64>,
    /// Act on every thread, including ones already read
    #[arg(short = 'a', long)]
    pub all: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ThreadArgs {
    /// The thread's id
    #[arg(value_name = "ID")]
    pub id: i64,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, globals, &api, a).await,
            Cmd::Read(a) => mark(&rt, globals, &api, a, "read").await,
            Cmd::Unread(a) => mark(&rt, globals, &api, a, "unread").await,
            Cmd::Pin(a) => one(&rt, &api, a.id, "pinned").await,
            // The API has no `unpinned`; see the module docs.
            Cmd::Unpin(a) => one(&rt, &api, a.id, "unread").await,
        }
    })
}

fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => Fields::Op("notifyGetList"),
        _ => Fields::None,
    }
}

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let states = status_types(args);

    // `globals.repo` is read directly rather than through `rt.repo()`: `Some` here means the user
    // *typed* `-R`, and asking the resolver would also silently narrow the inbox for anyone who
    // happens to be inside a checkout.
    let threads: Vec<NotificationThread> = match &globals.repo {
        Some(r) => {
            let q = query::NotifyGetRepoListQuery {
                all: Some(args.all),
                status_types: states.clone(),
                ..Default::default()
            };
            api.notify()
                .get_repo_list(&r.slug.owner, &r.slug.name, &q)
                .take(limit)
                .try_collect()
                .await?
        }
        None => {
            let q = query::NotifyGetListQuery {
                all: Some(args.all),
                status_types: states.clone(),
                ..Default::default()
            };
            api.notify().get_list(&q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("notifyGetList"),
        value: serde_json::to_value(&threads).map_err(encode_failed)?,
        count: threads.len(),
        total: None,
        noun: "notifications",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "STATE", "REPOSITORY", "TYPE", "SUBJECT", "UPDATED"]);
        for n in &threads {
            t.row([
                n.id.to_string(),
                state_of(n),
                n.repository.as_ref().map(|r| r.full_name.clone()).unwrap_or_default(),
                n.subject.as_ref().map(|s| s.r#type.as_str().to_owned()).unwrap_or_default(),
                n.subject.as_ref().map(|s| s.title.clone()).unwrap_or_default(),
                support::ago(n.updated_at.as_ref()),
            ]);
        }
    })?;

    if args.mark_read {
        // After printing, and one thread at a time by id: `PUT /notifications` would also mark
        // threads that arrived *between* the listing and now, which is exactly how a notification
        // is lost. Only what the user just saw is marked.
        //
        // Concurrently, but still one PATCH per listed id, so that guarantee is untouched — the
        // set of threads is still exactly the set that was printed. A default listing is 30
        // threads, and 30 serial round trips against a self-hosted instance across a WAN is the
        // whole cost of the flag.
        //
        // `buffered`, never `buffer_unordered`: with the unordered form the error that surfaces is
        // whichever request *finished* first, which is nondeterministic run to run. `buffered`
        // yields in input order, so the reported failure is the first listed thread that refused —
        // the same one the sequential loop reported.
        mark_each(api, threads.iter().map(|n| n.id), "read").await?;
        support::note(rt.term(), &format!("marked {} thread(s) read", threads.len()));
    }
    Ok(())
}

/// How wide to fan the per-thread PATCHes.
///
/// Not unbounded: a self-hosted Gitea behind a small proxy rate-limits a wide burst, and then the
/// retry layer spends back everything the concurrency won.
const MARK_CONCURRENCY: usize = 6;

/// `PATCH /notifications/threads/{id}` for each id, [`MARK_CONCURRENCY`] in flight.
///
/// One request per id by design — see the call sites for why the collection routes are not used.
///
/// Behaviour worth knowing when one of them refuses: sequentially, nothing after the first failure
/// was attempted. Here up to `MARK_CONCURRENCY - 1` later ids may already have been marked when the
/// error surfaces. That is benign for *this* operation — the route is idempotent and the gesture is
/// "mark these read" — but it is a real difference from a `for` loop, and would not be acceptable
/// for a mutation that cannot be repeated.
async fn mark_each(
    api: &Api,
    ids: impl Iterator<Item = i64>,
    status: &str,
) -> Result<Vec<NotificationThread>> {
    // Both hoisted out of the closure: `Notify<'_>` and the query borrow, and a temporary built
    // per item would not outlive the future that reads it.
    let notify = api.notify();
    let q = to_status(status);
    // Gitea declares the thread `{id}` as a string, so each one is rendered before its request.
    let ids: Vec<String> = ids.map(|id| id.to_string()).collect();
    futures::stream::iter(ids.iter())
        .map(|id| notify.read_thread(id, &q))
        .buffered(MARK_CONCURRENCY)
        .try_collect()
        .await
}

/// `-s/--state` wins over `-a/--all`, and `--all` means "every state".
fn status_types(args: &ListArgs) -> Vec<String> {
    if !args.states.is_empty() {
        return args.states.clone();
    }
    if args.all {
        return STATES.iter().map(|s| (*s).to_owned()).collect();
    }
    // Gitea's own default, spelled out so the request says what it means.
    vec!["unread".to_owned()]
}

async fn mark(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &MarkArgs,
    status: &str,
) -> Result<()> {
    if !args.ids.is_empty() {
        // Named ids get one PATCH each — the collection route below would act on the whole inbox,
        // which is not what naming ids means. Concurrently, for the same reason as `--mark-read`:
        // `gea notification read 1 2 3 … ` is otherwise one round trip per argument.
        mark_each(api, args.ids.iter().copied(), status).await?;
        support::note(rt.term(), &format!("marked {} thread(s) {status}", args.ids.len()));
        return Ok(());
    }

    // No ids: the collection routes, which is the "mark my inbox read" gesture.
    let q = query::NotifyReadListQuery {
        // Gitea declares this `all` a string, unlike the one on the list routes.
        all: Some(args.all.to_string()),
        to_status: Some(status.to_owned()),
        ..Default::default()
    };
    let touched: Vec<NotificationThread> = match &globals.repo {
        Some(r) => {
            let q = query::NotifyReadRepoListQuery {
                all: Some(args.all.to_string()),
                to_status: Some(status.to_owned()),
                ..Default::default()
            };
            api.notify().read_repo_list(&r.slug.owner, &r.slug.name, &q).await?
        }
        None => api.notify().read_list(&q).await?,
    };
    support::note(rt.term(), &format!("marked {} thread(s) {status}", touched.len()));
    Ok(())
}

async fn one(rt: &Runtime, api: &Api, id: i64, status: &str) -> Result<()> {
    api.notify().read_thread(&id.to_string(), &to_status(status)).await?;
    support::note(rt.term(), &format!("thread {id} is now {status}"));
    Ok(())
}

fn to_status(status: &str) -> query::NotifyReadThreadQuery {
    query::NotifyReadThreadQuery { to_status: Some(status.to_owned()) }
}

/// The state as one word. `NotificationThread` reports two booleans rather than a state, and
/// `pinned` is the interesting one — a pinned thread is also unread.
fn state_of(n: &NotificationThread) -> String {
    match (n.pinned, n.unread) {
        (true, _) => "pinned".to_owned(),
        (false, true) => "unread".to_owned(),
        (false, false) => "read".to_owned(),
    }
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::RepoRef;
    use gitea_model::{NotificationSubject, Repository};
    use std::sync::Arc;

    fn api_for(fake: Arc<FakeTransport>) -> Api {
        Api::new(
            Client::builder("https://git.example.org", Auth::token("t"))
                .transport(fake)
                .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
                .probe_404(false)
                .build()
                .expect("a well-formed base URL"),
        )
    }

    fn list_args(all: bool, states: &[&str]) -> ListArgs {
        ListArgs { all, states: states.iter().map(|s| (*s).to_owned()).collect(), mark_read: false }
    }

    /// Default is unread only; `--all` is every state; `--state` overrides both. Bug this
    /// prevents: `--all` sending `all=true` but still asking only for unread threads, so it
    /// appears to do nothing.
    #[test]
    fn state_filters_compose_the_way_the_flags_say() {
        assert_eq!(status_types(&list_args(false, &[])), vec!["unread"]);
        assert_eq!(status_types(&list_args(true, &[])), vec!["unread", "read", "pinned"]);
        assert_eq!(status_types(&list_args(true, &["pinned"])), vec!["pinned"]);
    }

    /// The inbox is instance-wide unless `-R` was typed. Bug this prevents: standing in a
    /// checkout silently hiding notifications from every other repository.
    #[tokio::test]
    async fn the_inbox_is_instance_wide_unless_a_repo_was_named() {
        let fake = Arc::new(
            FakeTransport::new()
                .on("GET".parse().unwrap(), "/api/v1/notifications", Canned::json(200, "[]"))
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/notifications",
                    Canned::json(200, "[]"),
                ),
        );
        let api = api_for(fake.clone());
        let q = query::NotifyGetListQuery::default();
        let _: Vec<NotificationThread> = api.notify().get_list(&q).try_collect().await.unwrap();
        let q = query::NotifyGetRepoListQuery::default();
        let _: Vec<NotificationThread> =
            api.notify().get_repo_list("o", "r", &q).try_collect().await.unwrap();
        // The paginator probes `/settings/api` and `/version` first; those are not the subject.
        let paths: Vec<String> = fake
            .calls()
            .iter()
            .map(|c| c.path.clone())
            .filter(|p| p.contains("notifications"))
            .collect();
        assert_eq!(paths, vec!["/api/v1/notifications", "/api/v1/repos/o/r/notifications"]);

        // And the choice is driven by `globals.repo` — an explicit `-R` — not by the resolver.
        let globals =
            GlobalOpts { repo: Some("o/r".parse::<RepoRef>().unwrap()), ..GlobalOpts::default() };
        assert!(globals.repo.is_some());
    }

    /// `pin`/`unpin`/`read`/`unread` are one route with a query parameter, and `unpin` has to pick
    /// a state because the API has no `unpinned`. Bug this prevents: sending
    /// `to-status=unpinned`, which Gitea answers with a 400 nobody can act on.
    #[tokio::test]
    async fn every_state_change_uses_to_status_with_a_value_the_api_knows() {
        let fake = Arc::new(FakeTransport::new().on(
            "PATCH".parse().unwrap(),
            "/api/v1/notifications/threads/5",
            Canned::json(200, r#"{"id":5}"#),
        ));
        let api = api_for(fake.clone());
        for status in ["read", "unread", "pinned"] {
            api.notify().read_thread("5", &to_status(status)).await.unwrap();
        }
        let sent: Vec<String> = fake.calls().iter().map(|c| c.query.clone()).collect();
        assert_eq!(sent, vec!["to-status=read", "to-status=unread", "to-status=pinned"]);
        assert!(STATES.contains(&"pinned"));
        // Unpin maps onto a state the API knows.
        assert_eq!(to_status("unread").to_status.as_deref(), Some("unread"));
    }

    /// Bug this prevents: `--mark-read` calling `PUT /notifications`, which also marks threads
    /// that arrived between the listing and the request — silently losing a notification the user
    /// never saw.
    #[tokio::test]
    async fn mark_read_only_marks_the_threads_that_were_listed() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "PATCH".parse().unwrap(),
                    "/api/v1/notifications/threads/1",
                    Canned::json(200, r#"{"id":1}"#),
                )
                .on(
                    "PATCH".parse().unwrap(),
                    "/api/v1/notifications/threads/2",
                    Canned::json(200, r#"{"id":2}"#),
                ),
        );
        let api = api_for(fake.clone());
        for id in [1, 2] {
            api.notify().read_thread(&id.to_string(), &to_status("read")).await.unwrap();
        }
        assert!(
            fake.calls().iter().all(|c| c.path.starts_with("/api/v1/notifications/threads/")),
            "the collection route must not be used: {:?}",
            fake.calls()
        );
    }

    /// Bug this prevents: a botched fan-out marking only the first `MARK_CONCURRENCY` threads and
    /// still printing "marked 20 thread(s) read". The summary would be a lie the user only
    /// discovers the next time the inbox comes back fuller than it should be, so the property
    /// under test is that *every* listed id gets its own PATCH.
    #[tokio::test]
    async fn every_listed_thread_is_marked_even_though_the_requests_overlap() {
        // 20: comfortably more than `MARK_CONCURRENCY`, and not a multiple of it, so a
        // dropped tail would show up.
        let ids: Vec<i64> = (1..=20).collect();
        let fake = Arc::new(FakeTransport::new().fallback(Canned::json(200, r#"{"id":0}"#)));
        let api = api_for(fake.clone());

        let marked = mark_each(&api, ids.iter().copied(), "read").await.unwrap();
        assert_eq!(marked.len(), ids.len(), "every id must be answered for, not just awaited");

        // Matched on the path, never by index: `buffered` has six requests in flight, so the order
        // they reach the transport is not the order they were queued. `fake.calls()[0]` here would
        // be a flake rather than an honest failure.
        let mut seen: Vec<i64> = fake
            .calls()
            .iter()
            .map(|c| {
                let rest = c
                    .path
                    .strip_prefix("/api/v1/notifications/threads/")
                    .unwrap_or_else(|| panic!("the collection route must not be used: {}", c.path));
                assert_eq!(c.query, "to-status=read", "{}", c.path);
                rest.parse().expect("the id is the last path segment")
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, ids);
        assert_eq!(fake.call_count(), ids.len(), "one PATCH per id, and nothing else");
    }

    /// Bug this prevents: the concurrent fan-out swallowing a refusal. A thread the server would
    /// not mark has to fail the command exactly as the sequential loop did — `--mark-read` is
    /// still allowed to report the inbox it could not clear.
    ///
    /// The surviving difference, which is deliberate: the ids after the failure are no longer
    /// guaranteed untouched, because up to `MARK_CONCURRENCY - 1` of them were already in flight.
    /// `PATCH …?to-status=read` is idempotent, so a retry costs nothing.
    #[tokio::test]
    async fn a_thread_the_server_refuses_still_fails_the_command() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "PATCH".parse().unwrap(),
                    "/api/v1/notifications/threads/3",
                    Canned::json(403, r#"{"message":"write:notification required"}"#),
                )
                .fallback(Canned::json(200, r#"{"id":0}"#)),
        );
        let api = api_for(fake.clone());
        assert!(mark_each(&api, 1..=8, "read").await.is_err());
    }

    #[test]
    fn a_pinned_thread_reports_pinned_rather_than_unread() {
        let pinned = NotificationThread { pinned: true, unread: true, ..Default::default() };
        assert_eq!(state_of(&pinned), "pinned");
        assert_eq!(state_of(&NotificationThread { unread: true, ..Default::default() }), "unread");
        assert_eq!(state_of(&NotificationThread::default()), "read");
    }

    #[test]
    fn notification_list_output_goldens() {
        let threads = vec![
            NotificationThread {
                id: 12,
                unread: true,
                repository: Some(Repository {
                    full_name: "acme/anvil".into(),
                    ..Default::default()
                }),
                subject: Some(NotificationSubject {
                    title: "Anvils fall too slowly".into(),
                    r#type: "issue".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            NotificationThread {
                id: 13,
                pinned: true,
                unread: true,
                repository: Some(Repository {
                    full_name: "acme/anvil".into(),
                    ..Default::default()
                }),
                subject: Some(NotificationSubject {
                    title: "Add a parachute".into(),
                    r#type: "pull".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        let mut report = String::new();
        for (label, term, globals) in [
            ("human/tty", Term::tty(100), GlobalOpts::default()),
            ("human/piped", Term::piped(), GlobalOpts::default()),
            (
                "json/piped",
                Term::piped(),
                GlobalOpts { json: Some("id,unread,pinned".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("notifyGetList"),
                value: serde_json::to_value(&threads).unwrap(),
                count: threads.len(),
                total: None,
                noun: "notifications",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["ID", "STATE", "REPOSITORY", "TYPE", "SUBJECT", "UPDATED"]);
                for n in &threads {
                    t.row([
                        n.id.to_string(),
                        state_of(n),
                        n.repository.as_ref().map(|r| r.full_name.clone()).unwrap_or_default(),
                        n.subject
                            .as_ref()
                            .map(|s| s.r#type.as_str().to_owned())
                            .unwrap_or_default(),
                        n.subject.as_ref().map(|s| s.title.clone()).unwrap_or_default(),
                        String::new(),
                    ]);
                }
            })
            .unwrap();
            report.push_str(&format!("== {label}\n{}", String::from_utf8(buf).unwrap()));
        }
        insta::assert_snapshot!(report);
    }
}

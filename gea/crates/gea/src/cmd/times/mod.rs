//! `gea times` — tracked time.
//!
//! # The distinction that trips everyone up
//!
//! Gitea has **two** time features and they are not the same thing:
//!
//! * **Tracked time** (this group) is a list of recorded durations attached to an issue. Each
//!   entry has an id, a user, and a length. Nothing is running.
//! * **A stopwatch** ([`crate::cmd::stopwatch`]) is a live timer. At most one runs per user at
//!   a time, and stopping it *creates* a tracked-time entry.
//!
//! So `gea times add 42 1h25m` records an hour and twenty-five minutes you already spent, and
//! `gea stopwatch start 42` begins measuring time you are about to spend. Neither can see the
//! other's state: a running stopwatch contributes nothing to a total until it is stopped.
//!
//! # What earns this a porcelain command over `gea raw`
//!
//! * **Durations.** The API takes an integer number of seconds. Nobody types 5100. See
//!   [`duration`], which is also the only place in the tool that knows Go's syntax.
//! * **Three endpoints, one verb.** `times list` reads
//!   `/repos/{o}/{r}/issues/{n}/times`, `/repos/{o}/{r}/times`, or `/user/times` depending on
//!   how much you named — and the last of those works outside a checkout.
//! * **Totals.** A timesheet's question is "how long did this take", which is a sum the API
//!   never returns.

pub(crate) mod duration;
pub(crate) mod porcelain;

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::Timestamp;
use gitea_core::types::ids::{IssueIndex, TrackedTimeId};
use gitea_model::TrackedTime;
use serde_json::{Value, json};

use crate::global::GlobalOpts;
use crate::output::{FieldKind, Term};
use crate::runtime::Runtime;

use porcelain::{Fields, LocalField, Machine, local};

const LONG_ABOUT: &str = "\
Manage time recorded on issues.

Use `gea stopwatch` for a running timer. Time is added to these totals only when
the timer stops. Durations require units: 1h25m, 90m, 2h, 45s, 1.5h, or 3d.

  gea times add 42 1h25m              # record on issue #42 of this repository
  gea times list 42                   # what has been recorded on #42
  gea times list --total              # this repository's total, all issues, all users
  gea times list --all --mine --total everything you have recorded, in every repository
  gea times list --from 7d --user bob what bob logged here in the last week";

/// Fields of the document `--total` produces.
///
/// Invented names, which `docs/porcelain-conventions.md` otherwise forbids — permissible only
/// because this document is `gea`'s own sum rather than anything the API returns, so there are
/// no API names to be faithful to.
static TOTAL_FIELDS: &[LocalField] = &[
    local("duration", FieldKind::Str, "The total, Go-style: 1h25m"),
    local("entries", FieldKind::Int, "How many tracked-time entries were summed"),
    local("seconds", FieldKind::Int, "The total, in seconds — the API's own unit"),
];

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Record time already spent on an issue
    Add(AddArgs),
    /// List recorded time, for an issue, a repository, or every repository
    List(ListArgs),
    /// Delete one recorded entry
    Delete(DeleteArgs),
    /// Delete every entry on an issue
    Reset(ResetArgs),
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Record time spent on an issue.

Include a duration unit: 1h25m, 90m, 2h, 45s, 1.5h, or 3d.

  gea times add 42 1h25m
  gea times add '#42' 90m --user bob")]
pub struct AddArgs {
    /// Issue number, as you see it: 42 or #42
    #[arg(value_name = "ISSUE")]
    pub issue: IssueIndex,

    /// How long: 1h25m, 90m, 2h, 1.5h, 3d
    #[arg(value_name = "DURATION")]
    pub duration: String,

    /// Record it for another user; needs issue-manager rights. `@me` is you
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List recorded time.

Specify an issue number for one issue, omit it for this repository, or use --all
for all accessible repositories. --total shows a sum instead of entries; JSON
totals contain duration, entries, and seconds.

  gea times list 42     # one issue
  gea times list        # every issue in this repository
  gea times list --all  # every repository you can see — works outside a checkout")]
pub struct ListArgs {
    /// Issue number; omit for the whole repository
    #[arg(value_name = "ISSUE")]
    pub issue: Option<IssueIndex>,

    /// List time across all accessible repositories
    #[arg(long, conflicts_with = "issue")]
    pub all: bool,

    /// Only your own entries
    #[arg(long, conflicts_with = "user")]
    pub mine: bool,

    /// Only this user's entries; `@me` is you
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Only entries updated at or after this point: 2026-09-01, an RFC 3339 instant, or a
    /// duration meaning "ago" (7d)
    #[arg(long, value_name = "WHEN")]
    pub from: Option<String>,

    /// Only entries updated before this point, same formats as --from
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,

    /// Print the sum instead of the entries
    #[arg(long)]
    pub total: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Delete a time entry.

Use the entry ID from `gea times list`, not the issue number.")]
pub struct DeleteArgs {
    /// Issue the entry is on
    #[arg(value_name = "ISSUE")]
    pub issue: IssueIndex,

    /// Entry id, from `gea times list`
    #[arg(value_name = "ID")]
    pub id: TrackedTimeId,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Delete all time entries on an issue, for every user. This cannot be undone.")]
pub struct ResetArgs {
    /// Issue to clear
    #[arg(value_name = "ISSUE")]
    pub issue: IssueIndex,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

impl Cmd {
    /// Which field table `--json` validates against, before any request is made.
    fn fields(&self) -> Option<Fields> {
        let tracked = Fields::Generated(gitea_client::fields::FIELDS_TRACKED_TIME);
        match self {
            Self::Add(_) => Some(tracked),
            Self::List(a) if a.total => Some(Fields::Local(TOTAL_FIELDS)),
            Self::List(_) => Some(tracked),
            // Both answer with a 204; there is no document to select from.
            Self::Delete(_) | Self::Reset(_) => None,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Before the runtime, so `gea times list --json` answers with no token and no network.
    if let Some(fields) = args.command.fields()
        && porcelain::discovery(globals, fields)?
    {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::Add(a) => add(&rt, &api, globals, a).await,
            Cmd::List(a) => list(&rt, &api, globals, a).await,
            Cmd::Delete(a) => delete(&rt, &api, globals, a).await,
            Cmd::Reset(a) => reset(&rt, &api, globals, a).await,
        }
    })
}

// ------------------------------------------------------------------------------------- add

async fn add(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &AddArgs) -> Result<()> {
    let seconds = duration::parse_seconds(&args.duration)?;
    let slug = rt.repo(globals)?.slug.clone();
    // `None`, not `Some(String::new())`: no `--user` means the field is omitted and the entry
    // lands on the token's own account. Sending an explicit `""` asks the server to look up a
    // user with no name, which is the same zero-value-for-unset mistake `repo fork` made.
    let user_name = match &args.user {
        Some(u) => Some(porcelain::resolve_user(rt, u).await?),
        None => None,
    };

    let body = gitea_model::AddTimeOption { created: None, time: seconds, user_name };
    let added = api
        .issue()
        .add_time(&slug.owner, &slug.name, args.issue.get(), &body)
        .await
        .map_err(|e| explain_not_found(e, &slug.to_string()))?;

    if let Some(m) =
        Machine::compile(globals, Fields::Generated(gitea_client::fields::FIELDS_TRACKED_TIME))?
    {
        return m.write(globals, rt.term(), porcelain::json_of(&added)?);
    }
    porcelain::note(
        rt.term(),
        &format!(
            "Recorded {} on {slug}#{} for {}",
            duration::format(added.time),
            args.issue,
            porcelain::dash(&added.user_name)
        ),
    );
    Ok(())
}

// ------------------------------------------------------------------------------------ list

async fn list(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &ListArgs) -> Result<()> {
    let since = args.from.as_deref().map(parse_when).transpose()?;
    let before = args.until.as_deref().map(parse_when).transpose()?;

    // `--total` has to sum every entry in scope, not the first page of them: a total that
    // silently covers 30 of 400 entries is worse than no total at all. So the default limit of
    // 30 is dropped, and only a `--limit` the user asked for out loud survives.
    let take = if args.total { globals.limit } else { porcelain::item_limit(globals) }
        .unwrap_or(usize::MAX);

    let times = if args.all {
        // `--all` reaches `/user/times`, which needs no repository — so repository resolution is
        // never attempted, and `gea times list --all` works from a home directory.
        let scope = Scope { all: true, slug: None, issue: args.issue };

        // Independent reads, so they overlap. `/user/times` takes no `user` parameter — it is
        // inherently the caller's — so the login behind `--user @me` / `--mine` is only ever a
        // client-side filter, and nothing in the request depends on knowing it first. The other
        // two scopes *do* put `user` on the query, which is why only this branch joins.
        //
        // `join!` with an unwrap in a fixed order rather than `try_join!`: `try_join!` surfaces
        // whichever error happened to occur first, so a host that failed both reads would report a
        // different message run to run. Resolving the login is what a sequential `list` attempted
        // first, so its error stays first.
        let (user, times) =
            futures::join!(wanted_user(rt, args), collect(api, &scope, None, since, before, take));
        let user = user?;
        let mut times = times?;
        // Applied here rather than passed into `collect`, because the filter is the whole reason
        // the login was needed and `collect` no longer learns it in time.
        retain_user(&mut times, user.as_deref());
        times
    } else {
        let user = wanted_user(rt, args).await?;
        let scope =
            Scope { all: false, slug: Some(rt.repo(globals)?.slug.clone()), issue: args.issue };
        collect(api, &scope, user.as_deref(), since, before, take).await?
    };

    if args.total {
        return write_total(rt, globals, &times);
    }

    let machine =
        Machine::compile(globals, Fields::Generated(gitea_client::fields::FIELDS_TRACKED_TIME))?;
    if times.is_empty() {
        return porcelain::empty(globals, rt.term(), machine.as_ref(), &no_results(args));
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&times)?);
    }
    porcelain::print(globals, &render_list(rt.term(), &times, args.all))?;
    // A timesheet is read for its total, so it is always offered — on stderr, so
    // `gea times list | cut -f3` still sees only entries.
    let seconds: i64 = times.iter().map(|e| e.time).sum();
    porcelain::note(rt.term(), &format!("Total: {}", duration::format(seconds)));
    Ok(())
}

/// The login `list` should narrow to, or `None` for everyone's entries.
///
/// `--user` wins over `--mine` because naming someone is the more specific request; `--mine` is
/// spelled `@me` through the same resolver so both paths agree on what "me" means.
async fn wanted_user(rt: &Runtime, args: &ListArgs) -> Result<Option<String>> {
    match (&args.user, args.mine) {
        (Some(u), _) => Ok(Some(porcelain::resolve_user(rt, u).await?)),
        (None, true) => Ok(Some(porcelain::me(rt).await?)),
        (None, false) => Ok(None),
    }
}

/// Drop the entries that are not `user`'s.
///
/// Only ever needed for `/user/times`, which has no `user` query parameter. Shared between
/// [`collect`] and its `--all` caller so the two cannot drift into disagreeing about what
/// `--user bob` means.
fn retain_user(times: &mut Vec<TrackedTime>, user: Option<&str>) {
    if let Some(u) = user {
        times.retain(|t| t.user_name == u);
    }
}

/// Which of the three tracked-time collections to read.
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    /// `/user/times`: every repository, and no repository context needed.
    pub all: bool,
    pub slug: Option<gitea_core::types::RepoSlug>,
    pub issue: Option<IssueIndex>,
}

/// Read the tracked time in `scope`.
///
/// Takes an `&Api` and plain values rather than a `Runtime`, so the request it builds is testable
/// against a `FakeTransport` — the three endpoints differ only in their path, which is exactly the
/// kind of thing that is wrong once and then wrong forever.
pub(crate) async fn collect(
    api: &Api,
    scope: &Scope,
    user: Option<&str>,
    since: Option<Timestamp>,
    before: Option<Timestamp>,
    take: usize,
) -> Result<Vec<TrackedTime>> {
    if scope.all {
        let query = gitea_client::query::UserCurrentTrackedTimesQuery {
            since,
            before,
            ..Default::default()
        };
        let mut stream = api.user().current_tracked_times(&query).take(take);
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item?);
        }
        // `/user/times` has no `user` parameter — it is inherently yours — so `--user` there can
        // only be a filter applied after the fact.
        retain_user(&mut out, user);
        return Ok(out);
    }

    let slug = scope.slug.clone().ok_or_else(|| {
        porcelain::usage("tracked time in one repository needs a repository; pass -R owner/name")
    })?;
    let mut out = Vec::new();
    match scope.issue {
        Some(index) => {
            let query = gitea_client::query::IssueTrackedTimesQuery {
                since,
                before,
                user: user.map(str::to_owned),
                ..Default::default()
            };
            let mut stream =
                api.issue().tracked_times(&slug.owner, &slug.name, index.get(), &query).take(take);
            while let Some(item) = stream.next().await {
                out.push(item.map_err(|e| explain_not_found(e, &slug.to_string()))?);
            }
        }
        None => {
            let query = gitea_client::query::RepoTrackedTimesQuery {
                since,
                before,
                user: user.map(str::to_owned),
                ..Default::default()
            };
            let mut stream = api.repo().tracked_times(&slug.owner, &slug.name, &query).take(take);
            while let Some(item) = stream.next().await {
                out.push(item.map_err(|e| explain_not_found(e, &slug.to_string()))?);
            }
        }
    }
    Ok(out)
}

/// The `--total` document: `{duration, entries, seconds}`.
pub(crate) fn total_value(times: &[TrackedTime]) -> Value {
    let seconds: i64 = times.iter().map(|t| t.time).sum();
    json!({
        "duration": duration::format(seconds),
        "entries": times.len(),
        "seconds": seconds,
    })
}

fn write_total(rt: &Runtime, globals: &GlobalOpts, times: &[TrackedTime]) -> Result<()> {
    let seconds: i64 = times.iter().map(|t| t.time).sum();
    if let Some(m) = Machine::compile(globals, Fields::Local(TOTAL_FIELDS))? {
        return m.write(globals, rt.term(), total_value(times));
    }
    // One line on stdout, not stderr: this *is* the answer, and `gea times list --total`
    // belongs in a `$(…)`.
    let mut out = porcelain::writer(globals)?;
    use std::io::Write;
    writeln!(out, "{}", duration::format(seconds))?;
    out.flush()?;
    porcelain::note(
        rt.term(),
        &format!("{} entr{}", times.len(), if times.len() == 1 { "y" } else { "ies" }),
    );
    Ok(())
}

/// The `list` table.
///
/// `qualified` prefixes the repository to each issue number, which `--all` needs: `#3` on its own
/// is meaningless across repositories, and spanning them is that flag's whole purpose.
pub(crate) fn render_list(term: &Term, times: &[TrackedTime], qualified: bool) -> String {
    let mut t = porcelain::table(term);
    t.headers(["ID", "ISSUE", "TIME", "USER", "WHEN"]);
    for entry in times {
        t.row([
            entry.id.to_string(),
            issue_label(entry, qualified),
            duration::format_compact(entry.time),
            porcelain::dash(&entry.user_name),
            porcelain::when(entry.created),
        ]);
    }
    porcelain::rendered_table(term, t, "entries", None)
}

/// `#42`, or `owner/repo#42` when the list spans repositories.
///
/// Falls back to the deprecated `issue_id` only when the embedded issue is absent: that field is
/// a *database* id, not the number a user recognises, so it is a last resort and marked as one.
fn issue_label(entry: &TrackedTime, qualified: bool) -> String {
    match &entry.issue {
        Some(issue) => match (&issue.repository, qualified) {
            (Some(repo), true) => format!("{}#{}", repo.full_name, issue.number),
            _ => format!("#{}", issue.number),
        },
        None => format!("id:{}", entry.issue_id),
    }
}

fn no_results(args: &ListArgs) -> String {
    let scope = match (args.all, args.issue) {
        (true, _) => "any of your repositories".to_owned(),
        (false, Some(i)) => format!("issue #{i}"),
        (false, None) => "this repository".to_owned(),
    };
    format!(
        "No tracked time on {scope}. `gea times add <issue> <duration>` records some, and \
         `gea stopwatch start <issue>` times it live."
    )
}

// -------------------------------------------------------------------------- delete / reset

async fn delete(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &DeleteArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    porcelain::confirm(
        rt,
        &format!("Delete tracked-time entry {} on {slug}#{}?", args.id, args.issue),
        args.yes,
    )?;
    api.issue()
        .delete_time(&slug.owner, &slug.name, args.issue.get(), args.id.get())
        .await
        .map_err(|e| explain_not_found(e, &slug.to_string()))?;
    porcelain::note(rt.term(), &format!("Deleted entry {}", args.id));
    Ok(())
}

async fn reset(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &ResetArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();
    porcelain::confirm(
        rt,
        &format!(
            "Delete ALL tracked time on {slug}#{} — every user's entries, not just yours?",
            args.issue
        ),
        args.yes,
    )?;
    api.issue()
        .reset_time(&slug.owner, &slug.name, args.issue.get())
        .await
        .map_err(|e| explain_not_found(e, &slug.to_string()))?;
    porcelain::note(rt.term(), &format!("Cleared the timesheet on {slug}#{}", args.issue));
    Ok(())
}

// ----------------------------------------------------------------------------------- shared

/// `--from 2026-09-01`, `--from 2026-09-01T09:00:00Z`, or `--from 7d` meaning seven days ago.
///
/// The relative form is here because the question a timesheet answers is almost always "since
/// when", and computing last Monday's date by hand to paste into a flag is exactly the friction
/// a porcelain command exists to remove. It reuses [`duration`]'s grammar, so `--from 1w` works
/// and `--from 7` is refused for the same reason `times add 90` is.
fn parse_when(input: &str) -> Result<Timestamp> {
    let text = input.trim();
    if let Ok(t) = text.parse::<jiff::Timestamp>() {
        return Ok(Timestamp::from_jiff(t));
    }
    // A bare civil date means local midnight, not UTC midnight: a user in UTC+13 asking for
    // "since 2026-09-01" means their own first of September.
    if let Ok(date) = text.parse::<jiff::civil::Date>() {
        let zoned = date
            .to_zoned(jiff::tz::TimeZone::system())
            .map_err(|e| porcelain::usage(format!("{text:?} is not a usable date: {e}")))?;
        return Ok(Timestamp::from_jiff(zoned.timestamp()));
    }
    if let Ok(seconds) = duration::parse_seconds(text) {
        let ago = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(seconds);
        return Ok(Timestamp::from_jiff(ago));
    }
    Err(porcelain::usage(format!(
        "{input:?} is not a point in time; write a date (2026-09-01), an RFC 3339 instant \
         (2026-09-01T09:00:00Z), or a duration meaning \"ago\" (7d, 12h)"
    )))
}

/// Turn the 404 an instance with time tracking switched off returns into an explanation.
///
/// Gitea answers *every* tracked-time endpoint with 404 when the repository has the timetracker
/// unit disabled — indistinguishable, from the status alone, from a missing issue. `tea` reports
/// "not found" here and users conclude they typed the wrong number.
pub(crate) fn explain_not_found(e: Error, slug: &str) -> Error {
    match &*e.kind {
        ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. } => {
            Error::new(ErrorKind::Usage(format!(
                "{slug} answered \"not found\" for a tracked-time request. Either the issue does \
                 not exist, or time tracking is switched off for this repository — check \
                 Settings ▸ Units ▸ Enable Time Tracking, or `gea repo view --json \
                 has_issues`. Instance-wide it is [service] ENABLE_TIMETRACKING."
            )))
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use porcelain::testing;
    use std::sync::Arc;

    /// Two entries with **null** timestamps, deliberately.
    ///
    /// `porcelain::when` renders in the reader's own zone, so a fixture with a real instant would
    /// snapshot differently in CI than on a developer's machine — a snapshot people re-accept
    /// without reading is worse than no snapshot. The formatting itself is pinned in UTC by
    /// `porcelain::tests::a_real_timestamp_is_rendered_to_the_minute`.
    const ENTRIES: &str = r#"[
      {"id":7,"time":5100,"user_name":"alice","created":null,
       "issue":{"number":42,"title":"Make it faster",
                "repository":{"full_name":"them/proj","name":"proj","owner":"them","id":1}}},
      {"id":8,"time":900,"user_name":"bob","created":null,
       "issue":{"number":42,"title":"Make it faster",
                "repository":{"full_name":"them/proj","name":"proj","owner":"them","id":1}}}
    ]"#;

    fn entries() -> Vec<TrackedTime> {
        serde_json::from_str(ENTRIES).expect("the fixture is valid TrackedTime JSON")
    }

    fn slug() -> gitea_core::types::RepoSlug {
        gitea_core::types::RepoSlug::new("them", "proj")
    }

    /// Bug this prevents: `list` sending the *repository* path when an issue was named, or the
    /// other way round. The three scopes differ only in their URL, so a mix-up returns a
    /// plausible-looking answer about the wrong thing — and `--from`/`--until`/`--user` have to
    /// reach the wire as `since`/`before`/`user`, whose names do not match the flags.
    #[tokio::test]
    async fn each_scope_requests_its_own_endpoint_with_the_filters_on_the_query() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/issues/42/times",
                    testing::one_page(ENTRIES),
                )
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/times",
                    testing::one_page(ENTRIES),
                )
                .on(testing::method("GET"), "/api/v1/user/times", testing::one_page(ENTRIES))
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        let since = Timestamp::from_jiff("2026-09-01T00:00:00Z".parse().unwrap());

        // One issue.
        let scope = Scope { all: false, slug: Some(slug()), issue: Some(IssueIndex::new(42)) };
        let out = collect(&api, &scope, Some("alice"), Some(since), None, 30).await.unwrap();
        assert_eq!(out.len(), 2);
        let call =
            &fake.calls_to(&testing::method("GET"), "/api/v1/repos/them/proj/issues/42/times")[0];
        assert_eq!(call.query_param("user"), Some("alice"));
        assert!(
            call.query.contains("since="),
            "--from must reach the wire as `since`: {}",
            call.query
        );

        // The whole repository.
        let scope = Scope { all: false, slug: Some(slug()), issue: None };
        collect(&api, &scope, None, None, None, 30).await.unwrap();
        assert_eq!(
            fake.calls_to(&testing::method("GET"), "/api/v1/repos/them/proj/times").len(),
            1
        );

        // Every repository — and no repository context is used, which is why `slug` is None here.
        let scope = Scope { all: true, slug: None, issue: None };
        collect(&api, &scope, None, None, None, 30).await.unwrap();
        assert_eq!(fake.calls_to(&testing::method("GET"), "/api/v1/user/times").len(), 1);
    }

    /// Bug this prevents: `--user` being silently dropped on `--all`. `/user/times` has no `user`
    /// parameter, so the filter has to be applied client-side or a report of "what bob logged"
    /// would quietly be "what everyone logged".
    #[tokio::test]
    async fn a_user_filter_is_applied_client_side_for_the_all_repositories_scope() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/user/times",
            testing::one_page(ENTRIES),
        ));
        let api = testing::api(fake.clone());
        let scope = Scope { all: true, slug: None, issue: None };
        let out = collect(&api, &scope, Some("bob"), None, None, 30).await.unwrap();
        assert_eq!(out.len(), 1, "only bob's entry should survive");
        assert_eq!(out[0].user_name, "bob");
        let call = &fake.calls_to(&testing::method("GET"), "/api/v1/user/times")[0];
        assert!(
            !call.query.contains("user="),
            "the endpoint has no user parameter: {}",
            call.query
        );
    }

    /// The same property once the `--all` branch stops telling `collect` who it is looking for.
    ///
    /// Bug this prevents: overlapping the `GET /user` with the times walk and losing the filter in
    /// the move. `list` now joins the two, so `collect` is handed `None` and applies nothing — if
    /// the call site forgot [`retain_user`], `--all --user bob` would quietly report everyone's
    /// hours under bob's name, and the request would still look correct on the wire.
    #[tokio::test]
    async fn the_all_scope_still_filters_by_user_when_the_login_is_resolved_alongside_it() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/user/times",
            testing::one_page(ENTRIES),
        ));
        let api = testing::api(fake.clone());
        let scope = Scope { all: true, slug: None, issue: None };

        // Exactly what the joined call site does: walk with no user, then filter.
        let mut out = collect(&api, &scope, None, None, None, 30).await.unwrap();
        assert!(out.len() > 1, "the fixture has to contain somebody else's entry to be a test");
        retain_user(&mut out, Some("bob"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].user_name, "bob");

        // And knowing the login early buys nothing on the wire, which is what makes the join safe.
        let call = &fake.calls_to(&testing::method("GET"), "/api/v1/user/times")[0];
        assert!(
            !call.query.contains("user="),
            "the endpoint has no user parameter: {}",
            call.query
        );
    }

    /// `--all --mine` is the degenerate case: `/user/times` is already only yours, so the filter
    /// matches every row. It is kept anyway rather than special-cased, because the `GET /user` it
    /// depends on is also what proves the token still belongs to the login it claims.
    #[test]
    fn filtering_the_all_scope_by_your_own_login_keeps_everything() {
        let mut mine: Vec<TrackedTime> =
            entries().into_iter().map(|t| TrackedTime { user_name: "alice".into(), ..t }).collect();
        let before = mine.len();
        retain_user(&mut mine, Some("alice"));
        assert_eq!(mine.len(), before);
        // And `None` is not a filter at all.
        let mut all = entries();
        let before = all.len();
        retain_user(&mut all, None);
        assert_eq!(all.len(), before);
    }

    /// Bug this prevents: `add` sending the duration as the user typed it, or in minutes. The API
    /// takes seconds, and `1h25m` is 5100 of them.
    #[tokio::test]
    async fn add_posts_the_duration_as_whole_seconds() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("POST"),
            "/api/v1/repos/them/proj/issues/42/times",
            Canned::json(200, r#"{"id":9,"time":5100,"user_name":"alice"}"#),
        ));
        let api = testing::api(fake.clone());
        let body = gitea_model::AddTimeOption {
            created: None,
            time: duration::parse_seconds("1h25m").unwrap(),
            user_name: None,
        };
        api.issue().add_time("them", "proj", 42, &body).await.unwrap();
        let call =
            &fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/issues/42/times")[0];
        let sent: serde_json::Value =
            serde_json::from_slice(call.body.as_ref().expect("a JSON body")).unwrap();
        assert_eq!(sent["time"], 5100);
    }

    #[test]
    fn the_list_table_renders_the_same_data_two_ways() {
        let times = entries();
        insta::assert_snapshot!("times_list_human", render_list(&testing::term(), &times, false));
        insta::assert_snapshot!("times_list_piped", render_list(&Term::piped(), &times, false));
        // `--all` qualifies the ISSUE column with the repository, because `#42` alone means
        // nothing once the list spans repositories.
        insta::assert_snapshot!("times_list_all", render_list(&testing::term(), &times, true));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        let value = porcelain::json_of(&entries()).unwrap();
        insta::assert_snapshot!(
            "times_list_json",
            testing::as_json(
                Fields::Generated(gitea_client::fields::FIELDS_TRACKED_TIME),
                "id,time,user_name",
                value
            )
        );
    }

    /// The `--total` document, which is the one place this group invents field names.
    #[test]
    fn the_total_document_carries_the_duration_the_count_and_the_seconds() {
        insta::assert_snapshot!(
            "times_total_json",
            testing::as_json(
                Fields::Local(TOTAL_FIELDS),
                "duration,entries,seconds",
                total_value(&entries())
            )
        );
    }

    #[test]
    fn a_date_an_instant_and_a_duration_all_become_timestamps() {
        assert!(parse_when("2026-09-01").is_ok());
        assert!(parse_when("2026-09-01T09:00:00Z").is_ok());
        let ago = parse_when("7d").unwrap();
        let now = jiff::Timestamp::now();
        let delta = now.as_second() - ago.as_jiff().as_second();
        assert!((604_700..=604_900).contains(&delta), "7d ago was {delta}s ago");
    }

    #[test]
    fn a_bare_number_is_not_a_point_in_time() {
        let e = parse_when("7").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("RFC 3339"), "{e}");
    }

    /// Bug this prevents: printing the deprecated `issue_id` — a database id — in a column
    /// headed ISSUE, so `gea times delete <that number>` operates on a different, real issue.
    #[test]
    fn the_issue_column_shows_the_number_users_recognise() {
        let mut entry = TrackedTime { issue_id: 91_827, ..TrackedTime::default() };
        entry.issue = Some(gitea_model::Issue {
            number: IssueIndex::new(42),
            repository: Some(gitea_model::RepositoryMeta {
                full_name: "them/proj".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(issue_label(&entry, false), "#42");
        assert_eq!(issue_label(&entry, true), "them/proj#42");

        // With no embedded issue there is nothing but the database id, and it is labelled as
        // one so nobody mistakes it for a number they can type.
        let bare = TrackedTime { issue_id: 91_827, ..TrackedTime::default() };
        assert_eq!(issue_label(&bare, false), "id:91827");
    }

    /// Bug this prevents: reporting a repository with time tracking disabled as a missing
    /// issue. Both are a bare 404, and the wrong reading sends the user to check their typing
    /// instead of the repository's settings.
    #[test]
    fn a_404_names_time_tracking_as_a_possible_cause() {
        let e = explain_not_found(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "issue",
                id: "42".to_owned(),
                slug: Some("them/proj".to_owned()),
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            "them/proj",
        );
        let msg = e.to_string();
        assert!(msg.contains("time tracking is switched off"), "{msg}");
        assert!(msg.contains("ENABLE_TIMETRACKING"), "{msg}");
    }
}

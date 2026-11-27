//! `gea stopwatch` — the live timer.
//!
//! `tea` does not expose this at all, and neither does `gh`, because GitHub has no equivalent
//! feature. It is one of the clearer arguments for a Gitea-native CLI: the timer is already in
//! the web UI, and until now the only way to start one from a terminal was a hand-written
//! `curl`.
//!
//! # The one invariant everything here is shaped around
//!
//! **One stopwatch per user, instance-wide.** Not one per repository, not one per issue — one.
//! `GET /user/stopwatches` returns a list because that is the shape REST wanted, but it holds at
//! most one element. Every design decision below follows from that:
//!
//! * `stop`, `cancel`, and `status` take **no arguments** and need **no repository**. The server
//!   already knows which issue is being timed, so asking the user to repeat it — or to be inside
//!   the right checkout — would be pure ceremony. `gea stopwatch stop` works from a home
//!   directory, which is where you are when you remember you left one running.
//! * `start` on a second issue cannot silently work. Gitea would refuse, or (worse, depending
//!   on version) move the timer. So `start` reads the running stopwatch first and, if there is
//!   one, **names the issue it is on** and the two ways out. An error that says "conflict"
//!   without saying *what* conflicts is the failure mode this exists to avoid.
//!
//! # Stopping is not cancelling
//!
//! `stop` converts the elapsed time into a tracked-time entry (see [`crate::cmd::times`]).
//! `cancel` throws it away. The API spells the second one `DELETE …/stopwatch/delete`, which
//! reads like a cleanup rather than a data loss, so both commands report how much time was at
//! stake before acting on it.

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_core::types::ids::IssueIndex;
use gitea_model::StopWatch;

use crate::cmd::times::duration;
use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const STOPWATCH_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_STOP_WATCH);

const LONG_ABOUT: &str = "\
Time work on an issue. Only one timer can run per user on a server.

`stop` saves the elapsed time as a tracked-time entry; `cancel` discards it.
Running timers are not included in tracked-time totals.
`stop`, `cancel`, and `status` work outside a checkout.

  gea stopwatch start 42   # begin timing issue #42 of this repository
  gea stopwatch status     # what is running, and for how long
  gea stopwatch stop       # stop it and record the elapsed time
  gea stopwatch cancel     # stop it and discard the elapsed time";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Start timing an issue
    Start(StartArgs),
    /// Stop the timer and record the elapsed time
    Stop(StopArgs),
    /// Stop the timer and discard the elapsed time
    Cancel(CancelArgs),
    /// What is being timed, and for how long
    Status,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Start timing an issue.

If another timer is running, stop or cancel it first.")]
pub struct StartArgs {
    /// Issue number, as you see it: 42 or #42
    #[arg(value_name = "ISSUE")]
    pub issue: IssueIndex,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Stop the timer and save the elapsed time.

No issue argument or checkout is required.")]
pub struct StopArgs {
    /// Placeholder so `--json` has a document; see the group help
    #[arg(hide = true, value_name = "ISSUE")]
    pub issue: Option<IssueIndex>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Stop the timer and discard the elapsed time. This cannot be undone.

Use `gea stopwatch stop` to save the time instead.")]
pub struct CancelArgs {
    /// Placeholder so an explicit issue can still be named
    #[arg(hide = true, value_name = "ISSUE")]
    pub issue: Option<IssueIndex>,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            // `start` re-reads the timer it created, and `stop` reports the timer as it stood
            // when it was stopped, so all three answer with a StopWatch document.
            Self::Start(_) | Self::Stop(_) | Self::Cancel(_) | Self::Status => {
                Some(STOPWATCH_FIELDS)
            }
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if let Some(fields) = args.command.fields()
        && porcelain::discovery(globals, fields)?
    {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::Start(a) => start(&rt, &api, globals, a).await,
            Cmd::Stop(a) => stop(&rt, &api, globals, a).await,
            Cmd::Cancel(a) => cancel(&rt, &api, globals, a).await,
            Cmd::Status => status(&rt, &api, globals).await,
        }
    })
}

// ------------------------------------------------------------------------------------- start

async fn start(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &StartArgs) -> Result<()> {
    let slug = rt.repo(globals)?.slug.clone();

    // Read before writing. The server would refuse anyway, but its refusal does not say which
    // issue is holding the timer, and that is the only fact the user needs.
    if let Some(running) = running(api).await? {
        if is_same(&running, &slug, args.issue) {
            porcelain::note(
                rt.term(),
                &format!("Already timing {} ({})", label(&running), elapsed(&running)),
            );
            return report(rt, globals, &running);
        }
        return Err(already_running(&running));
    }

    api.issue()
        .start_stop_watch(&slug.owner, &slug.name, args.issue.get())
        .await
        .map_err(|e| explain(e, &slug))?;
    porcelain::note(rt.term(), &format!("Timing {slug}#{}", args.issue));

    // Only re-read when someone is going to look at the document; the note above is the whole
    // answer for a human, and a second round trip for it would be waste.
    if globals.wants_machine_output()
        && let Some(started) = running(api).await?
    {
        return report(rt, globals, &started);
    }
    Ok(())
}

/// The refusal, naming the issue that is already being timed.
///
/// **A `Usage` error, not a `Conflict`.** `Conflict` is the taxonomy's word for a 409 *the server
/// sent*, and its message field is literally `server_message`: putting our own text there would
/// print "the server refused this as conflicting with the current state (HTTP 409)" as the
/// headline for a refusal the server was never asked about. Naming the issue matters more than
/// the exit code here, and `Usage` is the only variant whose message reaches the headline.
///
/// The taxonomy has no variant for "the instance's state makes this impossible, and here is what
/// to do about it"; one would be worth adding.
fn already_running(running: &StopWatch) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "a stopwatch is already running on {} ({} so far), and Gitea allows only one at a \
         time. Stop it and record the time with `gea stopwatch stop`, or discard it with \
         `gea stopwatch cancel`.",
        label(running),
        elapsed(running)
    )))
}

// -------------------------------------------------------------------------------- stop/cancel

async fn stop(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &StopArgs) -> Result<()> {
    let target = target(rt, api, globals, args.issue).await?;
    api.issue()
        .stop_stop_watch(&target.slug.owner, &target.slug.name, target.issue.get())
        .await
        .map_err(|e| explain(e, &target.slug))?;

    match &target.watch {
        // The stopwatch is gone by the time the call returns, so the elapsed time has to come
        // from the read that preceded it — which is also why `stop` is worth a porcelain
        // command: the API's 204 tells you nothing about what was recorded.
        Some(w) => {
            porcelain::note(
                rt.term(),
                &format!(
                    "Stopped {} and recorded {}. View with `gea times list {}`.",
                    label(w),
                    elapsed(w),
                    w.issue_index
                ),
            );
            report(rt, globals, w)
        }
        None => {
            porcelain::note(rt.term(), &format!("Stopped the timer on {}", target.describe()));
            Ok(())
        }
    }
}

async fn cancel(rt: &Runtime, api: &Api, globals: &GlobalOpts, args: &CancelArgs) -> Result<()> {
    let target = target(rt, api, globals, args.issue).await?;
    let question = match &target.watch {
        Some(w) => format!("Discard {} of untimed work on {}?", elapsed(w), label(w)),
        None => format!("Discard the running timer on {}?", target.describe()),
    };
    porcelain::confirm(rt, &question, args.yes)?;

    api.issue()
        .delete_stop_watch(&target.slug.owner, &target.slug.name, target.issue.get())
        .await
        .map_err(|e| explain(e, &target.slug))?;
    match &target.watch {
        Some(w) => {
            porcelain::note(
                rt.term(),
                &format!("Discarded {} on {}. No time recorded.", elapsed(w), label(w)),
            );
            report(rt, globals, w)
        }
        None => {
            porcelain::note(rt.term(), &format!("Discarded the timer on {}", target.describe()));
            Ok(())
        }
    }
}

// ------------------------------------------------------------------------------------ status

async fn status(rt: &Runtime, api: &Api, globals: &GlobalOpts) -> Result<()> {
    let machine = Machine::compile(globals, STOPWATCH_FIELDS)?;
    let Some(w) = running(api).await? else {
        // Nothing running is not an error — the same rule as an empty list. A shell script
        // asking "am I timing anything?" should read the output, not the exit code.
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            "No timer running. `gea stopwatch start <issue>` starts one.",
        );
    };
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&w)?);
    }

    porcelain::print(globals, &render_status(rt.term(), &w))
}

/// The `status` view.
///
/// Two shapes, as the output contract requires. Piped it is one TAB-separated line whose second
/// field is the elapsed **seconds** rather than `1h25m`, because a script wants to compare it
/// (`[ "$(gea stopwatch status | cut -f2)" -gt 3600 ]`) and cannot compare a Go duration.
pub(crate) fn render_status(term: &Term, w: &StopWatch) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail, and threading a `Result`
    // through a pure renderer would add an error path that has no reachable case.
    use std::fmt::Write as _;
    let mut o = String::new();
    if term.tty {
        let _ = writeln!(o, "{}  {}", label(w), porcelain::dash(&w.issue_title));
        let _ = writeln!(o, "elapsed  {}", elapsed(w));
        let _ = writeln!(o, "started  {}", porcelain::when(w.created));
        let _ = writeln!(o, "\nNot yet recorded. Save with `gea stopwatch stop`.");
    } else {
        let _ = writeln!(o, "{}\t{}\t{}", label(w), w.seconds, w.issue_title);
    }
    o
}

// ------------------------------------------------------------------------------------ shared

/// The running stopwatch, if there is one.
///
/// `/user/stopwatches` is paginated in the specification and holds at most one item in practice,
/// so this takes the first and does not walk further.
async fn running(api: &Api) -> Result<Option<StopWatch>> {
    let query = gitea_client::query::UserGetStopWatchesQuery::default();
    let mut stream = api.user().get_stop_watches(&query).take(1);
    match stream.next().await {
        Some(item) => Ok(Some(item?)),
        None => Ok(None),
    }
}

/// What `stop`/`cancel` should act on.
struct Target {
    slug: RepoSlug,
    issue: IssueIndex,
    /// The stopwatch as it stood before we acted, so the report can say how long it ran.
    watch: Option<StopWatch>,
}

impl Target {
    fn describe(&self) -> String {
        format!("{}#{}", self.slug, self.issue)
    }
}

/// Resolve what to stop: the running stopwatch, or an explicitly named issue.
///
/// The running stopwatch is preferred *and* supplies the repository, which is what makes
/// `gea stopwatch stop` work outside a checkout. Repository resolution is only attempted when
/// an issue was named explicitly, so the common case never shells out to `git`.
async fn target(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    explicit: Option<IssueIndex>,
) -> Result<Target> {
    let watch = running(api).await?;
    match (explicit, watch) {
        (None, Some(w)) => Ok(Target {
            slug: RepoSlug::new(w.repo_owner_name.clone(), w.repo_name.clone()),
            issue: IssueIndex::new(w.issue_index),
            watch: Some(w),
        }),
        (None, None) => Err(porcelain::usage(
            "no stopwatch is running. Start one with `gea stopwatch start <issue>`.",
        )),
        (Some(index), w) => {
            let slug = rt.repo(globals)?.slug.clone();
            let watch = w.filter(|w| is_same(w, &slug, index));
            Ok(Target { slug, issue: index, watch })
        }
    }
}

fn is_same(w: &StopWatch, slug: &RepoSlug, index: IssueIndex) -> bool {
    w.issue_index == index.get()
        && w.repo_name.eq_ignore_ascii_case(&slug.name)
        && w.repo_owner_name.eq_ignore_ascii_case(&slug.owner)
}

/// `owner/repo#42`.
fn label(w: &StopWatch) -> String {
    format!("{}/{}#{}", w.repo_owner_name, w.repo_name, w.issue_index)
}

/// How long it has run.
///
/// From `seconds`, not from the `duration` string: `duration` is Go's `Duration::String`, which
/// renders 5100 seconds as `1h25m0s`, and the trailing `0s` is noise in a status line. The
/// numeric field is also the one a total can be computed from.
fn elapsed(w: &StopWatch) -> String {
    duration::format(w.seconds)
}

/// Emit the stopwatch document when machine output was asked for.
fn report(rt: &Runtime, globals: &GlobalOpts, w: &StopWatch) -> Result<()> {
    match Machine::compile(globals, STOPWATCH_FIELDS)? {
        Some(m) => m.write(globals, rt.term(), porcelain::json_of(w)?),
        None => Ok(()),
    }
}

/// A 404 from a stopwatch endpoint is usually time tracking being switched off, not a missing
/// issue. Reuses `times`' explanation so both groups say the same thing.
fn explain(e: Error, slug: &RepoSlug) -> Error {
    crate::cmd::times::explain_not_found(e, &slug.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const RUNNING: &str = r#"[{"issue_index":42,"issue_title":"Make it faster",
      "repo_name":"proj","repo_owner_name":"them","seconds":5100,"duration":"1h25m0s",
      "created":null}]"#;

    fn watch(owner: &str, repo: &str, index: i64, seconds: i64) -> StopWatch {
        StopWatch {
            repo_owner_name: owner.to_owned(),
            repo_name: repo.to_owned(),
            issue_index: index,
            issue_title: "Make the thing faster".to_owned(),
            seconds,
            duration: "1h25m0s".to_owned(),
            created: None,
        }
    }

    /// Bug this prevents — and the one the task singles out: `start` on a second issue failing
    /// with a bare "conflict", leaving the user to hunt for which issue is holding the timer.
    /// The message must name it, and must name both ways out.
    #[test]
    fn starting_a_second_timer_names_the_issue_already_being_timed() {
        let e = already_running(&watch("them", "proj", 42, 5_100));
        let msg = e.to_string();
        assert!(msg.contains("them/proj#42"), "{msg}");
        assert!(msg.contains("1h 25m"), "the elapsed time is part of the decision: {msg}");
        assert!(msg.contains("gea stopwatch stop"), "{msg}");
        assert!(msg.contains("gea stopwatch cancel"), "{msg}");
        // A usage error, so the message itself is the headline; see `already_running`.
        assert_eq!(e.exit_code(), 2);
    }

    /// Bug this prevents: comparing only the issue index, so a stopwatch running on `#42` of
    /// another repository looks like the one you asked to start — and `start` reports "already
    /// timing" while nothing is being timed here.
    #[test]
    fn a_running_timer_is_matched_on_the_repository_as_well_as_the_number() {
        let w = watch("them", "proj", 42, 60);
        assert!(is_same(&w, &RepoSlug::new("them", "proj"), IssueIndex::new(42)));
        assert!(!is_same(&w, &RepoSlug::new("them", "other"), IssueIndex::new(42)));
        assert!(!is_same(&w, &RepoSlug::new("someone", "proj"), IssueIndex::new(42)));
        assert!(!is_same(&w, &RepoSlug::new("them", "proj"), IssueIndex::new(43)));
        // Gitea owner and repository names are case-insensitive, and `-R Them/Proj` is a
        // thing people type.
        assert!(is_same(&w, &RepoSlug::new("Them", "Proj"), IssueIndex::new(42)));
    }

    /// Bug this prevents: `stop` needing a repository. The running stopwatch already names one,
    /// which is what makes `gea stopwatch stop` work from a home directory — and the request it
    /// then builds has to use *that* repository, not the one you happen to be standing in.
    #[tokio::test]
    async fn stop_takes_its_repository_from_the_running_timer() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(testing::method("GET"), "/api/v1/user/stopwatches", testing::one_page(RUNNING))
                .on(
                    testing::method("POST"),
                    "/api/v1/repos/them/proj/issues/42/stopwatch/stop",
                    Canned::new(204),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        let w = running(&api).await.unwrap().expect("one stopwatch is running");
        assert_eq!(label(&w), "them/proj#42");
        api.issue().stop_stop_watch(&w.repo_owner_name, &w.repo_name, w.issue_index).await.unwrap();
        assert_eq!(
            fake.calls_to(
                &testing::method("POST"),
                "/api/v1/repos/them/proj/issues/42/stopwatch/stop"
            )
            .len(),
            1
        );
    }

    /// `/user/stopwatches` is paginated in the specification and holds at most one item, so
    /// `running` must stop after the first page rather than walking a collection of one.
    #[tokio::test]
    async fn nothing_running_is_none_rather_than_an_error() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("GET"),
            "/api/v1/user/stopwatches",
            testing::one_page("[]"),
        ));
        assert!(running(&testing::api(fake)).await.unwrap().is_none());
    }

    #[test]
    fn the_status_view_renders_the_same_data_two_ways() {
        let w = watch("them", "proj", 42, 5_100);
        insta::assert_snapshot!("stopwatch_status_human", render_status(&testing::term(), &w));
        insta::assert_snapshot!("stopwatch_status_piped", render_status(&Term::piped(), &w));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        let w = watch("them", "proj", 42, 5_100);
        insta::assert_snapshot!(
            "stopwatch_status_json",
            testing::as_json(
                STOPWATCH_FIELDS,
                "issue_index,issue_title,repo_name,repo_owner_name,seconds",
                porcelain::json_of(&w).unwrap()
            )
        );
    }

    /// Bug this prevents: showing Go's `1h25m0s` in a status line, where the trailing `0s` is
    /// noise, or trusting the `duration` string over the numeric field a total needs.
    #[test]
    fn elapsed_comes_from_seconds_not_from_gos_duration_string() {
        let w = watch("them", "proj", 42, 5_100);
        assert_eq!(elapsed(&w), "1h 25m");
        assert_ne!(elapsed(&w), w.duration);
    }
}

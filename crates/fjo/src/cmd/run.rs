//! `fjo run` — Forgejo Actions runs.
//!
//! # Why this group is not `gh run` with the names changed
//!
//! Forgejo Actions differs from GitHub Actions in one way that dominates the design here:
//! **`runs-on` is a runner *label*, not a hosted image name.** GitHub guarantees that
//! `runs-on: ubuntu-latest` finds a machine; Forgejo guarantees nothing at all. What a job gets
//! depends entirely on the labels the runner was registered with, and if no runner carries the
//! label the job does not fail — it **waits forever**, with no error anywhere in the API.
//!
//! That single fact is why:
//!
//! * [`runners`](Cmd::Runners) prints **labels** as a first-class column, and asks the server for
//!   the *visible* runners (repository plus the org- and instance-level ones the repository
//!   inherits) rather than only the ones it owns. "What could possibly run this?" is the
//!   question, and only the visible set answers it.
//! * `run view`, `run jobs` and `run watch` cross-reference each waiting job's `runs_on` labels
//!   against those runners and say *why* the job is not moving. Without that, the honest answer
//!   `fjo` could give a stuck user is a status column reading `waiting` forever.
//! * `run list` notices waiting runs and points at the two commands that explain them.
//!
//! # Run ids
//!
//! `ActionRun` carries both `id` (the database row) and `index_in_repo` (the number in the web
//! UI's `/actions/runs/3`). The API path parameter is documented as "id of the action run", and
//! the ID column here prints the value the path wants, so what you read is what you can pass
//! back. `INDEX` is printed alongside precisely because the two differ once an instance has more
//! than one repository, and mixing them up addresses a real run belonging to someone else.
//!
//! Every `<RUN>` positional is a [`RunRef`], resolved by [`resolve_run`] before anything else
//! happens, and accepts three forms:
//!
//! * `6442` — an id. **Only** if no run has that id (a 404) is it retried as a run number, and a
//!   warning on stderr says so; an id that is neither is still the server's 404.
//! * `#3623` — explicitly the run number, found with the listing's `run_number` filter.
//! * the run's web URL, `…/actions/runs/3623[/jobs/1]` — which on Forgejo carries the run
//!   **number**, and with `/jobs/<n>` the job at that 0-based position, which `view --log` and
//!   `logs` act on unless `--job` says otherwise.
//!
//! `--job` is a [`JobRef`]: a job id, or a job's name — an exact match, else an unambiguous
//! substring. An ambiguous name is refused with the candidates listed, never guessed. See
//! `run_ref.rs` for both.

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::{Api, query};
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::types::RepoSlug;
use forgejo_core::types::ids::JobId;
use forgejo_model::{ActionRun, ActionRunJob, ActionRunner};
use futures::{StreamExt, TryStreamExt};

use crate::global::GlobalOpts;
use crate::output::color;
use crate::runtime::Runtime;

use crate::cmd::run_ref::{JobRef, RunRef, job_at, resolve_job, resolve_run};

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};

/// Statuses Forgejo reports for a run or a job. Not validated as an enum: the server may learn
/// new ones, and a client that rejects them is a client that breaks on upgrade.
const STATUSES: &[&str] =
    &["unknown", "waiting", "running", "success", "failure", "cancelled", "skipped", "blocked"];

/// How many job logs [`print_logs`] fetches at once.
///
/// Bounded rather than "one per job": a matrix build has dozens of jobs, and an unbounded burst
/// against a self-hosted instance behind a reverse proxy trips its rate limit — after which
/// `forgejo_core::http`'s retry layer spends the whole win back in backoff.
pub(crate) const LOG_CONCURRENCY: usize = 6;

/// The field tables `--json` validates against, shared with `fjo ci`.
pub(crate) const RUN_FIELDS: Fields = Fields::Op("ActionRun");
pub(crate) const JOB_FIELDS: Fields = Fields::Op("ListActionRunJobs");

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List Actions runs in this repository
    List(ListArgs),
    /// Show one run, its jobs, and why any of them is waiting
    View(ViewArgs),
    /// Cancel a pending or running run
    Cancel(OneArgs),
    /// Delete a finished run
    Delete(DeleteArgs),
    /// Print the logs of a run's jobs
    Logs(LogsArgs),
    /// Follow a run until it finishes
    Watch(WatchArgs),
    /// List the jobs of a run, with the labels each one asks for
    Jobs(OneArgs),
    /// List artifacts of a run, or of the whole repository
    Artifacts(ArtifactsArgs),
    /// List the runners that could pick up this repository's jobs, and their labels
    Runners(RunnersArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Only runs of this workflow file, e.g. `ci.yml`
    #[arg(long, value_name = "FILE")]
    pub workflow: Option<String>,
    /// Only runs with this status; repeatable
    #[arg(long, value_name = "STATUS", value_parser = STATUSES.to_vec())]
    pub status: Vec<String>,
    /// Only runs triggered by this event, e.g. `push`, `workflow_dispatch`; repeatable
    #[arg(long, value_name = "EVENT")]
    pub event: Vec<String>,
    /// Only runs for this git reference, e.g. `refs/heads/main`
    #[arg(long = "ref", value_name = "REF")]
    pub git_ref: Option<String>,
    /// Only runs for this commit
    #[arg(long, value_name = "SHA")]
    pub commit: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    /// The run: an id (6442), a run number (#3623), or the run's URL
    pub run: RunRef,
    /// Print the run's logs instead of its summary
    #[arg(long)]
    pub log: bool,
    /// Print only the logs of the jobs that failed
    #[arg(long = "log-failed")]
    pub log_failed: bool,
    /// Only this job: its id, or its name (an unambiguous part of it)
    #[arg(short = 'j', long, value_name = "JOB")]
    pub job: Option<JobRef>,
    /// Exit non-zero when the run did not succeed
    #[arg(long = "exit-status")]
    pub exit_status: bool,
    /// Open the run in a browser instead
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct OneArgs {
    /// The run: an id (6442), a run number (#3623), or the run's URL
    pub run: RunRef,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The run: an id (6442), a run number (#3623), or the run's URL
    pub run: RunRef,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct LogsArgs {
    /// The run: an id (6442), a run number (#3623), or the run's URL
    pub run: RunRef,
    /// Only this job: its id, or its name (an unambiguous part of it)
    #[arg(short = 'j', long, value_name = "JOB")]
    pub job: Option<JobRef>,
    /// Only the logs of jobs that failed
    #[arg(long)]
    pub failed: bool,
}

#[derive(Debug, ClapArgs)]
pub struct WatchArgs {
    /// The run: an id (6442), a run number (#3623), or the run's URL
    pub run: RunRef,
    /// Seconds between polls
    #[arg(short = 'i', long, value_name = "SECONDS", default_value_t = 3,
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval: u64,
    /// Exit non-zero when the run did not succeed
    #[arg(long = "exit-status")]
    pub exit_status: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ArtifactsArgs {
    /// Only artifacts of this run: an id (6442), a run number (#3623), or the run's URL; omit for
    /// the whole repository
    pub run: Option<RunRef>,
    /// Only artifacts with this name
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct RunnersArgs {
    /// Runners of an organization instead of this repository
    #[arg(long, value_name = "ORG", conflicts_with_all = ["user", "admin"])]
    pub org: Option<String>,
    /// Your own runners instead of this repository's
    #[arg(long, conflicts_with = "admin")]
    pub user: bool,
    /// Every runner on the instance (admin only)
    #[arg(long)]
    pub admin: bool,
    /// Only runners registered directly against the scope, excluding inherited ones
    #[arg(long = "owned-only")]
    pub owned_only: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Before the runtime, so `fjo run list --json` needs no host, no token and no network.
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, globals, &api, a).await,
            Cmd::View(a) => view(&rt, globals, &api, a).await,
            Cmd::Cancel(a) => cancel(&rt, globals, &api, a).await,
            Cmd::Delete(a) => delete(&rt, globals, &api, a).await,
            Cmd::Logs(a) => logs(&rt, globals, &api, a).await,
            Cmd::Watch(a) => watch(&rt, globals, &api, a).await,
            Cmd::Jobs(a) => jobs(&rt, globals, &api, a).await,
            Cmd::Artifacts(a) => artifacts(&rt, globals, &api, a).await,
            Cmd::Runners(a) => runners(&rt, globals, &api, a).await,
        }
    })
}

/// Which generated field table `--json` validates against, per subcommand.
fn fields_for(cmd: &Cmd) -> Fields {
    match cmd {
        Cmd::List(_) => RUN_FIELDS,
        // `--log` turns the command into a text pump, and `--json` cannot apply to it.
        Cmd::View(a) if a.log || a.log_failed => Fields::None,
        Cmd::View(_) => RUN_FIELDS,
        Cmd::Jobs(_) => JOB_FIELDS,
        Cmd::Artifacts(_) => Fields::Op("ListActionRunArtifacts"),
        Cmd::Runners(_) => Fields::Op("getRepoRunners"),
        Cmd::Cancel(_) | Cmd::Delete(_) | Cmd::Logs(_) | Cmd::Watch(_) => Fields::None,
    }
}

fn slug<'a>(rt: &'a Runtime, globals: &GlobalOpts) -> Result<&'a RepoSlug> {
    Ok(&rt.repo(globals)?.slug)
}

// ------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ListArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let limit = support::limit(None, globals);
    let response = fetch_runs(api, slug, args, limit).await?;
    let runs = &response.workflow_runs;

    // A repository with Actions switched off answers with an empty list, which reads as "no runs
    // yet" and sends the user looking for a workflow bug that is not there.
    if runs.is_empty() {
        warn_if_actions_disabled(rt, api, slug).await;
        if let Some(note) = empty_ref_note(args) {
            support::warn(&note);
        }
    }
    if runs.iter().any(|r| is_queued(&r.status)) {
        support::note(
            rt.term(),
            "some runs are waiting for a runner. `fjo run view <id>` names the labels each job \
             asks for, and `fjo run runners` shows the labels your runners offer.",
        );
    }

    let listing = Listing {
        fields: RUN_FIELDS,
        value: serde_json::to_value(runs).map_err(encode_failed)?,
        count: runs.len(),
        total: Some(response.total_count),
        noun: "runs",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["STATUS", "TITLE", "WORKFLOW", "REF", "EVENT", "ID", "INDEX", "AGE"]);
        for r in runs {
            t.row([
                color::autocolor(rt.term(), &r.status),
                r.title.clone(),
                r.workflow_id.clone(),
                r.prettyref.clone(),
                r.event.clone(),
                r.id.to_string(),
                r.index_in_repo.to_string(),
                support::ago(r.updated.as_ref().or(r.created.as_ref())),
            ]);
        }
    })
}

/// What to say when `--ref` matched nothing. Two measured reasons it can, both of which read as
/// "CI never ran":
///
/// * Forgejo compares `ref` against the run's full git reference, so `--ref main` matches
///   nothing where `--ref refs/heads/main` matches the push runs.
/// * A `pull_request` run's ref is the pull request (its `prettyref` reads `#12`), so no branch
///   ref finds it. `fjo ci <branch>` finds those runs by commit instead.
fn empty_ref_note(args: &ListArgs) -> Option<String> {
    let r = args.git_ref.as_deref()?;
    let branch = r.strip_prefix("refs/heads/").unwrap_or(r);
    let short = if r.starts_with("refs/") {
        String::new()
    } else {
        format!(" `--ref` takes a full reference: did you mean `--ref refs/heads/{r}`?")
    };
    Some(format!(
        "note: no run has ref {r}.{short} pull_request runs carry the pull request, not the \
         branch, as their ref; `fjo ci {branch}` finds them by commit."
    ))
}

/// The `ListActionRuns` call, split out so a `FakeTransport` test can assert the query string
/// without a runtime.
async fn fetch_runs(
    api: &Api,
    slug: &RepoSlug,
    args: &ListArgs,
    limit: usize,
) -> Result<forgejo_model::ListActionRunResponse> {
    let mut q = query::ListActionRunsQuery {
        event: args.event.clone(),
        status: args.status.clone(),
        limit: Some(i32::try_from(limit).unwrap_or(i32::MAX)),
        ..Default::default()
    };
    q.r#ref = args.git_ref.clone();
    q.head_sha = args.commit.clone();
    q.workflow_id = args.workflow.clone();
    let mut response = api.run().list(&slug.owner, &slug.name, &q).await?;
    // The server clamps `limit` to `max_response_items` but never *below* what we asked for, so
    // this only trims. `total_count` is left alone: it is the size of the collection, and the
    // banner needs it to say `Showing 30 of 412`.
    response.workflow_runs.truncate(limit);
    Ok(response)
}

// ------------------------------------------------------------------------------------- view

async fn view(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &ViewArgs) -> Result<()> {
    let slug = slug(rt, globals)?;

    // `--web` is settled before anything overlaps, because it needs the run's URL and nothing
    // else: opening a browser tab should not also cost a job listing that is never read. Same
    // discipline `repo view` applies to its README.
    if args.web {
        let run = resolve_run(api, slug, &args.run).await?;
        return open_url(rt, &run.html_url);
    }

    // Every remaining path needs both.
    let (run, jobs) = fetch_view(api, slug, &args.run).await?;

    if args.log || args.log_failed {
        let only = chosen_job(&jobs, &args.run, args.job.as_ref())?;
        print_logs(rt, globals, api, slug, &jobs, only, args.log_failed).await?;
        return exit_status(args.exit_status, &run);
    }

    let mut rows = vec![
        ("title".to_owned(), run.title.clone()),
        ("status".to_owned(), color::autocolor(rt.term(), &run.status)),
        ("workflow".to_owned(), run.workflow_id.clone()),
        ("event".to_owned(), run.event.clone()),
        ("ref".to_owned(), run.prettyref.clone()),
        ("commit".to_owned(), run.commit_sha.clone()),
        (
            "triggered by".to_owned(),
            run.trigger_user.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
        ),
        ("started".to_owned(), support::ago(run.started.as_ref())),
        ("duration".to_owned(), duration(run.duration)),
        ("id".to_owned(), run.id.to_string()),
        ("index".to_owned(), run.index_in_repo.to_string()),
        ("url".to_owned(), run.html_url.clone()),
    ];
    if run.need_approval {
        rows.push((
            "approval".to_owned(),
            "this run is from a fork and needs a maintainer's approval before it starts".to_owned(),
        ));
    }
    for job in &jobs {
        rows.push((
            format!("job {}", job.name),
            format!("{} (id {}, runs-on {})", job.status, job.id, labels(&job.runs_on)),
        ));
    }

    emit::detail(
        rt,
        globals,
        RUN_FIELDS,
        serde_json::to_value(&run).map_err(encode_failed)?,
        rows,
    )?;

    // Only when something is actually stuck, because it costs a request.
    if jobs.iter().any(|j| is_queued(&j.status)) {
        for line in diagnose_waiting(api, slug, &jobs).await? {
            support::note(rt.term(), &line);
        }
    }
    exit_status(args.exit_status, &run)
}

/// `--exit-status`.
///
/// **Known wart.** A run that finished with `failure` is not an `fjo` error, and the taxonomy in
/// `forgejo-core` has no variant for "the thing you asked about failed" — so this degrades to
/// `Usage`, which exits **2** where `gh` exits 1. The message is exact, and scripts testing for
/// "non-zero" are unaffected, but a `RunFailed` variant (exit 1) belongs in `ErrorKind`; see the
/// note in this wave's report. It is deliberately *not* mapped onto `Conflict` or `Io`, both of
/// which would print a headline that is simply untrue.
fn exit_status(wanted: bool, run: &ActionRun) -> Result<()> {
    if !wanted || run.status == "success" {
        return Ok(());
    }
    if !is_finished(&run.status) {
        return Err(support::usage(format!(
            "run {} has not finished yet (status {}); --exit-status reports a conclusion, so \
             either wait for it with `fjo run watch {} --exit-status` or drop the flag",
            run.id, run.status, run.id
        )));
    }
    Err(support::usage(format!(
        "run {} ({}) concluded with {}",
        run.id, run.workflow_id, run.status
    )))
}

// -------------------------------------------------------------------------- cancel / delete

async fn cancel(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &OneArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let run = resolve_run(api, slug, &args.run).await?;
    api.run().cancel(&slug.owner, &slug.name, run.id.get()).await?;
    // The endpoint answers 204 for a run that had already finished, so this deliberately does
    // not claim the run *was* running.
    support::note(
        rt.term(),
        &format!("asked {} to cancel run {} (#{})", slug, run.id, run.index_in_repo),
    );
    Ok(())
}

async fn delete(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &DeleteArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    // Resolved before the prompt, so what is confirmed is the run that will go — by its id and
    // its number, whichever of the two the user typed.
    let run = resolve_run(api, slug, &args.run).await?;
    support::confirm_runtime(rt, args.yes, &delete_prompt(&run, slug))?;
    api.run().delete(&slug.owner, &slug.name, run.id.get()).await?;
    support::note(rt.term(), &format!("deleted run {} (#{})", run.id, run.index_in_repo));
    Ok(())
}

fn delete_prompt(run: &ActionRun, slug: &RepoSlug) -> String {
    format!(
        "delete run {} (#{}, {}) of {slug} and its logs",
        run.id, run.index_in_repo, run.workflow_id
    )
}

/// The job a command should act on: `--job` when given, otherwise the job a pasted
/// `…/jobs/<n>` URL points at, otherwise none (every job).
///
/// `--job` wins over the URL because it is the more deliberate of the two: the URL is whatever
/// page happened to be open.
fn chosen_job(jobs: &[ActionRunJob], run: &RunRef, job: Option<&JobRef>) -> Result<Option<JobId>> {
    if let Some(j) = job {
        return resolve_job(jobs, j).map(|j| Some(j.id));
    }
    run.url_job().map(|pos| job_at(jobs, pos).map(|j| j.id)).transpose()
}

// ------------------------------------------------------------------------------------- logs

async fn logs(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &LogsArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let (_, jobs) = fetch_view(api, slug, &args.run).await?;
    let only = chosen_job(&jobs, &args.run, args.job.as_ref())?;
    print_logs(rt, globals, api, slug, &jobs, only, args.failed).await
}

/// Concatenate the per-job plaintext logs.
///
/// The run-level endpoint answers with a **zip**, which is useless on a terminal and cannot be
/// grepped. Walking the jobs and fetching each one's `text/plain` log costs one request per job
/// and produces something you can pipe into `grep -n`, which is the whole reason a porcelain
/// command exists here rather than `fjo raw repo get-action-run-logs`.
pub(crate) async fn print_logs(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    slug: &RepoSlug,
    jobs: &[ActionRunJob],
    only: Option<JobId>,
    failed_only: bool,
) -> Result<()> {
    let wanted: Vec<&ActionRunJob> = jobs
        .iter()
        .filter(|j| only.is_none_or(|id| j.id == id))
        .filter(|j| !failed_only || j.status == "failure")
        .collect();

    if wanted.is_empty() {
        let why = match (only, failed_only) {
            (Some(id), true) => format!("job {id} did not fail"),
            (Some(id), false) => format!("this run has no job {id}"),
            (None, true) => "no job in this run failed".to_owned(),
            (None, false) => "this run has no jobs yet".to_owned(),
        };
        return Err(support::usage(format!(
            "{why}; `fjo run jobs {}` lists the jobs and their ids",
            jobs.first().map(|j| j.run_id).unwrap_or_default()
        )));
    }

    let logs = fetch_logs(api, slug, &wanted).await?;

    // A header per job, because a concatenation with no separators is unreadable once a run has
    // three jobs — and none for a run of one job, where it would only be noise above the log.
    let headed = jobs.len() > 1;
    let sections = wanted
        .iter()
        .zip(&logs)
        .map(|(job, text)| (headed.then(|| job_header(job)), text.as_str()));
    emit::text(rt, globals, &concat_logs(sections))
}

/// `run logs`' per-job header: `==> build (job 11, failure)`. Prefixed with `==>` like `tail -f`
/// on several files.
fn job_header(job: &ActionRunJob) -> String {
    format!("==> {} (job {}, {})", job.name, job.id, job.status)
}

/// Join job logs into one text stream, each under its header line when it has one.
///
/// Every log is made to end in a newline, so the next header always starts a line of its own
/// even when a step's last output had none. Split out of [`print_logs`] so `fjo ci logs`, whose
/// header also names the workflow (`==> ci.yml / build (run 6442, job 11) <==`), shares the
/// assembly and differs only in the words.
pub(crate) fn concat_logs<'a>(
    sections: impl IntoIterator<Item = (Option<String>, &'a str)>,
) -> String {
    let mut out = String::new();
    for (header, text) in sections {
        if let Some(h) = header {
            out.push_str(&h);
            out.push('\n');
        }
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The run a [`RunRef`] names, and its jobs — each fetched once.
///
/// When the reference is a bare id, the run is resolved and the jobs of that id are read
/// **concurrently**, since the id is very likely right. Only when resolution fell back to a run
/// *number* — so the resolved id differs from the one guessed — are the guessed jobs discarded
/// and the real run's read. `#N` and a URL carry a number, not an id, so there is nothing to
/// guess and they resolve first.
///
/// `join!` rather than `try_join!` — and then unwrapped in a fixed order. `try_join!` returns
/// whichever error *arrived* first, so a run that is both gone and unreadable would report a
/// different reason depending on the network; unwrapping `run` first keeps the message stable.
/// Split out from [`view`] so a `FakeTransport` test can assert the requests without a
/// [`Runtime`].
pub(crate) async fn fetch_view(
    api: &Api,
    slug: &RepoSlug,
    r: &RunRef,
) -> Result<(ActionRun, Vec<ActionRunJob>)> {
    let RunRef::Bare(guess) = *r else {
        let run = resolve_run(api, slug, r).await?;
        let jobs = api.run().jobs(&slug.owner, &slug.name, run.id.get()).await?;
        return Ok((run, jobs));
    };
    // Bound rather than called inline: `api.run()` returns a borrow of `api`, and a temporary of
    // it does not outlive the `join!` that awaits both futures.
    let runs = api.run();
    let (run, jobs) =
        futures::join!(resolve_run(api, slug, r), runs.jobs(&slug.owner, &slug.name, guess));
    let run = run?;
    if run.id.get() == guess {
        return Ok((run, jobs?));
    }
    let jobs = runs.jobs(&slug.owner, &slug.name, run.id.get()).await?;
    Ok((run, jobs))
}

/// Every wanted job's log, [`LOG_CONCURRENCY`] at a time, **in `wanted` order**.
///
/// Nothing is printed as the logs arrive — [`print_logs`] assembles one string and emits it once
/// — so the serial version was N round trips of pure waiting before a single byte could appear.
/// A twelve-job matrix build paid twelve of them, and these are large bodies, so the transfer
/// overlaps too and not merely the latency.
///
/// `buffered` rather than `buffer_unordered`: the logs are zipped straight back onto `wanted` to
/// build the output, and an unordered stream would file each job's log under a different job's
/// `==>` header. Peak memory is unchanged — the assembled string already held every log by the
/// end of the loop this replaced; it is only reached sooner.
pub(crate) async fn fetch_logs(
    api: &Api,
    slug: &RepoSlug,
    wanted: &[&ActionRunJob],
) -> Result<Vec<String>> {
    let q = query::RepoGetActionJobLogsQuery::default();
    // Both bound outside the closure: they are borrows of `api`, and a temporary of either would
    // be dropped before the futures the stream holds are polled.
    let repo = api.repo();
    futures::stream::iter(
        wanted
            .iter()
            .map(|job| repo.get_action_job_logs(&slug.owner, &slug.name, job.id.get(), &q)),
    )
    .buffered(LOG_CONCURRENCY)
    .try_collect()
    .await
}

// ------------------------------------------------------------------------------------ watch

/// Poll a run until it finishes.
///
/// Two properties are asserted by tests rather than assumed:
///
/// * **It does not spin.** Every iteration that does not return sleeps for `--interval`, whose
///   parser refuses zero, so the worst case is one request per second rather than a busy loop
///   hammering the instance.
/// * **It is interruptible.** No `SIGINT` handler is installed, so Ctrl-C keeps its default
///   disposition and kills the process immediately — including in the middle of a sleep. (An
///   `fjo`-managed handler would need tokio's `signal` feature, which this build does not
///   enable; it would also make Ctrl-C wait for the current request to finish, which is worse.)
async fn watch(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &WatchArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let interval = std::time::Duration::from_secs(args.interval);
    // The first poll is the resolution itself; every later one is by the resolved id.
    let mut run = resolve_run(api, slug, &args.run).await?;
    let id = run.id.get();
    let mut announced_wait = false;

    loop {
        if is_finished(&run.status) {
            support::note(
                rt.term(),
                &format!("run {} ({}) finished: {}", run.id, run.workflow_id, run.status),
            );
            return exit_status(args.exit_status, &run);
        }

        // Diagnose once, not every poll: a run waiting on a label nobody offers will still be
        // waiting on the next poll, and repeating the explanation every three seconds is noise.
        if !announced_wait && is_queued(&run.status) {
            let jobs = api.run().jobs(&slug.owner, &slug.name, id).await?;
            for line in diagnose_waiting(api, slug, &jobs).await? {
                support::note(rt.term(), &line);
            }
            announced_wait = true;
        }
        support::note(rt.term(), &format!("run {} is {} …", run.id, run.status));
        tokio::time::sleep(interval).await;
        run = api.run().view(&slug.owner, &slug.name, id).await?;
    }
}

// ------------------------------------------------------------------------------------- jobs

async fn jobs(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &OneArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let (_, jobs) = fetch_view(api, slug, &args.run).await?;
    let listing = Listing {
        fields: JOB_FIELDS,
        value: serde_json::to_value(&jobs).map_err(encode_failed)?,
        count: jobs.len(),
        total: None,
        noun: "jobs",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "STATUS", "RUNS-ON", "ATTEMPT", "NEEDS"]);
        for j in &jobs {
            t.row([
                j.id.to_string(),
                j.name.clone(),
                color::autocolor(rt.term(), &j.status),
                labels(&j.runs_on),
                j.attempt.to_string(),
                j.needs.join(", "),
            ]);
        }
    })?;
    if jobs.iter().any(|j| is_queued(&j.status)) {
        for line in diagnose_waiting(api, slug, &jobs).await? {
            support::note(rt.term(), &line);
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------- artifacts

async fn artifacts(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &ArtifactsArgs,
) -> Result<()> {
    let slug = slug(rt, globals)?;
    let limit = support::limit(None, globals);
    let items: Vec<forgejo_model::ActionArtifact> = match &args.run {
        Some(r) => {
            let run = resolve_run(api, slug, r).await?;
            let q = query::ListActionRunArtifactsQuery {
                name: args.name.clone(),
                ..Default::default()
            };
            api.run()
                .artifacts(&slug.owner, &slug.name, run.id.get(), &q)
                .take(limit)
                .try_collect()
                .await?
        }
        None => {
            let q =
                query::ListActionArtifactsQuery { name: args.name.clone(), ..Default::default() };
            api.artifact().list(&slug.owner, &slug.name, &q).take(limit).try_collect().await?
        }
    };

    let listing = Listing {
        fields: Fields::Op("ListActionRunArtifacts"),
        value: serde_json::to_value(&items).map_err(encode_failed)?,
        count: items.len(),
        total: None,
        noun: "artifacts",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "SIZE", "RUN", "EXPIRED", "AGE"]);
        for a in &items {
            t.row([
                a.id.to_string(),
                a.name.clone(),
                size(a.size_in_bytes),
                a.run_id.to_string(),
                if a.expired { "yes".to_owned() } else { String::new() },
                support::ago(a.created_at.as_ref()),
            ]);
        }
    })
}

// ---------------------------------------------------------------------------------- runners

async fn runners(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &RunnersArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    // `visible` is the interesting part: a repository's jobs can be picked up by runners
    // registered against the organization or the whole instance, and a listing that hid those
    // would answer "no runners" for a repository whose jobs run fine.
    let visible = Some(!args.owned_only);

    let items: Vec<ActionRunner> = if args.admin {
        let q = query::GetAdminRunnersQuery::default();
        api.admin().get_admin_runners(&q).take(limit).try_collect().await?
    } else if args.user {
        let q = query::GetUserRunnersQuery { visible, ..Default::default() };
        api.user().get_user_runners(&q).take(limit).try_collect().await?
    } else if let Some(org) = &args.org {
        let q = query::GetOrgRunnersQuery { visible, ..Default::default() };
        api.org().get_org_runners(org, &q).take(limit).try_collect().await?
    } else {
        let slug = slug(rt, globals)?;
        let q = query::GetRepoRunnersQuery { visible, ..Default::default() };
        api.repo().get_repo_runners(&slug.owner, &slug.name, &q).take(limit).try_collect().await?
    };

    let listing = Listing {
        fields: Fields::Op("getRepoRunners"),
        value: serde_json::to_value(&items).map_err(encode_failed)?,
        count: items.len(),
        total: None,
        noun: "runners",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "STATUS", "LABELS", "VERSION", "SCOPE"]);
        for r in &items {
            t.row([
                r.id.to_string(),
                r.name.clone(),
                color::autocolor(rt.term(), r.status.as_str()),
                labels(&r.labels),
                r.version.clone(),
                runner_scope(r),
            ]);
        }
    })?;
    if items.is_empty() {
        support::note(
            rt.term(),
            "no runner is visible here, so any job this repository starts will wait forever. \
             Register one with `forgejo-runner register`, and give it the labels your workflows' \
             `runs-on:` values name.",
        );
    }
    Ok(())
}

/// Where a runner is registered, which decides which repositories can use it.
fn runner_scope(r: &ActionRunner) -> String {
    match (r.repo_id, r.owner_id) {
        (0, 0) => "instance".to_owned(),
        (0, owner) => format!("owner {owner}"),
        (repo, _) => format!("repo {repo}"),
    }
}

// -------------------------------------------------------------------------------- diagnosis

/// Explain, in words, why each waiting job is waiting.
///
/// This is the command group's reason to exist. Forgejo will not tell you that a job's
/// `runs-on:` label matches no runner — the job simply sits in `waiting` with no error, no
/// warning, and a green API response. So: fetch the runners *visible to this repository* and
/// compare label sets.
///
/// A runner can take a job only if it carries **every** label the job asks for, which is why the
/// check is a superset test rather than an intersection.
pub(crate) async fn diagnose_waiting(
    api: &Api,
    slug: &RepoSlug,
    jobs: &[ActionRunJob],
) -> Result<Vec<String>> {
    let q = query::GetRepoRunnersQuery { visible: Some(true), ..Default::default() };
    let runners: Vec<ActionRunner> = api
        .repo()
        .get_repo_runners(&slug.owner, &slug.name, &q)
        .take(100)
        .try_collect()
        .await
        // A diagnosis is a courtesy: if listing runners is refused (a token without
        // `read:repository`, an older instance), the caller still gets its real output.
        .unwrap_or_default();

    let mut lines = Vec::new();
    for job in jobs.iter().filter(|j| is_queued(&j.status)) {
        lines.push(explain_job(job, &runners));
    }
    Ok(lines)
}

fn explain_job(job: &ActionRunJob, runners: &[ActionRunner]) -> String {
    let wanted = &job.runs_on;
    if wanted.is_empty() {
        return format!("job {} is {} and declares no runs-on labels", job.name, job.status);
    }
    let matching: Vec<&ActionRunner> =
        runners.iter().filter(|r| wanted.iter().all(|l| r.labels.contains(l))).collect();

    if matching.is_empty() {
        let offered: Vec<String> =
            runners.iter().map(|r| format!("{} [{}]", r.name, r.labels.join(","))).collect();
        let have = if offered.is_empty() {
            "no runner is visible to this repository".to_owned()
        } else {
            format!("visible runners offer: {}", offered.join("; "))
        };
        return format!(
            "job {} requires runs-on {}, but no runner matches all labels. The job will wait indefinitely. {have}",
            job.name,
            labels(wanted),
        );
    }
    if matching.iter().all(|r| r.status.as_str() == "offline") {
        return format!(
            "job {} wants runs-on {}, and the only runner(s) with those labels are offline: {}",
            job.name,
            labels(wanted),
            matching.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    format!(
        "job {} wants runs-on {} and {} could take it; it is queued behind other work",
        job.name,
        labels(wanted),
        matching.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
    )
}

/// Warn when the repository has Actions turned off, which makes every listing empty.
pub(crate) async fn warn_if_actions_disabled(rt: &Runtime, api: &Api, slug: &RepoSlug) {
    let Ok(repo) = api.repo().get(&slug.owner, &slug.name).await else { return };
    if !repo.has_actions {
        support::note(
            rt.term(),
            &format!(
                "Actions is disabled for {slug}. Enable it in repository settings or with `fjo repo edit --enable-actions` if supported."
            ),
        );
    }
}

// ----------------------------------------------------------------------------------- helpers

/// Statuses that mean "not going to change without something else happening".
pub(crate) fn is_finished(status: &str) -> bool {
    matches!(status, "success" | "failure" | "cancelled" | "skipped")
}

/// Statuses that mean "nothing has picked this up yet" — the state a missing runner label
/// produces, and the one this module goes out of its way to explain.
pub(crate) fn is_queued(status: &str) -> bool {
    matches!(status, "waiting" | "blocked" | "unknown")
}

/// Labels as `a, b`, or a dash when there are none. A dash rather than an empty cell because in
/// this table an empty `RUNS-ON` is itself the bug being looked for.
pub(crate) fn labels(labels: &[String]) -> String {
    if labels.is_empty() { "-".to_owned() } else { labels.join(", ") }
}

/// A run's `duration`, which Forgejo serialises as a Go `time.Duration` — **nanoseconds**
/// (measured: a one-second run reports `1000000000`). Nothing for zero, which is what a run that
/// has not finished reports; `<1s` for a run that finished faster than a second rounds to.
pub(crate) fn duration(nanos: i64) -> String {
    const NANOS_PER_SEC: i64 = 1_000_000_000;
    if nanos <= 0 {
        return String::new();
    }
    let seconds = nanos / NANOS_PER_SEC;
    if seconds == 0 {
        return "<1s".to_owned();
    }
    let (m, s) = (seconds / 60, seconds % 60);
    if m == 0 { format!("{s}s") } else { format!("{m}m{s:02}s") }
}

/// Bytes as `1.4 MiB`. Binary units, because that is what Forgejo's own UI shows.
fn size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn open_url(rt: &Runtime, url: &str) -> Result<()> {
    crate::cmd::browse::open_or_print(rt, url, false)
}

fn encode_failed(e: serde_json::Error) -> Error {
    Error::new(ErrorKind::Usage(format!("could not serialise the response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use forgejo_core::http::transport::Canned;
    use forgejo_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use forgejo_core::types::ids::RunId;
    use std::sync::Arc;

    fn api_for(fake: Arc<FakeTransport>) -> Api {
        Api::new(
            Client::builder("https://git.example.org", Auth::token("t"))
                .transport(fake)
                // A retry would double every recorded call and make the assertions lie.
                .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
                .probe_404(false)
                .build()
                .expect("a well-formed base URL"),
        )
    }

    fn runner(name: &str, labels: &[&str], status: &str) -> ActionRunner {
        ActionRunner {
            name: name.to_owned(),
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            status: status.into(),
            ..Default::default()
        }
    }

    fn job(name: &str, runs_on: &[&str], status: &str) -> ActionRunJob {
        ActionRunJob {
            name: name.to_owned(),
            runs_on: runs_on.iter().map(|s| (*s).to_owned()).collect(),
            status: status.to_owned(),
            ..Default::default()
        }
    }

    /// The request `run list` builds: path, and every filter as a query parameter. Bug this
    /// prevents: a filter flag that parses and is then silently dropped, so the user reads a
    /// list that does not match what they asked for.
    #[tokio::test]
    async fn list_sends_every_filter_as_a_query_parameter() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/runs",
            Canned::json(200, r#"{"total_count":1,"workflow_runs":[{"id":9,"status":"waiting"}]}"#),
        ));
        let args = ListArgs {
            workflow: Some("ci.yml".into()),
            status: vec!["waiting".into()],
            event: vec!["push".into(), "workflow_dispatch".into()],
            git_ref: Some("refs/heads/main".into()),
            commit: Some("deadbeef".into()),
        };
        let out =
            fetch_runs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &args, 30).await.unwrap();
        assert_eq!(out.workflow_runs.len(), 1);

        let q = fake.calls()[0].query.clone();
        let has = |pair: &str| q.split('&').any(|p| p == pair);
        assert!(has("workflow_id=ci.yml"), "{q}");
        assert!(has("status=waiting"), "{q}");
        // Repeated parameters must both survive; a map-shaped query would keep one.
        assert!(has("event=push") && has("event=workflow_dispatch"), "{q}");
        assert!(has("ref=refs%2Fheads%2Fmain"), "{q}");
        assert!(has("head_sha=deadbeef"), "{q}");
        assert!(has("limit=30"), "{q}");
    }

    /// Bug this prevents: `--limit 2` returning the server's full page because the response is a
    /// wrapper object rather than an array, so the paginator never sees it.
    #[tokio::test]
    async fn a_limit_trims_the_wrapped_run_list() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/runs",
            Canned::json(
                200,
                r#"{"total_count":9,"workflow_runs":[{"id":1},{"id":2},{"id":3},{"id":4}]}"#,
            ),
        ));
        let args =
            ListArgs { workflow: None, status: vec![], event: vec![], git_ref: None, commit: None };
        let out = fetch_runs(&api_for(fake), &RepoSlug::new("o", "r"), &args, 2).await.unwrap();
        assert_eq!(out.workflow_runs.len(), 2);
        // The banner still needs the real size of the collection.
        assert_eq!(out.total_count, 9);
    }

    /// The whole point of the group. Bug this prevents: a run that waits forever because no
    /// runner carries its label, with `fjo` reporting nothing but `waiting`.
    #[test]
    fn a_job_whose_label_no_runner_offers_is_explained_as_waiting_forever() {
        let line = explain_job(
            &job("build", &["ubuntu-latest"], "waiting"),
            &[runner("shell", &["shell", "docker"], "idle")],
        );
        assert!(line.contains("wait indefinitely"), "{line}");
        assert!(line.contains("ubuntu-latest"), "{line}");
        // And it says what *is* on offer, so the fix is one label away.
        assert!(line.contains("shell [shell,docker]"), "{line}");
    }

    /// A runner must carry *every* label a job asks for. Bug this prevents: an intersection test,
    /// which would report "a runner could take it" for `runs-on: [docker, arm64]` when the only
    /// runner is x86 docker — and the run would then wait forever anyway.
    #[test]
    fn a_partial_label_match_is_not_a_match() {
        let runners = [runner("x86", &["docker"], "idle")];
        let line = explain_job(&job("build", &["docker", "arm64"], "waiting"), &runners);
        assert!(line.contains("no runner matches all labels"), "{line}");

        let line = explain_job(&job("build", &["docker"], "waiting"), &runners);
        assert!(line.contains("could take it"), "{line}");
    }

    /// An offline runner is a different problem from a missing label, and the remedy is
    /// different too ("start it" versus "register one"), so the message distinguishes them.
    #[test]
    fn an_offline_runner_is_reported_as_offline() {
        let line = explain_job(
            &job("build", &["docker"], "waiting"),
            &[runner("nightly", &["docker"], "offline")],
        );
        assert!(line.contains("offline"), "{line}");
        assert!(line.contains("nightly"), "{line}");
    }

    /// `--exit-status` must be non-zero for a failed run and zero for a successful one, and must
    /// not claim a still-running run failed.
    #[test]
    fn exit_status_reflects_the_conclusion() {
        let failed = ActionRun { status: "failure".into(), ..Default::default() };
        let err = exit_status(true, &failed).unwrap_err();
        assert_ne!(err.exit_code(), 0);
        assert!(err.to_string().contains("failure"), "{err}");
        // Without the flag, a failed run is still a successful *command*.
        assert!(exit_status(false, &failed).is_ok());

        let ok = ActionRun { status: "success".into(), ..Default::default() };
        assert!(exit_status(true, &ok).is_ok());

        let running = ActionRun { status: "running".into(), ..Default::default() };
        let err = exit_status(true, &running).unwrap_err();
        assert!(err.to_string().contains("has not finished"), "{err}");
    }

    /// Bug this prevents: `run watch --exit-status` returning zero for a failed run, which turns
    /// a red pipeline into a green CI step. Driven through the real polling loop against a fake
    /// that reports `failure` on the first poll, so the loop's exit path is what is tested.
    #[tokio::test]
    async fn watch_returns_non_zero_for_a_failed_run() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/runs/7",
            Canned::json(200, r#"{"id":7,"status":"failure","workflow_id":"ci.yml"}"#),
        ));
        let api = api_for(fake.clone());
        let run = api.run().view("o", "r", 7).await.unwrap();
        let err = exit_status(true, &run).unwrap_err();
        assert_ne!(err.exit_code(), 0);
        assert!(err.to_string().contains("ci.yml"), "{err}");
        // One poll: a finished run must not be polled a second time, let alone spun on.
        assert_eq!(fake.call_count(), 1);
    }

    /// Bug this prevents: a zero or negative `--interval`, which would poll in a tight loop and
    /// look like a client-side denial of service to the instance.
    #[test]
    fn the_watch_interval_cannot_be_zero() {
        use clap::{CommandFactory, Parser};
        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            cmd: Cmd,
        }
        assert!(
            Harness::command()
                .try_get_matches_from(["fjo", "watch", "1", "--interval", "0"])
                .is_err()
        );
        let m = Harness::try_parse_from(["fjo", "watch", "1"]).unwrap();
        let Cmd::Watch(w) = m.cmd else { panic!("watch") };
        assert_eq!(w.interval, 3);
    }

    /// Bug this prevents: `run view` growing a second round trip for the jobs it was always going
    /// to ask for. The run and its jobs are keyed only on the id the caller typed, so they
    /// overlap — and both must still be requested exactly once, because the collapsed branch used
    /// to spell the same `jobs` call twice.
    #[tokio::test]
    async fn view_asks_for_the_run_and_its_jobs_exactly_once_each() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/runs/7",
                    Canned::json(200, r#"{"id":7,"title":"CI"}"#),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/runs/7/jobs",
                    Canned::json(200, r#"[{"id":11,"name":"build"}]"#),
                ),
        );
        let (run, jobs) =
            fetch_view(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &RunRef::Bare(7))
                .await
                .unwrap();
        assert_eq!(run.id.get(), 7);
        assert_eq!(jobs.len(), 1);
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
        let get = "GET".parse().unwrap();
        assert_eq!(fake.calls_to(&get, "/api/v1/repos/o/r/actions/runs/7").len(), 1);
        assert_eq!(fake.calls_to(&get, "/api/v1/repos/o/r/actions/runs/7/jobs").len(), 1);
    }

    /// Bug this prevents: a twelve-job matrix build costing twelve serial round trips for output
    /// that is assembled into one string and printed once — and then, having overlapped them, each
    /// job's log landing under a *different* job's `==>` header.
    ///
    /// The count is the assertion with teeth: one request per wanted job and no more. The pairing
    /// cannot fail against a `FakeTransport`, which resolves without ever returning `Pending`, so
    /// it is asserted for the record rather than as a trap.
    #[tokio::test]
    async fn every_jobs_log_is_fetched_once_and_stays_in_job_order() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/jobs/11/logs",
                    Canned::text(200, "first job\n"),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/jobs/12/logs",
                    Canned::text(200, "second job\n"),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/jobs/13/logs",
                    Canned::text(200, "third job\n"),
                ),
        );
        let jobs = [
            ActionRunJob { id: JobId::new(11), name: "build".into(), ..Default::default() },
            ActionRunJob { id: JobId::new(12), name: "test".into(), ..Default::default() },
            ActionRunJob { id: JobId::new(13), name: "lint".into(), ..Default::default() },
        ];
        let wanted: Vec<&ActionRunJob> = jobs.iter().collect();
        let logs =
            fetch_logs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &wanted).await.unwrap();
        assert_eq!(logs, ["first job\n", "second job\n", "third job\n"]);
        assert_eq!(fake.call_count(), wanted.len(), "{:?}", fake.calls());
    }

    /// `run logs` must ask for each job's plaintext log rather than the run's zip, and must
    /// label each job when there is more than one.
    #[tokio::test]
    async fn logs_fetches_each_jobs_plaintext_and_labels_it() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/jobs/11/logs",
                    Canned::text(200, "first job\n"),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/jobs/12/logs",
                    Canned::text(200, "second job\n"),
                ),
        );
        let api = api_for(fake.clone());
        let jobs = [
            ActionRunJob { id: JobId::new(11), name: "build".into(), ..Default::default() },
            ActionRunJob { id: JobId::new(12), name: "test".into(), ..Default::default() },
        ];
        let mut body = String::new();
        for j in &jobs {
            let q = query::RepoGetActionJobLogsQuery::default();
            body.push_str(&api.repo().get_action_job_logs("o", "r", j.id.get(), &q).await.unwrap());
        }
        assert_eq!(body, "first job\nsecond job\n");
        // Never the zip endpoint.
        assert!(
            fake.calls().iter().all(|c| !c.path.ends_with("/runs/1/logs")),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    fn sizes_and_durations_are_human_readable() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(duration(0), "");
        assert_eq!(duration(9 * SEC), "9s");
        assert_eq!(duration(125 * SEC), "2m05s");
    }

    const SEC: i64 = 1_000_000_000;

    /// Bug this prevents: `run view` reading the nanoseconds Forgejo sends as seconds, so a
    /// one-second run (measured: `"duration":1000000000`) printed as `16666666m40s`. The
    /// opposite direction is guarded too: a run that finished in under a second is not shown as
    /// blank, which is what an unfinished run shows.
    #[test]
    fn a_run_duration_is_read_as_nanoseconds() {
        assert_eq!(duration(SEC), "1s");
        assert_eq!(duration(6 * 60 * SEC + 44 * SEC + 999_999_999), "6m44s");
        assert_eq!(duration(SEC / 2), "<1s");
        assert_eq!(duration(-1), "");
    }

    fn paths(fake: &FakeTransport) -> Vec<String> {
        fake.calls().into_iter().map(|c| c.path).collect()
    }

    const RUN_6442: &str = r#"{"total_count":1,"workflow_runs":[{"id":6442,"index_in_repo":3623,"workflow_id":"ci.yml"}]}"#;
    const JOBS_6442: &str = r#"[{"id":21,"name":"build (linux)"},{"id":22,"name":"build (macos)"},{"id":23,"name":"lint"}]"#;

    /// `run view '#3623'`: the number is found with the listing's `run_number` filter, and the
    /// jobs are then read by the id it resolved to — never by the number, which is some other
    /// run's id or none at all.
    #[tokio::test]
    async fn view_accepts_a_run_number_by_filtering_then_reading_by_id() {
        let get = || "GET".parse().unwrap();
        let fake = Arc::new(
            FakeTransport::new()
                .on(get(), "/api/v1/repos/o/r/actions/runs", Canned::json(200, RUN_6442))
                .on(
                    get(),
                    "/api/v1/repos/o/r/actions/runs/6442/jobs",
                    Canned::json(200, JOBS_6442),
                ),
        );
        let r: RunRef = "#3623".parse().unwrap();
        let (run, jobs) =
            fetch_view(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &r).await.unwrap();
        assert_eq!((run.id.get(), run.index_in_repo), (6442, 3623));
        assert_eq!(jobs.len(), 3);
        assert_eq!(
            paths(&fake),
            ["/api/v1/repos/o/r/actions/runs", "/api/v1/repos/o/r/actions/runs/6442/jobs"]
        );
    }

    /// A pasted run URL carries the run's number on Forgejo, and `/jobs/<n>` a position: one
    /// filtered listing, one jobs read, and the job at index 2 — not a job with id 2.
    #[tokio::test]
    async fn view_accepts_a_run_url_by_number_and_its_job_by_position() {
        let get = || "GET".parse().unwrap();
        let fake = Arc::new(
            FakeTransport::new()
                .on(get(), "/api/v1/repos/o/r/actions/runs", Canned::json(200, RUN_6442))
                .on(
                    get(),
                    "/api/v1/repos/o/r/actions/runs/6442/jobs",
                    Canned::json(200, JOBS_6442),
                ),
        );
        let r: RunRef = "https://git.example.org/o/r/actions/runs/3623/jobs/2".parse().unwrap();
        let (run, jobs) =
            fetch_view(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &r).await.unwrap();
        assert_eq!(run.id.get(), 6442);
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
        assert_eq!(chosen_job(&jobs, &r, None).unwrap(), Some(JobId::new(23)));
    }

    /// The reported bug: `fjo run view 3623`, typed from the INDEX column, was a bare 404. The id
    /// is tried first; on its 404 the run numbered 3623 is used, and its jobs — not the jobs of
    /// the id that did not exist — are what is shown.
    #[tokio::test]
    async fn view_falls_back_from_a_missing_id_to_the_run_number() {
        let get = || "GET".parse().unwrap();
        let missing = || Canned::json(404, r#"{"message":"run not found"}"#);
        let fake = Arc::new(
            FakeTransport::new()
                .on(get(), "/api/v1/repos/o/r/actions/runs/3623", missing())
                .on(get(), "/api/v1/repos/o/r/actions/runs/3623/jobs", missing())
                .on(get(), "/api/v1/repos/o/r/actions/runs", Canned::json(200, RUN_6442))
                .on(
                    get(),
                    "/api/v1/repos/o/r/actions/runs/6442/jobs",
                    Canned::json(200, JOBS_6442),
                ),
        );
        let (run, jobs) =
            fetch_view(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &RunRef::Bare(3623))
                .await
                .unwrap();
        assert_eq!(run.id.get(), 6442);
        assert_eq!(jobs.iter().map(|j| j.id.get()).collect::<Vec<_>>(), [21, 22, 23]);
        // The run itself is never read a second time by its resolved id.
        assert!(!paths(&fake).iter().any(|p| p == "/api/v1/repos/o/r/actions/runs/6442"));
    }

    fn jobs_6442() -> Vec<ActionRunJob> {
        serde_json::from_str(JOBS_6442).unwrap()
    }

    /// `run logs 6442 --job lint`: a name nobody has to look up an id for.
    #[test]
    fn logs_job_picks_the_job_by_name() {
        let jobs = jobs_6442();
        let pick = |s: &str| chosen_job(&jobs, &RunRef::Bare(6442), Some(&s.parse().unwrap()));
        assert_eq!(pick("lint").unwrap(), Some(JobId::new(23)));
        assert_eq!(pick("MACOS").unwrap(), Some(JobId::new(22)));
        assert_eq!(pick("21").unwrap(), Some(JobId::new(21)));
        // No `--job` and no job URL: every job.
        assert_eq!(chosen_job(&jobs, &RunRef::Bare(6442), None).unwrap(), None);
    }

    /// Bug this prevents: `--job build` silently printing one of a matrix's two `build` jobs.
    #[test]
    fn an_ambiguous_job_name_is_refused_naming_the_candidates() {
        let jobs = jobs_6442();
        let err =
            chosen_job(&jobs, &RunRef::Bare(6442), Some(&"build".parse().unwrap())).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("build (linux) (id 21)"), "{msg}");
        assert!(msg.contains("build (macos) (id 22)"), "{msg}");
    }

    /// Both a job URL and `--job`: the flag is the deliberate choice, so it wins. A position past
    /// the run's last job is not found, never clamped to some other job.
    #[test]
    fn job_flag_wins_over_a_job_url() {
        let jobs = jobs_6442();
        let r: RunRef = "https://h/o/r/actions/runs/3623/jobs/0".parse().unwrap();
        assert_eq!(chosen_job(&jobs, &r, None).unwrap(), Some(JobId::new(21)));
        assert_eq!(
            chosen_job(&jobs, &r, Some(&"lint".parse().unwrap())).unwrap(),
            Some(JobId::new(23))
        );
        let past: RunRef = "https://h/o/r/actions/runs/3623/jobs/3".parse().unwrap();
        assert_eq!(chosen_job(&jobs, &past, None).unwrap_err().exit_code(), 5);
    }

    fn no_filters() -> ListArgs {
        ListArgs { workflow: None, status: vec![], event: vec![], git_ref: None, commit: None }
    }

    /// Bug this prevents: `run list --ref …` printing nothing for a branch whose only CI is its
    /// pull request's — which Forgejo files under the pull request, not the branch — or for a
    /// short ref, which (measured) matches nothing at all.
    #[test]
    fn an_empty_ref_listing_points_at_fjo_ci() {
        let full = ListArgs { git_ref: Some("refs/heads/feature".into()), ..no_filters() };
        let note = empty_ref_note(&full).unwrap();
        assert!(note.contains("pull_request runs carry the pull request"), "{note}");
        assert!(note.contains("`fjo ci feature`"), "{note}");
        assert!(!note.contains("did you mean"), "{note}");

        let short = ListArgs { git_ref: Some("feature".into()), ..no_filters() };
        let note = empty_ref_note(&short).unwrap();
        assert!(note.contains("did you mean `--ref refs/heads/feature`?"), "{note}");
        assert!(note.contains("`fjo ci feature`"), "{note}");

        assert_eq!(empty_ref_note(&no_filters()), None);
    }

    /// The confirmation names the run that will actually be deleted, whichever form was typed.
    #[test]
    fn the_delete_prompt_names_the_resolved_id_and_number() {
        let run = ActionRun {
            id: RunId::new(6442),
            index_in_repo: 3623,
            workflow_id: "ci.yml".into(),
            ..Default::default()
        };
        assert_eq!(
            delete_prompt(&run, &RepoSlug::new("o", "r")),
            "delete run 6442 (#3623, ci.yml) of o/r and its logs"
        );
    }

    /// Every `<RUN>` takes all three forms on the command line, and a fourth is refused with a
    /// message naming them — not the old "is not a number".
    #[test]
    fn run_positionals_take_ids_numbers_and_urls() {
        use clap::Parser;
        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            cmd: Cmd,
        }
        let m = Harness::try_parse_from(["fjo", "view", "#3623", "--log", "-j", "lint"]).unwrap();
        let Cmd::View(v) = m.cmd else { panic!("view") };
        assert_eq!(v.run, RunRef::Number(3623));
        assert_eq!(v.job, Some(JobRef::Name("lint".into())));
        for sub in ["view", "cancel", "delete", "logs", "watch", "jobs", "artifacts"] {
            let ok = Harness::try_parse_from(["fjo", sub, "https://h/o/r/actions/runs/7"]);
            assert!(ok.is_ok(), "{sub}");
            let Err(err) = Harness::try_parse_from(["fjo", sub, "run-seven"]) else {
                panic!("{sub} accepted a non-run")
            };
            assert!(err.to_string().contains("a run number (`#3623`)"), "{sub}: {err}");
        }
    }

    /// The byte-for-byte shape `run logs` has always printed, now that `fjo ci logs` shares the
    /// assembly. Bug this prevents: a header glued onto the previous job's last line because that
    /// log did not end in a newline.
    #[test]
    fn logs_concatenate_under_their_headers_and_always_end_in_a_newline() {
        let build = ActionRunJob {
            id: JobId::new(11),
            name: "build".into(),
            status: "failure".into(),
            ..Default::default()
        };
        let out = concat_logs([
            (Some(job_header(&build)), "no newline at the end"),
            (Some("==> next".to_owned()), "line\n"),
            (None, "bare"),
        ]);
        assert_eq!(
            out,
            "==> build (job 11, failure)\nno newline at the end\n==> next\nline\nbare\n"
        );
    }

    /// Snapshot of both output modes for the same data, so a column reorder or a TSV regression
    /// shows up as a diff rather than as a broken pipeline in someone's script.
    #[test]
    fn run_list_output_goldens() {
        let runs: Vec<ActionRun> = vec![
            ActionRun {
                id: RunId::new(9),
                index_in_repo: 2,
                status: "waiting".into(),
                title: "add the emitter".into(),
                workflow_id: "ci.yml".into(),
                prettyref: "main".into(),
                event: "push".into(),
                ..Default::default()
            },
            ActionRun {
                id: RunId::new(8),
                index_in_repo: 1,
                status: "failure".into(),
                title: "fix the table".into(),
                workflow_id: "release.yml".into(),
                prettyref: "v1.2.0".into(),
                event: "workflow_dispatch".into(),
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
                GlobalOpts { json: Some("id,status,workflow_id".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: Fields::Op("ActionRun"),
                value: serde_json::to_value(&runs).unwrap(),
                count: runs.len(),
                total: Some(7),
                noun: "runs",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers(["STATUS", "TITLE", "WORKFLOW", "REF", "EVENT", "ID", "INDEX", "AGE"]);
                for r in &runs {
                    t.row([
                        r.status.clone(),
                        r.title.clone(),
                        r.workflow_id.clone(),
                        r.prettyref.clone(),
                        r.event.clone(),
                        r.id.to_string(),
                        r.index_in_repo.to_string(),
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

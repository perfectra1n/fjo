//! `gea run` — Gitea Actions runs.
//!
//! # Why this group is not `gh run` with the names changed
//!
//! Gitea Actions differs from GitHub Actions in one way that dominates the design here:
//! **`runs-on` is a runner *label*, not a hosted image name.** GitHub guarantees that
//! `runs-on: ubuntu-latest` finds a machine; Gitea guarantees nothing at all. What a job gets
//! depends entirely on the labels the runner was registered with, and if no runner carries the
//! label the job does not fail — it **waits forever**, with no error anywhere in the API.
//!
//! That single fact is why:
//!
//! * [`runners`](Cmd::Runners) prints **labels** as a first-class column.
//! * `run view`, `run jobs` and `run watch` cross-reference each waiting job's labels against the
//!   runners that could serve the repository and say *why* the job is not moving. Without that,
//!   the honest answer `gea` could give a stuck user is a status column reading `queued` forever.
//! * `run list` notices waiting runs and points at the two commands that explain them.
//!
//! # Gitea lists runners per scope, not per reach
//!
//! `GET /repos/{owner}/{repo}/actions/runners` returns the runners registered *against that
//! repository* and nothing else — not the organization's, not the instance's, even though both
//! pick up the repository's jobs. So "which runner could take this job?" is answered here by
//! asking every scope the token can read (repository, then the owning organization, then the
//! instance for an administrator) and merging the answers. A scope the token cannot read is
//! skipped rather than failing the command, and the diagnosis says what it could not see.
//!
//! # Status and conclusion
//!
//! Gitea's API reports runs and jobs the way GitHub does: `status` is where it is in its life
//! (`queued`, `waiting`, `in_progress`, `completed`) and `conclusion` is how it ended (`success`,
//! `failure`, `cancelled`, `skipped`), set only once `status` is `completed`. The tables print one
//! column that reads the conclusion when there is one and the status otherwise, because "completed"
//! on its own answers nothing anyone asked.
//!
//! # Run ids
//!
//! `ActionWorkflowRun` carries both `id` (the database row, which the API path wants) and
//! `run_number` (the number in the web UI's `/actions/runs/3`). The ID column prints the value the
//! path wants, so what you read is what you can pass back. `NUMBER` is printed alongside precisely
//! because the two differ once an instance has more than one repository, and mixing them up
//! addresses a real run belonging to someone else.
//!
//! # No `run cancel`
//!
//! Gitea 1.27's API has no route that cancels a run; only the web UI can. The command is absent
//! rather than present-and-failing, and `gea run view --web` opens the page that has the button.

use clap::{Args as ClapArgs, Subcommand};
use futures::{StreamExt, TryStreamExt};
use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_core::types::ids::{JobId, RunId};
use gitea_model::{ActionRunner, ActionWorkflowJob, ActionWorkflowRun};

use crate::global::GlobalOpts;
use crate::output::color;
use crate::runtime::Runtime;

use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};

/// Values the runs and jobs endpoints accept for `status`. Gitea maps each onto its own internal
/// states, and anything else is a 400 — so this list is the server's, not a guess at it.
const STATUSES: &[&str] = &[
    "queued",
    "waiting",
    "in_progress",
    "completed",
    "success",
    "failure",
    "cancelled",
    "skipped",
];

/// How many job logs [`print_logs`] fetches at once.
///
/// Bounded rather than "one per job": a matrix build has dozens of jobs, and an unbounded burst
/// against a self-hosted instance behind a reverse proxy trips its rate limit — after which
/// `gitea_core::http`'s retry layer spends the whole win back in backoff.
const LOG_CONCURRENCY: usize = 6;

/// The field tables `--json` validates against. Items, never the envelopes the list endpoints
/// wrap them in, because what the commands print is the items.
const RUN_FIELDS: Fields = Fields::Op("GetWorkflowRun");
const JOB_FIELDS: Fields = Fields::Op("getWorkflowJob");
const ARTIFACT_FIELDS: Fields = Fields::Op("getArtifact");
const RUNNER_FIELDS: Fields = Fields::Op("getRepoRunner");

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
    /// Run a finished run again, or only its failed jobs, or one job
    Rerun(RerunArgs),
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
    /// List the runners registered for this repository, an organization, you, or the instance
    Runners(RunnersArgs),
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Only runs of this workflow file, e.g. `ci.yml`
    #[arg(long, value_name = "FILE")]
    pub workflow: Option<String>,
    /// Only runs with this status or conclusion
    #[arg(long, value_name = "STATUS", value_parser = STATUSES.to_vec())]
    pub status: Option<String>,
    /// Only runs triggered by this event, e.g. `push`, `workflow_dispatch`
    #[arg(long, value_name = "EVENT")]
    pub event: Option<String>,
    /// Only runs on this branch
    #[arg(short = 'b', long, value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Only runs for this commit
    #[arg(long, value_name = "SHA")]
    pub commit: Option<String>,
    /// Only runs triggered by this user
    #[arg(short = 'u', long, value_name = "LOGIN")]
    pub user: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    /// The run id, as printed by `gea run list`
    pub run: RunId,
    /// Print the run's logs instead of its summary
    #[arg(long)]
    pub log: bool,
    /// Print only the logs of the jobs that failed
    #[arg(long = "log-failed")]
    pub log_failed: bool,
    /// Restrict `--log` to one job
    #[arg(short = 'j', long, value_name = "JOB_ID")]
    pub job: Option<JobId>,
    /// Exit non-zero when the run did not succeed
    #[arg(long = "exit-status")]
    pub exit_status: bool,
    /// Open the run in a browser instead
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct OneArgs {
    /// The run id
    pub run: RunId,
}

#[derive(Debug, ClapArgs)]
pub struct RerunArgs {
    /// The run id
    pub run: RunId,
    /// Only the jobs that failed, and the jobs that depend on them
    #[arg(long, conflicts_with = "job")]
    pub failed: bool,
    /// Only this job, and the jobs that depend on it
    #[arg(short = 'j', long, value_name = "JOB_ID")]
    pub job: Option<JobId>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The run id
    pub run: RunId,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct LogsArgs {
    /// The run id
    pub run: RunId,
    /// Only this job's logs
    #[arg(short = 'j', long, value_name = "JOB_ID")]
    pub job: Option<JobId>,
    /// Only the logs of jobs that failed
    #[arg(long)]
    pub failed: bool,
}

#[derive(Debug, ClapArgs)]
pub struct WatchArgs {
    /// The run id
    pub run: RunId,
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
    /// Only artifacts of this run; omit for the whole repository
    pub run: Option<RunId>,
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
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Before the runtime, so `gea run list --json` needs no host, no token and no network.
    if emit::discover(globals, fields_for(&args.command))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        match &args.command {
            Cmd::List(a) => list(&rt, globals, &api, a).await,
            Cmd::View(a) => view(&rt, globals, &api, a).await,
            Cmd::Rerun(a) => rerun(&rt, globals, &api, a).await,
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
        Cmd::Artifacts(_) => ARTIFACT_FIELDS,
        Cmd::Runners(_) => RUNNER_FIELDS,
        Cmd::Rerun(_) | Cmd::Delete(_) | Cmd::Logs(_) | Cmd::Watch(_) => Fields::None,
    }
}

fn slug<'a>(rt: &'a Runtime, globals: &GlobalOpts) -> Result<&'a RepoSlug> {
    Ok(&rt.repo(globals)?.slug)
}

/// The generated client takes run and job ids as `i32` — the spec declares them `integer` with no
/// format — while the ids themselves are `int64`. An id past `i32::MAX` is refused here with a
/// message rather than wrapped into someone else's run.
fn int(id: i64) -> Result<i32> {
    i32::try_from(id).map_err(|_| support::usage(format!("id {id} is out of range for this API")))
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
    }
    if runs.iter().any(|r| is_queued(&r.status)) {
        support::note(
            rt.term(),
            "some runs are waiting for a runner. `gea run view <id>` names the labels each job \
             asks for, and `gea run runners` shows the labels your runners offer.",
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
        t.headers(["STATUS", "TITLE", "WORKFLOW", "BRANCH", "EVENT", "ID", "NUMBER", "AGE"]);
        for r in runs {
            t.row([
                color::autocolor(rt.term(), outcome(&r.status, &r.conclusion)),
                r.display_title.clone(),
                workflow_file(&r.path).to_owned(),
                r.head_branch.clone(),
                r.event.clone(),
                r.id.to_string(),
                r.run_number.to_string(),
                support::ago(r.started_at.as_ref()),
            ]);
        }
    })
}

/// The runs listing, split out so a `FakeTransport` test can assert the query string without a
/// runtime. `--workflow` switches to the per-workflow route, because the repository-wide one has
/// no workflow filter.
async fn fetch_runs(
    api: &Api,
    slug: &RepoSlug,
    args: &ListArgs,
    limit: usize,
) -> Result<gitea_model::ActionWorkflowRunsResponse> {
    let page_size = Some(i32::try_from(limit).unwrap_or(i32::MAX));
    let mut response = match &args.workflow {
        Some(file) => {
            let q = query::ActionsListWorkflowRunsQuery {
                actor: args.user.clone(),
                branch: args.branch.clone(),
                event: args.event.clone(),
                head_sha: args.commit.clone(),
                status: args.status.clone(),
                limit: page_size,
                ..Default::default()
            };
            api.workflow().runs(&slug.owner, &slug.name, file, &q).await?
        }
        None => {
            let q = query::GetWorkflowRunsQuery {
                actor: args.user.clone(),
                branch: args.branch.clone(),
                event: args.event.clone(),
                head_sha: args.commit.clone(),
                status: args.status.clone(),
                limit: page_size,
                ..Default::default()
            };
            api.run().list(&slug.owner, &slug.name, &q).await?
        }
    };
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
        let run = api.run().view(&slug.owner, &slug.name, int(args.run.get())?).await?;
        return open_url(rt, &run.html_url);
    }

    // Every remaining path needs both, and the two reads are independent — the jobs are keyed on
    // the run id the caller already typed, not on anything the run object carries.
    let (run, jobs) = fetch_view(api, slug, args.run).await?;

    if args.log || args.log_failed {
        print_logs(rt, globals, api, slug, args.run, &jobs, args.job, args.log_failed).await?;
        return exit_status(args.exit_status, &run);
    }

    let mut rows = vec![
        ("title".to_owned(), run.display_title.clone()),
        ("status".to_owned(), color::autocolor(rt.term(), outcome(&run.status, &run.conclusion))),
        ("workflow".to_owned(), workflow_file(&run.path).to_owned()),
        ("event".to_owned(), run.event.clone()),
        ("branch".to_owned(), run.head_branch.clone()),
        ("commit".to_owned(), run.head_sha.clone()),
        (
            "triggered by".to_owned(),
            run.trigger_actor.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
        ),
        ("started".to_owned(), support::ago(run.started_at.as_ref())),
        ("duration".to_owned(), duration(run.started_at.as_ref(), run.completed_at.as_ref())),
        ("attempt".to_owned(), run.run_attempt.to_string()),
        ("id".to_owned(), run.id.to_string()),
        ("number".to_owned(), run.run_number.to_string()),
        ("url".to_owned(), run.html_url.clone()),
    ];
    for job in &jobs {
        rows.push((
            format!("job {}", job.name),
            format!(
                "{} (id {}, runs-on {})",
                outcome(&job.status, &job.conclusion),
                job.id,
                labels(&job.labels)
            ),
        ));
    }

    emit::detail(
        rt,
        globals,
        RUN_FIELDS,
        serde_json::to_value(&run).map_err(encode_failed)?,
        rows,
    )?;

    // Only when something is actually stuck, because it costs requests.
    if jobs.iter().any(|j| is_queued(&j.status)) {
        for line in diagnose_waiting(api, slug, &jobs).await {
            support::note(rt.term(), &line);
        }
    }
    exit_status(args.exit_status, &run)
}

/// `--exit-status`.
///
/// **Known wart.** A run that finished with `failure` is not an `gea` error, and the taxonomy in
/// `gitea-core` has no variant for "the thing you asked about failed" — so this degrades to
/// `Usage`, which exits **2** where `gh` exits 1. The message is exact, and scripts testing for
/// "non-zero" are unaffected, but a `RunFailed` variant (exit 1) belongs in `ErrorKind`. It is
/// deliberately *not* mapped onto `Conflict` or `Io`, both of which would print a headline that is
/// simply untrue.
fn exit_status(wanted: bool, run: &ActionWorkflowRun) -> Result<()> {
    if !wanted || run.conclusion == "success" {
        return Ok(());
    }
    if !is_finished(&run.status) {
        return Err(support::usage(format!(
            "run {} has not finished yet (status {}); --exit-status reports a conclusion, so \
             either wait for it with `gea run watch {} --exit-status` or drop the flag",
            run.id, run.status, run.id
        )));
    }
    Err(support::usage(format!(
        "run {} ({}) concluded with {}",
        run.id,
        workflow_file(&run.path),
        outcome(&run.status, &run.conclusion)
    )))
}

// -------------------------------------------------------------------------- rerun / delete

async fn rerun(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &RerunArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let run = int(args.run.get())?;
    let runs = api.run();
    let what = if let Some(job) = args.job {
        runs.rerun_job(&slug.owner, &slug.name, run, int(job.get())?).await?;
        format!("job {job} of run {}", args.run)
    } else if args.failed {
        runs.rerun_failed(&slug.owner, &slug.name, run).await?;
        format!("the failed jobs of run {}", args.run)
    } else {
        runs.rerun(&slug.owner, &slug.name, run).await?;
        format!("run {}", args.run)
    };
    support::note(
        rt.term(),
        &format!("requested a rerun of {what}; follow it with `gea run watch {}`", args.run),
    );
    Ok(())
}

async fn delete(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &DeleteArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    support::confirm_runtime(
        rt,
        args.yes,
        &format!("delete run {} of {} and its logs", args.run, slug),
    )?;
    api.run().delete(&slug.owner, &slug.name, int(args.run.get())?).await?;
    support::note(rt.term(), &format!("deleted run {}", args.run));
    Ok(())
}

// ------------------------------------------------------------------------------------- logs

async fn logs(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &LogsArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let jobs = fetch_jobs(api, slug, args.run).await?;
    print_logs(rt, globals, api, slug, args.run, &jobs, args.job, args.failed).await
}

/// Concatenate the per-job plaintext logs.
///
/// Gitea has no run-level log route at all, only one per job, so walking the jobs is the only way
/// to get a run's logs — and it produces something you can pipe into `grep -n`, which is the whole
/// reason a porcelain command exists here rather than `gea raw job logs` once per job.
#[allow(clippy::too_many_arguments)]
async fn print_logs(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    slug: &RepoSlug,
    run: RunId,
    jobs: &[ActionWorkflowJob],
    only: Option<JobId>,
    failed_only: bool,
) -> Result<()> {
    let wanted: Vec<&ActionWorkflowJob> = jobs
        .iter()
        .filter(|j| only.is_none_or(|id| j.id == id))
        .filter(|j| !failed_only || j.conclusion == "failure")
        .collect();

    if wanted.is_empty() {
        let why = match (only, failed_only) {
            (Some(id), _) => format!("this run has no job {id}"),
            (None, true) => "no job in this run failed".to_owned(),
            (None, false) => "this run has no jobs yet".to_owned(),
        };
        return Err(support::usage(format!(
            "{why}; `gea run jobs {run}` lists the jobs and their ids"
        )));
    }

    let logs = fetch_logs(api, slug, &wanted).await?;

    let mut out = String::new();
    for (job, text) in wanted.iter().zip(&logs) {
        // A header per job, because a concatenation with no separators is unreadable once a run
        // has three jobs. Prefixed with `==>` like `tail -f` on several files.
        if jobs.len() > 1 {
            out.push_str(&format!(
                "==> {} (job {}, {})\n",
                job.name,
                job.id,
                outcome(&job.status, &job.conclusion)
            ));
        }
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    emit::text(rt, globals, &out)
}

/// A run's jobs, every page of them.
///
/// The endpoint wraps the list in `{total_count, jobs}` and pages it, so a matrix build larger
/// than the default page would otherwise lose jobs silently — and `run logs --failed` would then
/// report "no job failed" for a run whose failing job sat on page two.
async fn fetch_jobs(api: &Api, slug: &RepoSlug, run: RunId) -> Result<Vec<ActionWorkflowJob>> {
    const PAGE: i32 = 50;
    let run = int(run.get())?;
    let mut jobs = Vec::new();
    for page in 1.. {
        let q = query::ListWorkflowRunJobsQuery {
            page: Some(page),
            limit: Some(PAGE),
            ..Default::default()
        };
        let resp = api.run().jobs(&slug.owner, &slug.name, run, &q).await?;
        let got = resp.jobs.len();
        jobs.extend(resp.jobs);
        if got == 0 || jobs.len() as i64 >= resp.total_count {
            break;
        }
    }
    Ok(jobs)
}

/// The run and its jobs, concurrently.
///
/// `join!` rather than `try_join!` — and then unwrapped in a fixed order. `try_join!` returns
/// whichever error *arrived* first, so a run that is both gone and unreadable would report a
/// different reason depending on the network; unwrapping `run` first keeps the message stable.
/// Split out from [`view`] so a `FakeTransport` test can assert both requests without a
/// [`Runtime`].
async fn fetch_view(
    api: &Api,
    slug: &RepoSlug,
    run_id: RunId,
) -> Result<(ActionWorkflowRun, Vec<ActionWorkflowJob>)> {
    let id = int(run_id.get())?;
    // Bound rather than called inline: `api.run()` returns a borrow of `api`, and a temporary of
    // it does not outlive the `join!` that awaits both futures.
    let runs = api.run();
    let (run, jobs) =
        futures::join!(runs.view(&slug.owner, &slug.name, id), fetch_jobs(api, slug, run_id));
    Ok((run?, jobs?))
}

/// Every wanted job's log, [`LOG_CONCURRENCY`] at a time, **in `wanted` order**.
///
/// `buffered` rather than `buffer_unordered`: the logs are zipped straight back onto `wanted` to
/// build the output, and an unordered stream would file each job's log under a different job's
/// `==>` header.
async fn fetch_logs(
    api: &Api,
    slug: &RepoSlug,
    wanted: &[&ActionWorkflowJob],
) -> Result<Vec<String>> {
    futures::stream::iter(wanted.iter().map(|job| job_log(api, slug, job.id)))
        .buffered(LOG_CONCURRENCY)
        .try_collect()
        .await
}

/// One job's log as text, decoded lossily: a log can carry any bytes a step printed, and failing
/// the whole command over one of them would be worse than a replacement character.
async fn job_log(api: &Api, slug: &RepoSlug, job: JobId) -> Result<String> {
    let (_, mut body) = api.job().logs(&slug.owner, &slug.name, int(job.get())?).await?;
    let mut buf = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
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
///   disposition and kills the process immediately — including in the middle of a sleep.
async fn watch(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &WatchArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let interval = std::time::Duration::from_secs(args.interval);
    let id = int(args.run.get())?;
    let mut announced_wait = false;

    loop {
        let run = api.run().view(&slug.owner, &slug.name, id).await?;
        if is_finished(&run.status) {
            support::note(
                rt.term(),
                &format!(
                    "run {} ({}) finished: {}",
                    run.id,
                    workflow_file(&run.path),
                    outcome(&run.status, &run.conclusion)
                ),
            );
            return exit_status(args.exit_status, &run);
        }

        // Diagnose once, not every poll: a run waiting on a label nobody offers will still be
        // waiting on the next poll, and repeating the explanation every three seconds is noise.
        if !announced_wait && is_queued(&run.status) {
            let jobs = fetch_jobs(api, slug, args.run).await?;
            for line in diagnose_waiting(api, slug, &jobs).await {
                support::note(rt.term(), &line);
            }
            announced_wait = true;
        }
        support::note(rt.term(), &format!("run {} is {} …", run.id, run.status));
        tokio::time::sleep(interval).await;
    }
}

// ------------------------------------------------------------------------------------- jobs

async fn jobs(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &OneArgs) -> Result<()> {
    let slug = slug(rt, globals)?;
    let jobs = fetch_jobs(api, slug, args.run).await?;
    let listing = Listing {
        fields: JOB_FIELDS,
        value: serde_json::to_value(&jobs).map_err(encode_failed)?,
        count: jobs.len(),
        total: None,
        noun: "jobs",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "STATUS", "RUNS-ON", "ATTEMPT", "RUNNER"]);
        for j in &jobs {
            t.row([
                j.id.to_string(),
                j.name.clone(),
                color::autocolor(rt.term(), outcome(&j.status, &j.conclusion)),
                labels(&j.labels),
                j.run_attempt.to_string(),
                j.runner_name.clone(),
            ]);
        }
    })?;
    if jobs.iter().any(|j| is_queued(&j.status)) {
        for line in diagnose_waiting(api, slug, &jobs).await {
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
    let mut items: Vec<gitea_model::ActionArtifact> = match args.run {
        Some(run) => {
            let q = query::GetArtifactsOfRunQuery { name: args.name.clone() };
            api.run().artifacts(&slug.owner, &slug.name, int(run.get())?, &q).await?.artifacts
        }
        None => {
            let q = query::GetArtifactsQuery { name: args.name.clone() };
            api.artifact().list(&slug.owner, &slug.name, &q).await?.artifacts
        }
    };
    items.truncate(limit);

    let listing = Listing {
        fields: ARTIFACT_FIELDS,
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
                a.workflow_run.as_ref().map(|r| r.id.to_string()).unwrap_or_default(),
                if a.expired { "yes".to_owned() } else { String::new() },
                support::ago(a.created_at.as_ref()),
            ]);
        }
    })
}

// ---------------------------------------------------------------------------------- runners

async fn runners(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &RunnersArgs) -> Result<()> {
    let limit = support::limit(None, globals);
    let mut items: Vec<ActionRunner> = if args.admin {
        let q = query::GetAdminRunnersQuery::default();
        api.admin().get_admin_runners(&q).await?.runners
    } else if args.user {
        let q = query::GetUserRunnersQuery::default();
        api.user().get_user_runners(&q).await?.runners
    } else if let Some(org) = &args.org {
        let q = query::GetOrgRunnersQuery::default();
        api.org().get_org_runners(org, &q).await?.runners
    } else {
        let slug = slug(rt, globals)?;
        let q = query::GetRepoRunnersQuery::default();
        api.repo().get_repo_runners(&slug.owner, &slug.name, &q).await?.runners
    };
    items.truncate(limit);

    let listing = Listing {
        fields: RUNNER_FIELDS,
        value: serde_json::to_value(&items).map_err(encode_failed)?,
        count: items.len(),
        total: None,
        noun: "runners",
    };
    emit::list(rt, globals, listing, |t| {
        t.headers(["ID", "NAME", "STATUS", "LABELS", "BUSY", "DISABLED"]);
        for r in &items {
            t.row([
                r.id.to_string(),
                r.name.clone(),
                color::autocolor(rt.term(), &r.status),
                labels(&runner_labels(r)),
                yes(r.busy),
                yes(r.disabled),
            ]);
        }
    })?;
    if items.is_empty() && !args.admin && !args.user && args.org.is_none() {
        support::note(
            rt.term(),
            "no runner is registered against this repository. Gitea lists organization and \
             instance runners separately — try `gea run runners --org <owner>` or `--admin` — and \
             if none of those carry the labels your workflows' `runs-on:` values name, jobs will \
             wait forever. Register one with `act_runner register`.",
        );
    }
    Ok(())
}

fn yes(b: bool) -> String {
    if b { "yes".to_owned() } else { String::new() }
}

/// A runner's label names. Gitea sends them as objects (`{id, name, type}`), but only the name is
/// what `runs-on:` matches against.
fn runner_labels(r: &ActionRunner) -> Vec<String> {
    r.labels.iter().map(|l| l.name.clone()).collect()
}

// -------------------------------------------------------------------------------- diagnosis

/// Every runner this token can see that could serve `slug`: the repository's own, the owning
/// organization's, and the instance's. See the module docs for why this takes three calls.
///
/// Each scope is best-effort. A diagnosis is a courtesy: a token that cannot list organization
/// or instance runners still gets its real output, plus a line saying what was not looked at.
async fn candidate_runners(api: &Api, slug: &RepoSlug) -> (Vec<ActionRunner>, Vec<&'static str>) {
    // All bound up front: each is borrowed by a future the `join!` below holds across awaits.
    let (repos, orgs, admin) = (api.repo(), api.org(), api.admin());
    let (rq, oq, aq) = (
        query::GetRepoRunnersQuery::default(),
        query::GetOrgRunnersQuery::default(),
        query::GetAdminRunnersQuery::default(),
    );
    let (repo, org, admin) = futures::join!(
        repos.get_repo_runners(&slug.owner, &slug.name, &rq),
        orgs.get_org_runners(&slug.owner, &oq),
        admin.get_admin_runners(&aq),
    );
    let mut runners: Vec<ActionRunner> = Vec::new();
    let mut unseen = Vec::new();
    for (scope, got) in [("repository", repo), ("organization", org), ("instance", admin)] {
        match got {
            Ok(r) => {
                for runner in r.runners {
                    // The admin listing repeats every repository and organization runner.
                    if !runners.iter().any(|x| x.id == runner.id) {
                        runners.push(runner);
                    }
                }
            }
            Err(_) => unseen.push(scope),
        }
    }
    (runners, unseen)
}

/// Explain, in words, why each waiting job is waiting.
///
/// This is the command group's reason to exist. Gitea will not tell you that a job's
/// `runs-on:` label matches no runner — the job simply sits in `queued` with no error, no
/// warning, and a green API response. So: fetch the runners that could serve this repository and
/// compare label sets.
///
/// A runner can take a job only if it carries **every** label the job asks for, which is why the
/// check is a superset test rather than an intersection.
async fn diagnose_waiting(api: &Api, slug: &RepoSlug, jobs: &[ActionWorkflowJob]) -> Vec<String> {
    let (runners, unseen) = candidate_runners(api, slug).await;
    let mut lines: Vec<String> =
        jobs.iter().filter(|j| is_queued(&j.status)).map(|j| explain_job(j, &runners)).collect();
    // Only worth saying when nothing matched: if a runner could take the job, what we could not
    // see does not change the answer.
    if !unseen.is_empty() && lines.iter().any(|l| l.contains("no runner matches")) {
        lines.push(format!(
            "(this token cannot list {} runners, so a matching runner registered there would not \
             have been seen)",
            unseen.join(" or ")
        ));
    }
    lines
}

fn explain_job(job: &ActionWorkflowJob, runners: &[ActionRunner]) -> String {
    let wanted = &job.labels;
    if wanted.is_empty() {
        return format!("job {} is {} and declares no runs-on labels", job.name, job.status);
    }
    let matching: Vec<&ActionRunner> = runners
        .iter()
        .filter(|r| !r.disabled)
        .filter(|r| wanted.iter().all(|l| r.labels.iter().any(|rl| &rl.name == l)))
        .collect();

    if matching.is_empty() {
        let offered: Vec<String> = runners
            .iter()
            .map(|r| format!("{} [{}]", r.name, runner_labels(r).join(",")))
            .collect();
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
    if matching.iter().all(|r| r.status == "offline") {
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
async fn warn_if_actions_disabled(rt: &Runtime, api: &Api, slug: &RepoSlug) {
    let Ok(repo) = api.repo().get(&slug.owner, &slug.name).await else { return };
    if !repo.has_actions {
        support::note(
            rt.term(),
            &format!(
                "Actions is disabled for {slug}. Enable it in the repository's settings, or with \
                 `gea repo edit --enable-actions`."
            ),
        );
    }
}

// ----------------------------------------------------------------------------------- helpers

/// The run or job has reached an end state. Gitea says so with `status: completed`; the
/// conclusion then says which end.
fn is_finished(status: &str) -> bool {
    status == "completed"
}

/// Statuses that mean "nothing has picked this up yet" — the state a missing runner label
/// produces, and the one this module goes out of its way to explain. `queued` is Gitea's name
/// for a job waiting on a runner; `waiting` is one blocked on something else (an approval, a
/// `needs:`), which is still worth a diagnosis because it is still not moving.
fn is_queued(status: &str) -> bool {
    matches!(status, "queued" | "waiting" | "pending")
}

/// What to print in a STATUS column: how it ended when it has ended, where it is otherwise.
fn outcome<'a>(status: &'a str, conclusion: &'a str) -> &'a str {
    if conclusion.is_empty() { status } else { conclusion }
}

/// `ci.yml` from the run's `path`, which Gitea spells `<workflow file>@<ref>`.
fn workflow_file(path: &str) -> &str {
    path.split_once('@').map_or(path, |(file, _)| file)
}

/// Labels as `a, b`, or a dash when there are none. A dash rather than an empty cell because in
/// this table an empty `RUNS-ON` is itself the bug being looked for.
fn labels(labels: &[String]) -> String {
    if labels.is_empty() { "-".to_owned() } else { labels.join(", ") }
}

/// Wall-clock time between two timestamps, or nothing when either is unset — which is what a run
/// that has not started, or not finished, reports.
fn duration(
    started: Option<&gitea_core::types::Timestamp>,
    completed: Option<&gitea_core::types::Timestamp>,
) -> String {
    let (Some(s), Some(c)) = (started, completed) else { return String::new() };
    if s.is_unset() || c.is_unset() {
        return String::new();
    }
    format_seconds(c.as_jiff().as_second() - s.as_jiff().as_second())
}

fn format_seconds(seconds: i64) -> String {
    if seconds <= 0 {
        return String::new();
    }
    let (m, s) = (seconds / 60, seconds % 60);
    if m == 0 { format!("{s}s") } else { format!("{m}m{s:02}s") }
}

/// Bytes as `1.4 MiB`. Binary units, because that is what Gitea's own UI shows.
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
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_model::ActionRunnerLabel;
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
            labels: labels
                .iter()
                .map(|s| ActionRunnerLabel { name: (*s).to_owned(), ..Default::default() })
                .collect(),
            status: status.into(),
            ..Default::default()
        }
    }

    fn job(name: &str, runs_on: &[&str], status: &str) -> ActionWorkflowJob {
        ActionWorkflowJob {
            name: name.to_owned(),
            labels: runs_on.iter().map(|s| (*s).to_owned()).collect(),
            status: status.to_owned(),
            ..Default::default()
        }
    }

    fn no_filters() -> ListArgs {
        ListArgs {
            workflow: None,
            status: None,
            event: None,
            branch: None,
            commit: None,
            user: None,
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
            Canned::json(200, r#"{"total_count":1,"workflow_runs":[{"id":9,"status":"queued"}]}"#),
        ));
        let args = ListArgs {
            status: Some("queued".into()),
            event: Some("push".into()),
            branch: Some("main".into()),
            commit: Some("deadbeef".into()),
            user: Some("alice".into()),
            ..no_filters()
        };
        let out =
            fetch_runs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &args, 30).await.unwrap();
        assert_eq!(out.workflow_runs.len(), 1);

        let q = fake.calls()[0].query.clone();
        let has = |pair: &str| q.split('&').any(|p| p == pair);
        assert!(has("status=queued"), "{q}");
        assert!(has("event=push"), "{q}");
        assert!(has("branch=main"), "{q}");
        assert!(has("head_sha=deadbeef"), "{q}");
        assert!(has("actor=alice"), "{q}");
        assert!(has("limit=30"), "{q}");
    }

    /// Bug this prevents: `--workflow` being sent as a query parameter the repository-wide route
    /// does not have, which Gitea ignores — so the user reads every workflow's runs believing they
    /// are one workflow's.
    #[tokio::test]
    async fn a_workflow_filter_uses_the_per_workflow_route() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/workflows/ci.yml/runs",
            Canned::json(200, r#"{"total_count":0,"workflow_runs":[]}"#),
        ));
        let args = ListArgs { workflow: Some("ci.yml".into()), ..no_filters() };
        fetch_runs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &args, 30).await.unwrap();
        assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
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
        let out =
            fetch_runs(&api_for(fake), &RepoSlug::new("o", "r"), &no_filters(), 2).await.unwrap();
        assert_eq!(out.workflow_runs.len(), 2);
        // The banner still needs the real size of the collection.
        assert_eq!(out.total_count, 9);
    }

    /// The whole point of the group. Bug this prevents: a run that waits forever because no
    /// runner carries its label, with `gea` reporting nothing but `queued`.
    #[test]
    fn a_job_whose_label_no_runner_offers_is_explained_as_waiting_forever() {
        let line = explain_job(
            &job("build", &["ubuntu-latest"], "queued"),
            &[runner("shell", &["shell", "docker"], "online")],
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
        let runners = [runner("x86", &["docker"], "online")];
        let line = explain_job(&job("build", &["docker", "arm64"], "queued"), &runners);
        assert!(line.contains("no runner matches all labels"), "{line}");

        let line = explain_job(&job("build", &["docker"], "queued"), &runners);
        assert!(line.contains("could take it"), "{line}");
    }

    /// A disabled runner takes no jobs, so it must not be offered as the one that "could take it".
    #[test]
    fn a_disabled_runner_is_not_a_match() {
        let mut r = runner("x86", &["docker"], "online");
        r.disabled = true;
        let line = explain_job(&job("build", &["docker"], "queued"), &[r]);
        assert!(line.contains("no runner matches all labels"), "{line}");
    }

    /// An offline runner is a different problem from a missing label, and the remedy is
    /// different too ("start it" versus "register one"), so the message distinguishes them.
    #[test]
    fn an_offline_runner_is_reported_as_offline() {
        let line = explain_job(
            &job("build", &["docker"], "queued"),
            &[runner("nightly", &["docker"], "offline")],
        );
        assert!(line.contains("offline"), "{line}");
        assert!(line.contains("nightly"), "{line}");
    }

    /// Bug this prevents: diagnosing against the repository's runners only, which Gitea never
    /// fills with the organization's or the instance's — so every job served by an org runner
    /// would be reported as waiting forever.
    #[tokio::test]
    async fn the_diagnosis_merges_every_readable_scope_and_names_the_unreadable_ones() {
        let get = || "GET".parse().unwrap();
        let fake = Arc::new(
            FakeTransport::new()
                .on(get(), "/api/v1/repos/o/r/actions/runners", Canned::json(200, r#"{"runners":[]}"#))
                .on(
                    get(),
                    "/api/v1/orgs/o/actions/runners",
                    Canned::json(
                        200,
                        r#"{"runners":[{"id":3,"name":"org-box","status":"online","labels":[{"name":"docker"}]}]}"#,
                    ),
                )
                .on(get(), "/api/v1/admin/actions/runners", Canned::json(403, r#"{"message":"no"}"#)),
        );
        let api = api_for(fake);
        let slug = RepoSlug::new("o", "r");
        let lines = diagnose_waiting(&api, &slug, &[job("build", &["docker"], "queued")]).await;
        assert!(lines[0].contains("org-box could take it"), "{lines:?}");
        // A match was found, so what could not be read is irrelevant and not mentioned.
        assert_eq!(lines.len(), 1, "{lines:?}");

        let lines = diagnose_waiting(&api, &slug, &[job("build", &["arm64"], "queued")]).await;
        assert!(lines.iter().any(|l| l.contains("cannot list instance runners")), "{lines:?}");
    }

    /// `--exit-status` must be non-zero for a failed run and zero for a successful one, and must
    /// not claim a still-running run failed.
    #[test]
    fn exit_status_reflects_the_conclusion() {
        let failed = ActionWorkflowRun {
            status: "completed".into(),
            conclusion: "failure".into(),
            ..Default::default()
        };
        let err = exit_status(true, &failed).unwrap_err();
        assert_ne!(err.exit_code(), 0);
        assert!(err.to_string().contains("failure"), "{err}");
        // Without the flag, a failed run is still a successful *command*.
        assert!(exit_status(false, &failed).is_ok());

        let ok = ActionWorkflowRun {
            status: "completed".into(),
            conclusion: "success".into(),
            ..Default::default()
        };
        assert!(exit_status(true, &ok).is_ok());

        let running = ActionWorkflowRun { status: "in_progress".into(), ..Default::default() };
        let err = exit_status(true, &running).unwrap_err();
        assert!(err.to_string().contains("has not finished"), "{err}");
    }

    /// Bug this prevents: `run watch --exit-status` returning zero for a failed run, which turns
    /// a red pipeline into a green CI step.
    #[tokio::test]
    async fn watch_returns_non_zero_for_a_failed_run() {
        let fake = Arc::new(FakeTransport::new().on(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/runs/7",
            Canned::json(
                200,
                r#"{"id":7,"status":"completed","conclusion":"failure","path":"ci.yml@refs/heads/main"}"#,
            ),
        ));
        let api = api_for(fake.clone());
        let run = api.run().view("o", "r", 7).await.unwrap();
        assert!(is_finished(&run.status));
        let err = exit_status(true, &run).unwrap_err();
        assert_ne!(err.exit_code(), 0);
        // The file, not `ci.yml@refs/heads/main`.
        assert!(err.to_string().contains("(ci.yml)"), "{err}");
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
                .try_get_matches_from(["gea", "watch", "1", "--interval", "0"])
                .is_err()
        );
        let m = Harness::try_parse_from(["gea", "watch", "1"]).unwrap();
        let Cmd::Watch(w) = m.cmd else { panic!("watch") };
        assert_eq!(w.interval, 3);
    }

    /// Bug this prevents: `run view` growing a second round trip for the jobs it was always going
    /// to ask for.
    #[tokio::test]
    async fn view_asks_for_the_run_and_its_jobs_exactly_once_each() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/runs/7",
                    Canned::json(200, r#"{"id":7,"display_title":"CI"}"#),
                )
                .on(
                    "GET".parse().unwrap(),
                    "/api/v1/repos/o/r/actions/runs/7/jobs",
                    Canned::json(200, r#"{"total_count":1,"jobs":[{"id":11,"name":"build"}]}"#),
                ),
        );
        let (run, jobs) =
            fetch_view(&api_for(fake.clone()), &RepoSlug::new("o", "r"), RunId::new(7))
                .await
                .unwrap();
        assert_eq!(run.id.get(), 7);
        assert_eq!(jobs.len(), 1);
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
    }

    /// Bug this prevents: a matrix build bigger than one page losing its later jobs — and with
    /// them, `run logs --failed` reporting that nothing failed.
    #[tokio::test]
    async fn every_page_of_jobs_is_read() {
        let fake = Arc::new(FakeTransport::new().on_sequence(
            "GET".parse().unwrap(),
            "/api/v1/repos/o/r/actions/runs/7/jobs",
            vec![
                Canned::json(200, r#"{"total_count":2,"jobs":[{"id":1}]}"#),
                Canned::json(200, r#"{"total_count":2,"jobs":[{"id":2}]}"#),
            ],
        ));
        let jobs =
            fetch_jobs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), RunId::new(7)).await;
        let jobs = jobs.unwrap();
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
        assert!(fake.calls()[1].query.contains("page=2"), "{:?}", fake.calls());
        assert_eq!(jobs.len(), 2);
    }

    /// Bug this prevents: a twelve-job matrix costing twelve serial round trips, each log then
    /// landing under a *different* job's `==>` header.
    #[tokio::test]
    async fn every_jobs_log_is_fetched_once_as_text_and_stays_in_job_order() {
        let get = || "GET".parse().unwrap();
        let fake = Arc::new(
            FakeTransport::new()
                .on(get(), "/api/v1/repos/o/r/actions/jobs/11/logs", Canned::text(200, "first\n"))
                .on(get(), "/api/v1/repos/o/r/actions/jobs/12/logs", Canned::text(200, "second\n"))
                .on(get(), "/api/v1/repos/o/r/actions/jobs/13/logs", Canned::text(200, "third\n")),
        );
        let jobs = [
            ActionWorkflowJob { id: JobId::new(11), name: "build".into(), ..Default::default() },
            ActionWorkflowJob { id: JobId::new(12), name: "test".into(), ..Default::default() },
            ActionWorkflowJob { id: JobId::new(13), name: "lint".into(), ..Default::default() },
        ];
        let wanted: Vec<&ActionWorkflowJob> = jobs.iter().collect();
        let logs =
            fetch_logs(&api_for(fake.clone()), &RepoSlug::new("o", "r"), &wanted).await.unwrap();
        assert_eq!(logs, ["first\n", "second\n", "third\n"]);
        assert_eq!(fake.call_count(), wanted.len(), "{:?}", fake.calls());
    }

    #[test]
    fn statuses_read_as_their_conclusion_once_there_is_one() {
        assert_eq!(outcome("completed", "failure"), "failure");
        assert_eq!(outcome("queued", ""), "queued");
        assert_eq!(workflow_file("ci.yml@refs/heads/main"), "ci.yml");
        assert_eq!(workflow_file("ci.yml"), "ci.yml");
        assert!(is_queued("queued") && is_queued("waiting") && !is_queued("in_progress"));
    }

    #[test]
    fn sizes_and_durations_are_human_readable() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(format_seconds(0), "");
        assert_eq!(format_seconds(9), "9s");
        assert_eq!(format_seconds(125), "2m05s");
        assert_eq!(duration(None, None), "");
    }

    /// Snapshot of both output modes for the same data, so a column reorder or a TSV regression
    /// shows up as a diff rather than as a broken pipeline in someone's script.
    #[test]
    fn run_list_output_goldens() {
        let runs: Vec<ActionWorkflowRun> = vec![
            ActionWorkflowRun {
                id: RunId::new(9),
                run_number: 2,
                status: "queued".into(),
                display_title: "add the emitter".into(),
                path: "ci.yml@refs/heads/main".into(),
                head_branch: "main".into(),
                event: "push".into(),
                ..Default::default()
            },
            ActionWorkflowRun {
                id: RunId::new(8),
                run_number: 1,
                status: "completed".into(),
                conclusion: "failure".into(),
                display_title: "fix the table".into(),
                path: "release.yml@refs/tags/v1.2.0".into(),
                head_branch: String::new(),
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
                GlobalOpts { json: Some("id,status,conclusion".into()), ..Default::default() },
            ),
        ] {
            let listing = Listing {
                fields: RUN_FIELDS,
                value: serde_json::to_value(&runs).unwrap(),
                count: runs.len(),
                total: Some(7),
                noun: "runs",
            };
            let mut buf = Vec::new();
            emit::list_to(&mut buf, &term, &globals, listing, |t| {
                t.headers([
                    "STATUS", "TITLE", "WORKFLOW", "BRANCH", "EVENT", "ID", "NUMBER", "AGE",
                ]);
                for r in &runs {
                    t.row([
                        outcome(&r.status, &r.conclusion).to_owned(),
                        r.display_title.clone(),
                        workflow_file(&r.path).to_owned(),
                        r.head_branch.clone(),
                        r.event.clone(),
                        r.id.to_string(),
                        r.run_number.to_string(),
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

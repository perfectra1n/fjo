//! `fjo ci logs` — the logs of a target's failed jobs, across every run of the commit.
//!
//! The command an agent reaches for after `fjo ci` says something is red, so its default is the
//! answer to "why": only the jobs that failed, each under a header naming its workflow, run and
//! job. `--job` matches a name across *every* run's jobs, and because a push typically starts
//! several workflows that each have a `build`, an ambiguous name is refused with the candidates
//! labelled by workflow rather than guessed.

use forgejo_client::Api;
use forgejo_core::error::Result;
use forgejo_core::types::RepoSlug;
use forgejo_model::{ActionRun, ActionRunJob};
use futures::{StreamExt, TryStreamExt};

use super::target::{self, CiState, Resolved, ci_state};
use super::view::{self, RunView, job_failed};
use super::{LogsArgs, no_runs, select, sha7};
use crate::cmd::run::{LOG_CONCURRENCY, concat_logs, fetch_logs};
use crate::cmd::run_ref::{JobRef, pick_job};
use crate::cmd::support;
use crate::cmd::support::listing as emit;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// A job, with the run it belongs to.
pub(crate) type Picked<'a> = (&'a ActionRun, ActionRunJob);

pub(super) async fn run(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &LogsArgs,
) -> Result<()> {
    let resolved = target::resolve_target(rt, globals, api, args.target.as_deref()).await?;
    let runs = target::runs_for_sha(api, &resolved.slug, &resolved.sha).await?;
    let selected = select(&runs, false, args.workflow.as_deref());
    if selected.is_empty() {
        no_runs(rt, api, &resolved, &runs, args.workflow.as_deref()).await;
        return Ok(());
    }
    let picked = pick(api, &resolved.slug, &selected, args.all, args.job.as_ref()).await?;
    if picked.is_empty() {
        support::warn(&nothing_to_print(&resolved, &selected, args.all));
        return view::verdict(&resolved, &bare(&selected));
    }
    let wanted: Vec<&ActionRunJob> = picked.iter().map(|(_, j)| j).collect();
    let logs = fetch_logs(api, &resolved.slug, &wanted).await?;
    emit::text(rt, globals, &assemble(&picked, &logs))
}

fn bare<'a>(selected: &[&'a ActionRun]) -> Vec<RunView<'a>> {
    selected.iter().map(|r| RunView::bare(r)).collect()
}

/// Every log under its header: `==> ci.yml / build (run 6442, job 11) <==`.
///
/// Always headed, even for one job: the header is the only place the output says which
/// workflow and run the log came from, and with several workflows per push that is not obvious.
pub(crate) fn assemble(picked: &[Picked<'_>], logs: &[String]) -> String {
    concat_logs(picked.iter().zip(logs).map(|((run, job), text)| {
        let header =
            format!("==> {} / {} (run {}, job {}) <==", run.workflow_id, job.name, run.id, job.id);
        (Some(header), text.as_str())
    }))
}

/// The jobs whose logs to print, in run order then job order.
///
/// Jobs are fetched for the failed runs only by default — a green run cannot hold a failed job —
/// and for every selected run with `--all` or `--job`, whose answer can be anywhere.
pub(crate) async fn pick<'a>(
    api: &Api,
    slug: &RepoSlug,
    selected: &[&'a ActionRun],
    all: bool,
    job: Option<&JobRef>,
) -> Result<Vec<Picked<'a>>> {
    let scope: Vec<&ActionRun> = if all || job.is_some() {
        selected.to_vec()
    } else {
        selected.iter().copied().filter(|r| ci_state(r) == CiState::Failed).collect()
    };
    // Bound outside the closure: a borrow of `api`, and a temporary of it would be dropped
    // before the futures the stream holds are polled.
    let runs = api.run();
    let jobs: Vec<Vec<ActionRunJob>> =
        futures::stream::iter(scope.iter().map(|r| runs.jobs(&slug.owner, &slug.name, r.id.get())))
            .buffered(LOG_CONCURRENCY)
            .try_collect()
            .await?;
    let mut pairs: Vec<Picked<'a>> = scope
        .into_iter()
        .zip(jobs)
        .flat_map(|(run, jobs)| jobs.into_iter().map(move |j| (run, j)))
        .collect();
    match job {
        Some(r) => Ok(vec![pick_job(&pairs, job_of, describe, r)?.clone()]),
        None if all => Ok(pairs),
        None => {
            pairs.retain(|(_, j)| job_failed(j));
            Ok(pairs)
        }
    }
}

fn job_of<'x>(p: &'x Picked<'_>) -> &'x ActionRunJob {
    &p.1
}

/// A `--job` candidate, labelled with its workflow so two `build` jobs can be told apart.
fn describe(p: &Picked<'_>) -> String {
    format!("{} / {} (id {})", p.0.workflow_id, p.1.name, p.1.id)
}

/// Why there is nothing to print. The exit code is [`view::verdict`]'s, so a run that failed
/// without a failed job (cancelled before any job ran) still exits 1, and a pending one 8.
fn nothing_to_print(resolved: &Resolved, selected: &[&ActionRun], all: bool) -> String {
    let sha = sha7(&resolved.sha);
    let count = |s: CiState| selected.iter().filter(|r| ci_state(r) == s).count();
    if all {
        format!("the runs for {sha} have no jobs yet")
    } else if let Some(r) = selected.iter().find(|r| ci_state(r) == CiState::Failed) {
        format!(
            "{} (run {}) ended {} but no job in it failed; `fjo run view {}` shows why",
            r.workflow_id, r.id, r.status, r.id
        )
    } else if count(CiState::Pending) > 0 {
        format!(
            "nothing has failed for {sha} yet; {} run(s) still running",
            count(CiState::Pending)
        )
    } else {
        format!("no job failed for {sha}; --all prints every job")
    }
}

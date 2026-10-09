//! `fjo ci status`'s view: what is fetched for each run, how it is drawn, and the exit code.
//!
//! Kept free of [`crate::runtime::Runtime`] so every piece is testable on its own: [`gather`]
//! against a `FakeTransport`, [`render`] as a snapshot, [`verdict`] as an exit code.

use forgejo_client::Api;
use forgejo_core::error::{Error, ErrorKind, FailedRun, Result};
use forgejo_core::types::RepoSlug;
use forgejo_model::{ActionRun, ActionRunJob};
use futures::{StreamExt, TryStreamExt};

use super::sha7;
use super::target::{CiState, Resolved, ci_state};
use crate::cmd::run::{LOG_CONCURRENCY, diagnose_waiting, duration, is_queued};
use crate::output::Term;
use crate::output::color::{display_width, paint};

/// One selected run, with what the view prints under it.
#[derive(Debug, Clone)]
pub(crate) struct RunView<'a> {
    pub run: &'a ActionRun,
    /// The jobs that failed, when the run failed. Empty for any other run: their jobs are never
    /// fetched.
    pub failed_jobs: Vec<ActionRunJob>,
    /// `diagnose_waiting`'s explanation of each waiting job, when the run is pending.
    pub waiting: Vec<String>,
}

impl<'a> RunView<'a> {
    /// A run with nothing fetched for it.
    pub(crate) fn bare(run: &'a ActionRun) -> Self {
        Self { run, failed_jobs: Vec::new(), waiting: Vec::new() }
    }
}

/// A job that ended badly — the same two statuses that make a run [`CiState::Failed`].
pub(crate) fn job_failed(job: &ActionRunJob) -> bool {
    matches!(job.status.as_str(), "failure" | "cancelled")
}

/// Fetch what the view prints under each run, [`LOG_CONCURRENCY`] runs at a time and in
/// `selected` order.
///
/// Jobs are fetched **only** for a failed run (to name its failed jobs) and, when `diagnose`,
/// for a pending one (to explain a waiting job). A run that passed costs nothing: on a
/// four-workflow push that is the difference between one request and five.
pub(crate) async fn gather<'a>(
    api: &Api,
    slug: &RepoSlug,
    selected: &[&'a ActionRun],
    diagnose: bool,
) -> Result<Vec<RunView<'a>>> {
    futures::stream::iter(selected.iter().map(|run| detail(api, slug, run, diagnose)))
        .buffered(LOG_CONCURRENCY)
        .try_collect()
        .await
}

async fn detail<'a>(
    api: &Api,
    slug: &RepoSlug,
    run: &'a ActionRun,
    diagnose: bool,
) -> Result<RunView<'a>> {
    let mut view = RunView::bare(run);
    match ci_state(run) {
        CiState::Failed => {
            let jobs = api.run().jobs(&slug.owner, &slug.name, run.id.get()).await?;
            view.failed_jobs = jobs.into_iter().filter(job_failed).collect();
        }
        CiState::Pending if diagnose => {
            let jobs = api.run().jobs(&slug.owner, &slug.name, run.id.get()).await?;
            if jobs.iter().any(|j| is_queued(&j.status)) {
                view.waiting = diagnose_waiting(api, slug, &jobs).await?;
            }
        }
        _ => {}
    }
    Ok(view)
}

/// The verb of the next command to suggest: `logs` when something failed, `watch` while
/// something runs, nothing when all is well.
pub(crate) fn next_verb(views: &[RunView<'_>]) -> Option<&'static str> {
    let states = || views.iter().map(|v| ci_state(v.run));
    if states().any(|s| s == CiState::Failed) {
        Some("logs")
    } else if states().any(|s| s == CiState::Pending) {
        Some("watch")
    } else {
        None
    }
}

/// The exit code, through the taxonomy: a failed run is `CiFailed` (1), else an unfinished one
/// is `CiPending` (8), else success. `skipped` and unrecognised statuses block nothing.
pub(crate) fn verdict(resolved: &Resolved, views: &[RunView<'_>]) -> Result<()> {
    let failed: Vec<FailedRun> = views
        .iter()
        .filter(|v| ci_state(v.run) == CiState::Failed)
        .map(|v| {
            FailedRun::new(
                v.run.id.get(),
                v.run.workflow_id.clone(),
                v.run.status.clone(),
                v.failed_jobs.iter().map(|j| j.name.clone()).collect(),
                Some(v.run.html_url.clone()),
            )
        })
        .collect();
    let slug = Some(resolved.slug.to_string());
    let (target, sha) = (resolved.selector.clone(), resolved.sha.clone());
    if !failed.is_empty() {
        return Err(Error::new(ErrorKind::CiFailed { slug, target, sha, failed }));
    }
    let pending: Vec<String> = views
        .iter()
        .filter(|v| ci_state(v.run) == CiState::Pending)
        .map(|v| v.run.workflow_id.clone())
        .collect();
    if !pending.is_empty() {
        return Err(Error::new(ErrorKind::CiPending { slug, target, sha, pending }));
    }
    Ok(())
}

/// What a run is called in the view: its workflow file, plus the event when the same file
/// has runs for two events on this commit (`ci.yml (push)`, `ci.yml (pull_request)`) — two
/// identical names on two lines would read as a duplicate.
pub(crate) fn names(runs: &[&ActionRun]) -> Vec<String> {
    runs.iter()
        .map(|r| {
            let file = r.workflow_id.as_str();
            let twin =
                runs.iter().any(|o| o.id != r.id && o.workflow_id == file && o.event != r.event);
            if twin { format!("{file} ({})", r.event) } else { file.to_owned() }
        })
        .collect()
}

/// The mark and colour for a state.
fn mark(state: CiState) -> (&'static str, anstyle::Style) {
    use anstyle::AnsiColor::{Green, Red, Yellow};
    let fg = |c: anstyle::AnsiColor| anstyle::Style::new().fg_color(Some(c.into()));
    match state {
        CiState::Failed => ("✗", fg(Red)),
        CiState::Pending => ("●", fg(Yellow)),
        CiState::Passed => ("✓", fg(Green)),
        CiState::Other => ("-", anstyle::Style::new()),
    }
}

/// `text` padded to `width` visible columns, then painted — padding first, so an escape
/// sequence never counts towards the width.
fn cell(term: &Term, style: anstyle::Style, text: &str, width: usize) -> String {
    let pad = " ".repeat(width.saturating_sub(display_width(text)));
    format!("{}{pad}", paint(term, style, text))
}

/// The human view.
///
/// ```text
/// PR #2626 (renovate/zip-9.x) at f36d1a7
///
/// ✗ failure  ci.yml        run 6442  #3623  6m44s
///     ✗ Fast Checks        job 33898
/// ● waiting  review.yml    run 6443  #3624
///     waiting: job review requires runs-on …
///
/// next: fjo ci logs 2626
/// ```
///
/// Not a [`crate::output::Table`]: the job and diagnosis lines nest under their run, which a
/// table cannot express, and an agent reading a pipe needs the same nesting a person does — so
/// the layout is the same on a terminal and in a pipe, and only colour depends on `term`.
pub(crate) fn render(
    resolved: &Resolved,
    views: &[RunView<'_>],
    term: &Term,
    next: Option<&str>,
) -> String {
    let mut out = format!("{} at {}\n", resolved.label, sha7(&resolved.sha));
    if !views.is_empty() {
        out.push('\n');
        out.push_str(&render_runs(views, term));
    }
    if let Some(cmd) = next {
        out.push_str(&format!("\nnext: {cmd}\n"));
    }
    out
}

fn render_runs(views: &[RunView<'_>], term: &Term) -> String {
    let runs: Vec<&ActionRun> = views.iter().map(|v| v.run).collect();
    let names = names(&runs);
    let rows: Vec<[String; 5]> = views
        .iter()
        .zip(&names)
        .map(|(v, name)| {
            let r = v.run;
            [
                r.status.clone(),
                name.clone(),
                format!("run {}", r.id),
                format!("#{}", r.index_in_repo),
                duration(r.duration),
            ]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|row| display_width(&row[i])).max().unwrap_or(0);
    let widths = [width(0), width(1), width(2), width(3)];
    let job_width = views
        .iter()
        .flat_map(|v| v.failed_jobs.iter().map(|j| display_width(&j.name)))
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for (v, row) in views.iter().zip(&rows) {
        let (sym, style) = mark(ci_state(v.run));
        let line = format!(
            "{} {}  {}  {}  {}  {}",
            paint(term, style, sym),
            cell(term, style, &row[0], widths[0]),
            cell(term, anstyle::Style::new(), &row[1], widths[1]),
            cell(term, anstyle::Style::new(), &row[2], widths[2]),
            cell(term, anstyle::Style::new(), &row[3], widths[3]),
            row[4],
        );
        out.push_str(line.trim_end());
        out.push('\n');
        for job in &v.failed_jobs {
            let (sym, style) = mark(CiState::Failed);
            let name = cell(term, anstyle::Style::new(), &job.name, job_width);
            out.push_str(&format!("    {} {name}  job {}\n", paint(term, style, sym), job.id));
        }
        for why in &v.waiting {
            out.push_str(&format!("    waiting: {why}\n"));
        }
    }
    out
}

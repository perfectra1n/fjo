//! `gea ci status`'s view: what is fetched for each run, how it is drawn, and the exit code.
//!
//! Kept free of [`crate::runtime::Runtime`] so every piece is testable on its own: [`gather`]
//! against a `FakeTransport`, [`render`] as a snapshot, [`verdict`] as an exit code.

use futures::{StreamExt, TryStreamExt};
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, FailedRun, Result};
use gitea_core::types::RepoSlug;
use gitea_model::{ActionWorkflowJob, ActionWorkflowRun};

use super::target::{CiState, Resolved, ci_state};
use super::{select, sha7};
use crate::cmd::run::{
    LOG_CONCURRENCY, diagnose_waiting, duration, fetch_jobs, is_queued, outcome, workflow_file,
};
use crate::output::Term;
use crate::output::color::{display_width, paint};

/// One selected run, with what the view prints under it.
#[derive(Debug, Clone)]
pub(crate) struct RunView<'a> {
    pub run: &'a ActionWorkflowRun,
    /// The jobs that failed, when the run failed. Empty for any other run: their jobs are never
    /// fetched.
    pub failed_jobs: Vec<ActionWorkflowJob>,
    /// `diagnose_waiting`'s explanation of each waiting job, when the run is pending.
    pub waiting: Vec<String>,
    /// A newer run of the same workflow replaced this one; only `gea ci status --all` shows
    /// such a run. It is drawn with a `(superseded)` mark and counts towards neither
    /// [`next_verb`] nor [`verdict`], so a display flag cannot change the exit code.
    pub superseded: bool,
}

impl<'a> RunView<'a> {
    /// A run with nothing fetched for it.
    pub(crate) fn bare(run: &'a ActionWorkflowRun) -> Self {
        Self { run, failed_jobs: Vec::new(), waiting: Vec::new(), superseded: false }
    }
}

/// A job that ended badly — the same three words that make a run [`CiState::Failed`].
pub(crate) fn job_failed(job: &ActionWorkflowJob) -> bool {
    matches!(job.conclusion.as_str(), "failure" | "cancelled" | "timed_out")
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
    selected: &[&'a ActionWorkflowRun],
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
    run: &'a ActionWorkflowRun,
    diagnose: bool,
) -> Result<RunView<'a>> {
    let mut view = RunView::bare(run);
    match ci_state(run) {
        CiState::Failed => {
            let jobs = fetch_jobs(api, slug, run.id).await?;
            view.failed_jobs = jobs.into_iter().filter(job_failed).collect();
        }
        CiState::Pending if diagnose => {
            let jobs = fetch_jobs(api, slug, run.id).await?;
            if jobs.iter().any(|j| is_queued(&j.status)) {
                view.waiting = diagnose_waiting(api, slug, &jobs).await;
            }
        }
        _ => {}
    }
    Ok(view)
}

/// Flag each view whose run a newer run of the same workflow superseded: every one not in the
/// newest-per-workflow selection of `runs` (restricted to `workflow`, as the view is).
pub(crate) fn mark_superseded(
    views: &mut [RunView<'_>],
    runs: &[ActionWorkflowRun],
    workflow: Option<&str>,
) {
    let latest = select(runs, false, workflow);
    for v in views {
        v.superseded = !latest.iter().any(|r| r.id == v.run.id);
    }
}

/// The views that speak for the commit now: every one but the superseded.
fn current<'v, 'a>(views: &'v [RunView<'a>]) -> impl Iterator<Item = &'v RunView<'a>> {
    views.iter().filter(|v| !v.superseded)
}

/// The verb of the next command to suggest: `logs` when something failed, `watch` while
/// something runs, nothing when all is well. Superseded runs are not consulted.
pub(crate) fn next_verb(views: &[RunView<'_>]) -> Option<&'static str> {
    let states = || current(views).map(|v| ci_state(v.run));
    if states().any(|s| s == CiState::Failed) {
        Some("logs")
    } else if states().any(|s| s == CiState::Pending) {
        Some("watch")
    } else {
        None
    }
}

/// The exit code, through the taxonomy: a failed run is `CiFailed` (1), else an unfinished one
/// is `CiPending` (8), else success. `skipped` and unrecognised conclusions block nothing, and
/// neither does a superseded run.
///
/// `slug` is the repository the advice must name with `-R`, `None` when the advice needs none:
/// [`super::advice_slug`], the same rule the `next:` line follows.
pub(crate) fn verdict(
    resolved: &Resolved,
    views: &[RunView<'_>],
    slug: Option<String>,
) -> Result<()> {
    let failed: Vec<FailedRun> = current(views)
        .filter(|v| ci_state(v.run) == CiState::Failed)
        .map(|v| {
            FailedRun::new(
                v.run.id.get(),
                workflow_file(&v.run.path),
                v.run.conclusion.clone(),
                v.failed_jobs.iter().map(|j| j.name.clone()).collect(),
                Some(v.run.html_url.clone()),
            )
        })
        .collect();
    let (target, sha) = (resolved.selector.clone(), resolved.sha.clone());
    if !failed.is_empty() {
        return Err(Error::new(ErrorKind::CiFailed { slug, target, sha, failed }));
    }
    let pending: Vec<String> = current(views)
        .filter(|v| ci_state(v.run) == CiState::Pending)
        .map(|v| workflow_file(&v.run.path).to_owned())
        .collect();
    if !pending.is_empty() {
        return Err(Error::new(ErrorKind::CiPending { slug, target, sha, pending }));
    }
    Ok(())
}

/// What a run is called in the view: its workflow file, qualified just enough that two
/// different workflows on this commit never read as one — two identical names on two lines
/// would read as a duplicate, and `watch` would print one's transition under the other's name.
///
/// * The same file for two events: the event, `ci.yml (push)` and `ci.yml (pull_request)`.
/// * The same file and event for two refs (`ci.yml@refs/heads/main` and `ci.yml@refs/tags/v1`,
///   which [`super::target::latest_per_workflow`] keeps apart): the event *and* the short ref,
///   `ci.yml (push, main)` and `ci.yml (push, v1)`.
///
/// Runs of the same file, event and ref — a superseded run next to its replacement under
/// `--all` — share a name; they are the same workflow, and the view marks the older one.
pub(crate) fn names(runs: &[&ActionWorkflowRun]) -> Vec<String> {
    runs.iter()
        .map(|r| {
            let file = workflow_file(&r.path);
            let same_file = || runs.iter().filter(|o| workflow_file(&o.path) == file);
            if same_file().any(|o| o.event == r.event && o.path != r.path) {
                format!("{file} ({}, {})", r.event, short_ref(&r.path))
            } else if same_file().any(|o| o.event != r.event) {
                format!("{file} ({})", r.event)
            } else {
                file.to_owned()
            }
        })
        .collect()
}

/// The ref part of a run's `path` (`ci.yml@refs/heads/main`), as a person would say it: `main`,
/// `v1`, `pull/2626/merge`. Empty when the path carries no ref.
fn short_ref(path: &str) -> &str {
    let full = path.split_once('@').map_or("", |(_, r)| r);
    ["refs/heads/", "refs/tags/", "refs/"].iter().find_map(|p| full.strip_prefix(p)).unwrap_or(full)
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
/// ● queued   review.yml    run 6443  #3624
///     waiting: job review requires runs-on …
///
/// next: gea ci logs 2626
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
    let runs: Vec<&ActionWorkflowRun> = views.iter().map(|v| v.run).collect();
    let names = names(&runs);
    let rows: Vec<[String; 5]> = views
        .iter()
        .zip(&names)
        .map(|(v, name)| {
            let r = v.run;
            [
                outcome(&r.status, &r.conclusion).to_owned(),
                if v.superseded { format!("{name} (superseded)") } else { name.clone() },
                format!("run {}", r.id),
                format!("#{}", r.run_number),
                duration(r.started_at.as_ref(), r.completed_at.as_ref()),
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

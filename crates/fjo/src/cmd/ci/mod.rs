//! `fjo ci` — CI status, logs and watching for a pull request, branch or commit.
//!
//! The *context* layer over `fjo run`: where `fjo run` addresses one run precisely, `fjo ci`
//! answers "what is CI doing for this pull request / branch / commit / where I am" and goes from
//! there to logs and watch without the user ever typing an id. The commit SHA is the join key
//! throughout — a target resolves to a SHA, and runs are listed with the server-side `head_sha`
//! filter, which (unlike `--ref`) finds `pull_request` runs, which Forgejo files under the pull
//! request rather than the branch. [`target`] is that resolution.
//!
//! # No `fjo ci rerun`
//!
//! The `gea` original has one; Forgejo's API has no route to rerun a run or a job (measured on
//! 16.0.5: `POST …/actions/runs/{id}/rerun` is a router 404), and `fjo run` has no `rerun` for
//! the same reason. Rerunning is a web-UI action on Forgejo, so a failed `fjo ci` advises
//! `fjo run view <id> --web`, which opens the page that has the button.
//!
//! # Built for a reader who is a script as often as a person
//!
//! * **No pickers.** An ambiguous `--job` is refused with every candidate listed, labelled with
//!   its workflow, never guessed and never prompted for.
//! * **The exit code is the verdict.** `fjo ci` exits 1 (`ErrorKind::CiFailed`) when a selected
//!   run failed, 8 (`ErrorKind::CiPending`) while one is unfinished, 0 otherwise — under
//!   `--json` too, as `fjo pr checks` does — so `case $?` is all a script needs.
//! * **Every human view ends with the next command**, on stdout so an agent reading the output
//!   sees it: `next: fjo ci logs 2626` when something failed, `next: fjo ci watch 2626` while
//!   something is running.
//!
//! # Superseded runs are hidden
//!
//! One commit collects several runs of one workflow: a re-push, a manual rerun, a cancelled
//! duplicate. Only the newest says anything about the commit now, so every subcommand works on
//! [`target::latest_per_workflow`]'s selection; `fjo ci status --all` shows the rest.
//!
//! See `docs/superpowers/specs/2026-10-09-ci-porcelain-design.md`, including its section on
//! where Forgejo diverges from the Gitea port this was translated from.

mod logs;
pub(crate) mod target;
mod view;
mod watch;

use std::io::Write;

use clap::{Args as ClapArgs, Subcommand};
use forgejo_client::Api;
use forgejo_core::error::Result;
use forgejo_model::ActionRun;

use crate::cmd::run::{RUN_FIELDS, warn_if_actions_disabled};
use crate::cmd::run_ref::JobRef;
use crate::cmd::support;
use crate::cmd::support::listing::{self as emit, Fields, Listing};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

use target::{Resolved, latest_per_workflow, sort_for_display};

/// `fjo ci --help`. Set on the `Porcelain::Ci` variant, because a variant's doc comment
/// overrides a `long_about` on the struct it wraps.
pub const LONG_ABOUT: &str = "\
CI status and logs for a pull request, branch or commit.

TARGET is one of:
  (nothing)        the branch you are on, as the server has it
  42, #42, a URL   a pull request
  f36d1a7          a commit (7-40 hex characters, at least one a letter)
  anything else    a branch; its open pull request when it has one

Every target becomes one commit, and fjo ci shows the Actions runs for that commit,
including pull_request runs, which Forgejo files under the pull request, not the branch. Only the newest run of
each workflow is shown; a run it superseded is hidden (`fjo ci status --all` shows it).

`fjo ci [TARGET]` is `fjo ci status [TARGET]`. A branch named like a subcommand needs
the explicit form: `fjo ci status logs`.

Exit codes: 0 when every run passed (skipped counts as passed), 1 when a run failed,
8 while a run has not finished. The same under --json.

  fjo ci                     # CI for the branch you are on
  fjo ci 2626                # CI for pull request #2626
  fjo ci logs 2626           # the logs of its failed jobs
  fjo ci watch && fjo pr merge
  fjo ci --json id,status,workflow_id";

#[derive(Debug, ClapArgs)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Cmd>,
    // Bare `fjo ci [TARGET]` is `fjo ci status [TARGET]`.
    #[command(flatten)]
    pub status: StatusArgs,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Show the CI runs for a target, failed jobs first (the default)
    #[command(long_about = "\
Show the CI runs for a target: each workflow's newest run, failed first, with the jobs
that failed under it and why a waiting job is waiting. Ends with the next command to run.

Exit codes: 0 passed, 1 failed, 8 still running. The same under --json.

  fjo ci status
  fjo ci status 2626 --workflow ci.yml
  fjo ci status main --all --json id,status,workflow_id")]
    Status(StatusArgs),
    /// Print the logs of the failed jobs
    #[command(long_about = "\
Print job logs for a target's runs, each under a `==> workflow / job (run, job) <==`
header. By default only the jobs that failed; --all prints every job, --job one.

When nothing failed: exit 8 if something is still running, else 0.

  fjo ci logs
  fjo ci logs 2626 --job \"Fast Checks\"
  fjo ci logs main --all --workflow ci.yml")]
    Logs(LogsArgs),
    /// Wait for a target's CI to finish, then show it
    #[command(long_about = "\
Poll a target's runs until every one has finished, printing each status change to
stderr, then print `fjo ci status` and exit as it does (--exit-status is implied: 0
passed, 1 failed). When no run exists yet, keep polling up to --appear-timeout.

A run that has failed is finished even while its other jobs still run: nothing they do
can make it pass, so the verdict is reported at once.

Ctrl-C stops it at once.

  fjo ci watch
  fjo ci watch 2626 --interval 10
  fjo ci watch && fjo pr merge")]
    Watch(WatchArgs),
}

// No doc comment on purpose: it would become the `about` of whatever flattens this struct, and
// bare `fjo ci` flattens it.
#[derive(Debug, Clone, Default, ClapArgs)]
pub struct StatusArgs {
    /// A pull request (42, #42, URL), a branch, or a commit. Defaults to the branch you are on
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Also show runs a newer run of the same workflow superseded
    #[arg(long)]
    pub all: bool,
    /// Only runs of this workflow file, e.g. `ci.yml`
    #[arg(short = 'W', long, value_name = "FILE")]
    pub workflow: Option<String>,
}

#[derive(Debug, Clone, Default, ClapArgs)]
pub struct LogsArgs {
    /// A pull request (42, #42, URL), a branch, or a commit. Defaults to the branch you are on
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Every job's log, not only the failed ones
    #[arg(long, conflicts_with = "job")]
    pub all: bool,
    /// Only this job: an id, or a name matched across every run
    #[arg(short = 'j', long, value_name = "ID|NAME")]
    pub job: Option<JobRef>,
    /// Only runs of this workflow file, e.g. `ci.yml`
    #[arg(short = 'W', long, value_name = "FILE")]
    pub workflow: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct WatchArgs {
    /// A pull request (42, #42, URL), a branch, or a commit. Defaults to the branch you are on
    #[arg(value_name = "TARGET")]
    pub target: Option<String>,
    /// Seconds between polls
    #[arg(short = 'i', long, value_name = "SECONDS", default_value_t = 5,
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval: u64,
    /// Seconds to wait for a first run to appear before giving up
    #[arg(long = "appear-timeout", value_name = "SECONDS", default_value_t = 60,
          value_parser = clap::value_parser!(u64).range(0..=86400))]
    pub appear_timeout: u64,
    /// Only runs of this workflow file, e.g. `ci.yml`
    #[arg(short = 'W', long, value_name = "FILE")]
    pub workflow: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Before the runtime, so `fjo ci --json` needs no host, no token and no network.
    if emit::discover(globals, fields_for(args.command.as_ref()))? {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        match &args.command {
            None => status(&rt, globals, &api, &args.status).await,
            Some(Cmd::Status(a)) => status(&rt, globals, &api, a).await,
            Some(Cmd::Logs(a)) => logs::run(&rt, globals, &api, a).await,
            Some(Cmd::Watch(a)) => watch::run(&rt, globals, &api, a).await,
        }
    })
}

/// Which generated field table `--json` validates against. Only `status` prints a document;
/// `logs` is a text pump and `watch` ends in `status`'s human view.
fn fields_for(cmd: Option<&Cmd>) -> Fields {
    match cmd {
        None | Some(Cmd::Status(_)) => RUN_FIELDS,
        Some(Cmd::Logs(_) | Cmd::Watch(_)) => Fields::None,
    }
}

/// The first seven characters of a SHA.
pub(crate) fn sha7(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The runs a subcommand works on: the newest per workflow (or all of them), optionally one
/// workflow's, in the order that puts what needs attention first.
pub(crate) fn select<'a>(
    runs: &'a [ActionRun],
    all: bool,
    workflow: Option<&str>,
) -> Vec<&'a ActionRun> {
    let mut picked = if all {
        let mut every: Vec<&ActionRun> = runs.iter().collect();
        sort_for_display(&mut every);
        every
    } else {
        latest_per_workflow(runs)
    };
    if let Some(w) = workflow {
        // `.forgejo/workflows/ci.yml` and `ci.yml` both mean the file Forgejo reports as `ci.yml`.
        let want = w.rsplit('/').next().unwrap_or(w);
        picked.retain(|r| r.workflow_id == want);
    }
    picked
}

/// `fjo ci <verb> [target]`, the command a hint names.
///
/// `-R` is carried over when the user gave it, so the hint runs as printed from wherever the
/// user is — a hint that only works inside the checkout is not "the exact next command".
pub(crate) fn hint(verb: &str, resolved: &Resolved, globals: &GlobalOpts) -> String {
    let mut cmd = format!("fjo ci {verb}");
    if let Some(sel) = &resolved.selector {
        cmd.push(' ');
        cmd.push_str(sel);
    }
    let is_url = resolved.selector.as_deref().is_some_and(|s| s.contains("://"));
    if globals.repo.is_some() && !is_url {
        cmd.push_str(&format!(" -R {}", resolved.slug));
    }
    cmd
}

/// The stderr note for a commit with nothing selected, and a check for the usual reason.
pub(crate) async fn no_runs(
    rt: &Runtime,
    api: &Api,
    resolved: &Resolved,
    runs: &[ActionRun],
    workflow: Option<&str>,
) {
    support::warn(&no_runs_message(resolved, runs, workflow));
    if runs.is_empty() {
        warn_if_actions_disabled(rt, api, &resolved.slug).await;
    }
}

fn no_runs_message(resolved: &Resolved, runs: &[ActionRun], workflow: Option<&str>) -> String {
    let at = format!("{} ({})", sha7(&resolved.sha), resolved.label);
    match workflow {
        Some(w) if !runs.is_empty() => {
            let mut have: Vec<&str> = runs.iter().map(|r| r.workflow_id.as_str()).collect();
            have.sort_unstable();
            have.dedup();
            format!("no run of {w} for {at}; its runs are of: {}", have.join(", "))
        }
        _ => format!(
            "no Actions runs for {at}. The workflows' `on:` filters may not match, Actions may \
             be disabled, or the push has not been processed yet."
        ),
    }
}

// ----------------------------------------------------------------------------------- status

async fn status(rt: &Runtime, globals: &GlobalOpts, api: &Api, args: &StatusArgs) -> Result<()> {
    let resolved = target::resolve_target(rt, globals, api, args.target.as_deref()).await?;
    let runs = target::runs_for_sha(api, &resolved.slug, &resolved.sha).await?;
    let selected = select(&runs, args.all, args.workflow.as_deref());
    report(rt, globals, api, &resolved, &runs, &selected, args.workflow.as_deref()).await
}

/// Print the status view (or `--json`) for `selected`, and turn it into the exit code.
///
/// Shared by `status` and `watch`, which ends by printing exactly what `status` would.
pub(crate) async fn report(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    resolved: &Resolved,
    runs: &[ActionRun],
    selected: &[&ActionRun],
    workflow: Option<&str>,
) -> Result<()> {
    let machine = globals.wants_machine_output();
    // The waiting diagnosis costs requests and only the human view prints it.
    let views = view::gather(api, &resolved.slug, selected, !machine).await?;
    if machine {
        emit::list(rt, globals, listing(selected)?, |_| {})?;
    } else {
        let next = view::next_verb(&views).map(|verb| hint(verb, resolved, globals));
        let mut out = support::writer(globals)?;
        out.write_all(view::render(resolved, &views, rt.term(), next.as_deref()).as_bytes())?;
        out.flush()?;
    }
    if selected.is_empty() {
        no_runs(rt, api, resolved, runs, workflow).await;
    }
    view::verdict(resolved, &views)
}

/// The selected runs as `--json` prints them: the API's own objects, validated against
/// [`RUN_FIELDS`].
fn listing<'a>(selected: &[&ActionRun]) -> Result<Listing<'a>> {
    Ok(Listing {
        fields: RUN_FIELDS,
        value: support::to_value(selected)?,
        count: selected.len(),
        total: None,
        noun: "runs",
    })
}

#[cfg(test)]
mod tests;

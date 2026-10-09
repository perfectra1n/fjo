//! `gea ci watch` — poll a target's runs until every one has finished, then report.
//!
//! The same two guarantees as `gea run watch`, asserted by tests rather than assumed:
//!
//! * **It does not spin.** Every iteration that does not return sleeps for `--interval`, whose
//!   parser refuses zero, so the worst case is one listing per second rather than a busy loop.
//! * **It is interruptible.** No `SIGINT` handler is installed, so Ctrl-C keeps its default
//!   disposition and kills the process at once — including in the middle of a sleep.
//!
//! One thing `run watch` never faces: right after a push, the commit may have **no runs yet**,
//! because Gitea has not processed the push. "No runs" is then not an answer, so the loop keeps
//! polling for up to `--appear-timeout`. If none appears it fails with `CiNoRuns` (exit 8) —
//! never 0, which `gea ci watch && gea pr merge` would read as a green commit.

use std::future::Future;
use std::time::Duration;

use gitea_client::Api;
use gitea_core::error::Result;
use gitea_model::ActionWorkflowRun;

use super::target::{self, Resolved};
use super::view::names;
use super::{WatchArgs, no_runs_error, no_runs_message, report, select, sha7};
use crate::cmd::run::{is_finished, outcome, warn_if_actions_disabled};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

pub(super) async fn run(
    rt: &Runtime,
    globals: &GlobalOpts,
    api: &Api,
    args: &WatchArgs,
) -> Result<()> {
    let resolved = target::resolve_target(rt, globals, api, args.target.as_deref()).await?;
    let poll = Poll {
        workflow: args.workflow.as_deref(),
        interval: Duration::from_secs(args.interval),
        appear_timeout: Duration::from_secs(args.appear_timeout),
    };
    let runs =
        follow(api, &resolved, &poll, tokio::time::sleep, |line| support::note(rt.term(), line))
            .await?;
    let selected = match appeared(&runs, &resolved, globals, poll.workflow) {
        Ok(selected) => selected,
        Err(e) => {
            // The error lists the usual causes; these say which one applies, when we can tell.
            if runs.is_empty() {
                warn_if_actions_disabled(rt, api, &resolved.slug).await;
            } else {
                support::warn(&no_runs_message(&resolved, &runs, poll.workflow));
            }
            return Err(e);
        }
    };
    report(rt, globals, api, &resolved, &runs, &selected, poll.workflow).await
}

/// What [`follow`]'s answer selects, or `CiNoRuns` (exit 8) when it selects nothing — the
/// watch gave up waiting for a run to appear.
///
/// Where `gea ci status` reports an empty commit as success (an empty listing is not an error),
/// `watch` is the command scripts gate on, and "CI never looked at this commit" must not pass
/// the gate.
pub(crate) fn appeared<'r>(
    runs: &'r [ActionWorkflowRun],
    resolved: &Resolved,
    globals: &GlobalOpts,
    workflow: Option<&str>,
) -> Result<Vec<&'r ActionWorkflowRun>> {
    let selected = select(runs, false, workflow);
    if selected.is_empty() {
        return Err(no_runs_error(resolved, globals, workflow));
    }
    Ok(selected)
}

/// How [`follow`] polls.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Poll<'a> {
    pub workflow: Option<&'a str>,
    pub interval: Duration,
    pub appear_timeout: Duration,
}

/// Poll until every selected run has finished, or until no run has appeared within
/// `appear_timeout`, and answer the commit's runs as of the last poll.
///
/// `sleep` is a parameter so a test can count the sleeps without taking any; the binary passes
/// `tokio::time::sleep`. The appear timeout is measured in slept intervals for the same reason:
/// the loop's behaviour is then a function of what the server answered, not of the wall clock.
pub(crate) async fn follow<S, F>(
    api: &Api,
    resolved: &Resolved,
    poll: &Poll<'_>,
    mut sleep: S,
    mut say: impl FnMut(&str),
) -> Result<Vec<ActionWorkflowRun>>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let mut seen: Option<Vec<State>> = None;
    let mut waited = Duration::ZERO;
    let mut announced = false;
    loop {
        let runs = target::runs_for_sha(api, &resolved.slug, &resolved.sha).await?;
        let selected = select(&runs, false, poll.workflow);
        if selected.is_empty() {
            if waited >= poll.appear_timeout {
                return Ok(runs);
            }
            if !announced {
                say(&format!(
                    "no runs for {} ({}) yet; waiting up to {}s for them to appear …",
                    sha7(&resolved.sha),
                    resolved.label,
                    poll.appear_timeout.as_secs()
                ));
                announced = true;
            }
        } else {
            let now = states(&selected);
            if let Some(before) = &seen {
                for line in changes(before, &now) {
                    say(&line);
                }
            }
            seen = Some(now);
            if selected.iter().all(|r| is_finished(&r.status)) {
                return Ok(runs);
            }
        }
        sleep(poll.interval).await;
        waited += poll.interval;
    }
}

/// One selected run as of one poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    /// Which workflow this is: `(path, event)`, the key
    /// [`super::target::latest_per_workflow`] selects on. Stable across polls, unlike the
    /// display name, which gains a qualifier the moment a same-named sibling appears.
    pub key: (String, String),
    /// What the line calls it: [`names`]'s name for it in this poll.
    pub name: String,
    /// The outcome word, `queued`, `in_progress`, `failure`, ….
    pub outcome: String,
}

/// Each selected run's identity, name and outcome word.
pub(crate) fn states(selected: &[&ActionWorkflowRun]) -> Vec<State> {
    names(selected)
        .into_iter()
        .zip(selected)
        .map(|(name, r)| State {
            key: (r.path.clone(), r.event.clone()),
            name,
            outcome: outcome(&r.status, &r.conclusion).to_owned(),
        })
        .collect()
}

/// One line per run whose state moved since the last poll: `ci.yml: in_progress → failure`, or
/// `ci.yml: queued` for a run that was not there before.
///
/// Runs are matched across polls by [`State::key`], never by name: two workflows can share a
/// file name, and a name changes when its twin appears, either of which would put one run's
/// transition under another's name.
pub(crate) fn changes(before: &[State], now: &[State]) -> Vec<String> {
    now.iter()
        .filter_map(|s| match before.iter().find(|b| b.key == s.key) {
            Some(b) if b.outcome == s.outcome => None,
            Some(b) => Some(format!("{}: {} → {}", s.name, b.outcome, s.outcome)),
            None => Some(format!("{}: {}", s.name, s.outcome)),
        })
        .collect()
}

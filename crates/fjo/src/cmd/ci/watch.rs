//! `fjo ci watch` — poll a target's runs until every one has finished, then report.
//!
//! The same two guarantees as `fjo run watch`, asserted by tests rather than assumed:
//!
//! * **It does not spin.** Every iteration that does not return sleeps for `--interval`, whose
//!   parser refuses zero, so the worst case is one listing per second rather than a busy loop.
//! * **It is interruptible.** No `SIGINT` handler is installed, so Ctrl-C keeps its default
//!   disposition and kills the process at once — including in the middle of a sleep.
//!
//! One thing `run watch` never faces: right after a push, the commit may have **no runs yet**,
//! because Forgejo has not processed the push. "No runs" is then not an answer, so the loop keeps
//! polling for up to `--appear-timeout` before it reports the commit as having none.
//!
//! And one Forgejo-specific edge: a run's status turns `failure` the moment one of its jobs
//! fails, while its other jobs may still be running (measured on 16.0.5). The loop stops on the
//! run's status, so a red commit is reported as soon as it is decided rather than when its last
//! job ends — the answer cannot change, and `fjo ci watch && fjo pr merge` should fail fast.

use std::future::Future;
use std::time::Duration;

use forgejo_client::Api;
use forgejo_core::error::Result;
use forgejo_model::ActionRun;

use super::target::{self, Resolved};
use super::view::names;
use super::{WatchArgs, report, select, sha7};
use crate::cmd::run::is_finished;
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
    let selected = select(&runs, false, poll.workflow);
    report(rt, globals, api, &resolved, &runs, &selected, poll.workflow).await
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
) -> Result<Vec<ActionRun>>
where
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let mut seen: Option<Vec<(String, String)>> = None;
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

/// Each selected run's name and outcome word.
fn states(selected: &[&ActionRun]) -> Vec<(String, String)> {
    names(selected).into_iter().zip(selected).map(|(name, r)| (name, r.status.clone())).collect()
}

/// One line per run whose state moved since the last poll: `ci.yml: running → failure`, or
/// `ci.yml: waiting` for a run that was not there before.
fn changes(before: &[(String, String)], now: &[(String, String)]) -> Vec<String> {
    now.iter()
        .filter_map(|(name, state)| match before.iter().find(|(n, _)| n == name) {
            Some((_, was)) if was == state => None,
            Some((_, was)) => Some(format!("{name}: {was} → {state}")),
            None => Some(format!("{name}: {state}")),
        })
        .collect()
}

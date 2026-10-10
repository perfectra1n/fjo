//! A real Actions runner, so a run can actually *execute* rather than sit `waiting` forever.
//!
//! Without this the suite can drive every part of the run lifecycle except the outcome: a run is
//! born `waiting`, no job ever starts, there is no conclusion to read and no log to fetch. That
//! was a deliberate, documented limit of `live_actions.rs`, and it left the commands whose whole
//! job is to report an outcome — `run logs --failed`, `run watch --exit-status`, `run jobs`'
//! statuses — tested only against `FakeTransport` fixtures we wrote ourselves.
//!
//! # The shape: `forgejo-runner` in **host mode**
//!
//! [`Instance::attach_runner`] starts the official runner image as a sibling container. Its
//! labels are host-mode labels ([`HOST_LABEL`], `fjo-itest:host`), which tell the runner to
//! execute steps *inside its own container* instead of starting a job container per job. That is
//! the entire reason this is cheap and portable:
//!
//! * **No Docker-in-Docker.** A `docker://` label would make the runner talk to a Docker daemon to
//!   start job images, which means mounting the host's socket into it (a privilege escalation we
//!   would be handing a test fixture) or running a privileged daemon beside it. Host mode needs
//!   neither, so it works anywhere the Forgejo container itself does.
//! * **No job images to pull.** Host mode pulls nothing beyond the runner itself (~46 MB).
//!
//! The price is the step environment: the runner image is Alpine with `bash`, `sh` (busybox) and
//! `git`, and **no `node`**, so `uses:` actions (checkout, setup-*) cannot run. Test workflows use
//! `run:` steps only, and [`RUN_SHELL`] pins `sh` so a future image without bash does not change
//! what a test means.
//!
//! # Registration: the API route, not `forgejo-runner register`
//!
//! This is where the port from `gea` (whose `act_runner` registers itself from environment
//! variables) diverges most. Forgejo 16 has `POST /admin/actions/runners`, which creates the
//! runner record **server-side** and answers with its `id`, `uuid` and `token`; runner v13
//! accepts exactly those three in its config's `server.connections`, and marks
//! `forgejo-runner register` deprecated. So there is no registration round trip at all: the id
//! is known before the container starts, the labels live in the config (one source of truth),
//! and "is it up yet?" is a `GET` of that one runner rather than a scan of a listing by name.
//!
//! # How it reaches Forgejo, and why `ROOT_URL` does not matter
//!
//! The connection URL is the Forgejo container's **own** address on the Docker network,
//! `http://<container_ip>:3000`, not [`Instance::base_url`]. `base_url` is the host-published
//! port, which a sibling container cannot reach. The runner fetches tasks, reports state and
//! uploads logs through that one URL; what *would* break is a step that follows a
//! server-supplied URL, such as `actions/checkout` cloning `${{ github.server_url }}`, which
//! cannot run here anyway (no `node`).
//!
//! # Why one shared runner per process, and `attach_runner` as the building block
//!
//! [`shared_runner`] (and [`runner_or_skip!`](crate::runner_or_skip)) start one runner per test
//! binary against the one [`shared`](crate::shared) instance, with [`HOST_LABEL`]. That mirrors the
//! instance's own model for the same reason — startup is paid once — and for one more that is
//! specific to runners: **any runner carrying a label will take any job asking for it.** If two
//! tests each attached their own `fjo-itest` runner, test A's job could be claimed by test B's
//! runner, and when test B finished and dropped its guard, A's job would die mid-step and
//! conclude `failure` for a reason that has nothing to do with A. A single runner that lives as
//! long as the process cannot be pulled out from under anybody.
//!
//! [`Instance::attach_runner`] stays public for a test that genuinely needs a runner of its own;
//! it should then give it a label nobody else uses. The config raises `capacity` to
//! [`CAPACITY`] so tests sharing the one runner do not queue behind each other.
//!
//! # Unavailability is a skip, and only unavailability
//!
//! `Err` from [`Instance::attach_runner`] means "no runner could be had here": the image could
//! not be pulled, the container exited, or it never reported in within [`ONLINE_TIMEOUT`]. The
//! [`runner_or_skip!`](crate::runner_or_skip) macro turns that into the harness's usual skip —
//! printed on stdout, recorded as nothing covered, and a **panic under `FJO_ITEST_REQUIRE`**, which
//! `cargo xtask itest` sets unless `--allow-skip`. A server misbehaving (it refuses to register a
//! runner) is a test failure instead, and panics.
//!
//! # Measured on the pinned versions (Forgejo 16.0.5, forgejo-runner 13.2.0, warm image cache)
//!
//! * Runner listed `active` ~1 s after the container starts; a push-triggered two-job workflow
//!   (`echo`, `exit 1`) has finished ~2 s after the push.
//! * Statuses are the outcome: `waiting` → `running` → `success` / `failure` / `cancelled` /
//!   `skipped`. There is no `completed` + `conclusion` pair as on Gitea. And a **run** reads
//!   `failure` as soon as one of its jobs fails, while sibling jobs are still `running` — so a
//!   finished run is not proof that every job's log is complete.
//! * A same-repository pull request triggers **two** runs for the head commit: `push` (with
//!   `prettyref` = the branch) and `pull_request` (with `prettyref` = `#<n>`, and no branch
//!   anywhere). `?ref=refs/heads/<branch>` finds only the push run (`?ref=<branch>` finds
//!   nothing); `?head_sha=` finds both.
//! * `html_url` is `…/actions/runs/<index_in_repo>`, **not** the id, while the API path takes the
//!   id (`GET actions/runs/<index>` is a 404 once the two differ). Gitea is the other way round.
//! * The commit statuses Actions posts carry a **path-only** `target_url`,
//!   `/<owner>/<repo>/actions/runs/<index>/jobs/<n>`, where `<n>` is the job's **0-based
//!   position** in the run (jobs with ids 4 and 5 link as `jobs/0` and `jobs/1`). Contexts read
//!   `CI / ok (push)`; each job posts `pending` then its outcome, and both stay listed.
//! * `ActionRun.duration` is a Go `time.Duration`: **nanoseconds**, measured from `started` to
//!   `stopped` at one-second granularity. `stopped` is stamped when the run's status is
//!   *decided*, so a run whose first job fails at once reports no duration (`null`) even if its
//!   other jobs run on for seconds.
//! * There is no rerun route (`POST actions/runs/<id>/rerun` is a router 404); `cancel` exists
//!   and answers 204 even for a run that already finished.
//! * A job's log carries each step's *command text* (`⭐ Run Main echo …`) as well as its
//!   output, so a marker written literally in a `run:` line is in the log even if the step never
//!   printed it — see [`echo_unseen`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::{
    FORGEJO_PORT, Instance, KEEP_LABEL, LABEL, PID_LABEL, TestRepo, daemon_unreachable, split_image,
};

/// The runner release these tests target. Pinned for the same reason as
/// [`FORGEJO_IMAGE`](crate::FORGEJO_IMAGE): a floating tag would let a runner release change what
/// a test means with no commit here to show for it. Override with `FJO_ITEST_RUNNER_IMAGE`.
pub const RUNNER_IMAGE: &str = "code.forgejo.org/forgejo/runner:13.2.0";

/// What a test workflow puts in `runs-on:`. The label a runner offers is this plus a scheme —
/// see [`HOST_LABEL`].
pub const RUNS_ON: &str = "fjo-itest";

/// The label the shared runner offers: [`RUNS_ON`], executed in host mode.
///
/// `name:host` is the runner's syntax for "run jobs with this label directly in the runner's own
/// environment". Forgejo stores and lists only the name part (`fjo-itest`), which is what a
/// workflow's `runs-on` is matched against.
pub const HOST_LABEL: &str = "fjo-itest:host";

/// The shell test workflows pin in `defaults.run.shell`. See the module docs.
pub const RUN_SHELL: &str = "sh";

/// Where test workflows live. Forgejo reads `.forgejo/workflows` first and falls back to
/// `.github/workflows`; the Forgejo-specific directory is used so a test never depends on the
/// fallback.
pub const WORKFLOW_DIR: &str = ".forgejo/workflows";

/// How long a started runner has to be listed as online. A cold `docker pull` happens before
/// this clock starts; well inside nextest's 180 s per-test kill, so the failure is this module's
/// diagnosis and not an anonymous timeout.
pub const ONLINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Jobs one runner executes at once. The default is 1, which would serialise every test sharing
/// the runner behind whichever one is running; host-mode jobs here are a few `echo`s, so
/// parallelism is free.
pub const CAPACITY: u32 = 8;

/// The runner image, honouring `FJO_ITEST_RUNNER_IMAGE`.
fn runner_image() -> String {
    std::env::var("FJO_ITEST_RUNNER_IMAGE").unwrap_or_else(|_| RUNNER_IMAGE.to_owned())
}

/// The runner's config, written into the container and passed to `daemon --config`.
///
/// * `server.connections` — the `uuid`/`token` pair `POST /admin/actions/runners` returned, and
///   the labels. One connection, named after the harness.
/// * `capacity` — see [`CAPACITY`].
/// * `cache.enabled: false` — the cache server binds a port and advertises an address to jobs;
///   no test uses `actions/cache` (it cannot: no `node`), so it would be a moving part for nothing.
/// * `fetch_interval: 1s` — the default 2s is most of a test's wall time when the job itself
///   takes milliseconds.
/// * `timeout: 10m` — a job that hangs should fail the run long before the 3h default, and well
///   after anything a test workflow legitimately does.
fn config_yaml(url: &str, uuid: &str, token: &str, labels: &[&str]) -> String {
    let labels = labels.iter().map(|l| format!("        - {l}\n")).collect::<String>();
    format!(
        "log:\n  level: info\n\
         runner:\n  file: .runner\n  capacity: {CAPACITY}\n  timeout: 10m\n  \
         shutdown_timeout: 0s\n  fetch_timeout: 5s\n  fetch_interval: 1s\n  report_interval: 1s\n\
         cache:\n  enabled: false\n\
         server:\n  connections:\n    {LABEL}:\n      url: {url}\n      uuid: {uuid}\n      \
         token: {token}\n      labels:\n{labels}"
    )
}

/// A running forgejo-runner, registered against an [`Instance`].
///
/// Dropping it removes the container and then deletes the runner's record from Forgejo, so a
/// later listing does not accumulate dead `offline` runners. The shared runner in
/// [`shared_runner`] is never dropped (it lives in a `OnceLock`, like the shared instance) and is
/// collected by [`reap_stale`](crate::reap_stale) on the next run, which finds it by the same
/// labels.
pub struct RunnerGuard<'a> {
    inst: &'a Instance,
    /// The name it registered under, unique per process.
    pub name: String,
    /// Forgejo's id for it, from the registration reply.
    pub id: i64,
    /// The labels it offers, scheme included (e.g. `fjo-itest:host`).
    pub labels: Vec<String>,
    container: Option<Container<GenericImage>>,
    keep: bool,
}

// Hand-written for the same reason as `Instance`'s: `Container`'s `Debug` calls the daemon.
impl std::fmt::Debug for RunnerGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerGuard")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("labels", &self.labels)
            .field("container", &self.container.as_ref().map(|c| c.id().to_owned()))
            .finish_non_exhaustive()
    }
}

impl RunnerGuard<'_> {
    /// The runner container's recent output, for a failure message.
    pub fn logs(&self) -> String {
        self.container.as_ref().map(container_logs).unwrap_or_default()
    }
}

impl Drop for RunnerGuard<'_> {
    fn drop(&mut self) {
        let Some(container) = self.container.take() else { return };
        if self.keep {
            eprintln!("FJO_ITEST_KEEP set: leaving runner {} ({})", self.name, container.id());
            std::mem::forget(container);
            return;
        }
        // Container first, so it is not polling for work while its record disappears.
        drop(container);
        let _ = self.inst.api("DELETE", &format!("admin/actions/runners/{}", self.id), None);
    }
}

fn container_logs(container: &Container<GenericImage>) -> String {
    let out = container.stdout_to_vec().unwrap_or_default();
    let err = container.stderr_to_vec().unwrap_or_default();
    let all = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    let lines: Vec<&str> = all.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

impl Instance {
    /// Start a forgejo-runner registered against this instance, offering `labels`, and wait
    /// until Forgejo lists it online.
    ///
    /// `labels` are runner label specs, e.g. [`HOST_LABEL`]. Prefer [`shared_runner`] unless
    /// the test needs a runner of its own — and then give it a label no other test asks for; the
    /// module docs explain what goes wrong otherwise.
    ///
    /// `Err` means **no runner is available here**, and is meant to become a skip (see
    /// [`runner_or_skip!`](crate::runner_or_skip)): the image could not be pulled, the container
    /// died, or it never came online. The message carries the runner's logs. A server that
    /// refuses to register a runner is not "unavailable" but broken, and panics.
    ///
    /// Only for an instance this harness started: the runner reaches Forgejo at the container's
    /// own address, which an instance attached through `FJO_TEST_HOST` does not have.
    pub fn attach_runner(&self, labels: &[&str]) -> Result<RunnerGuard<'_>, String> {
        let Some(ip) = self.container_ip() else {
            return Err("this instance was not started by the harness (FJO_TEST_HOST), so there \
                        is no container address for a runner to reach it at; attach your own \
                        runner offering `fjo-itest` to that instance instead"
                .to_owned());
        };
        if labels.is_empty() || labels.iter().any(|l| l.contains([',', ' ', '\n'])) {
            // Spliced into hand-built YAML, one per line.
            panic!("runner labels must be non-empty and free of commas and spaces: {labels:?}");
        }

        static N: AtomicU32 = AtomicU32::new(0);
        let name =
            format!("{LABEL}-runner-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let (code, body) =
            self.api("POST", "admin/actions/runners", Some(&format!(r#"{{"name":"{name}"}}"#)));
        assert!((200..300).contains(&code), "could not register a runner: HTTP {code}: {body}");
        let reply: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let (Some(id), Some(uuid), Some(token)) =
            (reply["id"].as_i64(), reply["uuid"].as_str(), reply["token"].as_str())
        else {
            panic!("the runner registration reply lacks id, uuid or token: {body}");
        };

        let keep = std::env::var_os("FJO_ITEST_KEEP").is_some();
        let (image, tag) = split_image(&runner_image());
        let config = config_yaml(&format!("http://{ip}:{FORGEJO_PORT}"), uuid, token, labels);
        let request = GenericImage::new(image, tag)
            .with_container_name(&name)
            .with_label(LABEL, "1")
            .with_label(PID_LABEL, std::process::id().to_string())
            .with_label(KEEP_LABEL, if keep { "1" } else { "" })
            .with_copy_to("/config.yaml", config.into_bytes())
            .with_cmd(["forgejo-runner", "daemon", "--config", "/config.yaml"]);

        let started = Instant::now();
        let container = request.start().map_err(|e| {
            // Nothing will ever connect as this runner; do not leave its record behind.
            let _ = self.api("DELETE", &format!("admin/actions/runners/{id}"), None);
            let daemon = daemon_unreachable().map(|w| format!(" ({w})")).unwrap_or_default();
            format!("could not start the runner container from {}: {e}{daemon}", runner_image())
        })?;

        let guard = RunnerGuard {
            inst: self,
            name,
            id,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            container: Some(container),
            keep,
        };
        self.wait_runner_online(&guard, started)?;
        Ok(guard)
    }

    /// Poll the runner's own record until it is not `offline`.
    ///
    /// "Not offline" rather than "== online": the status is derived from the runner's last
    /// contact and whether it is running a task, and a runner that came up and immediately
    /// claimed a job is every bit as available as one that is idle.
    fn wait_runner_online(&self, guard: &RunnerGuard<'_>, started: Instant) -> Result<(), String> {
        let mut last = String::from("not yet contacted");
        while started.elapsed() < ONLINE_TIMEOUT {
            let (code, body) =
                self.api("GET", &format!("admin/actions/runners/{}", guard.id), None);
            if (200..300).contains(&code) {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let status = v["status"].as_str().unwrap_or("?").to_owned();
                if status != "offline" {
                    return Ok(());
                }
                last = format!("listed, status {status}");
            } else {
                last = format!("the runner record answered HTTP {code}: {body}");
            }
            let running = guard.container.as_ref().is_some_and(|c| c.is_running().unwrap_or(false));
            if !running {
                return Err(format!(
                    "the runner container {} exited after {:?} ({last}). Its logs:\n{}",
                    guard.name,
                    started.elapsed(),
                    guard.logs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "runner {} did not come online within {ONLINE_TIMEOUT:?} ({last}). It was told to \
             reach Forgejo at the container's own address; if its logs below show connection \
             errors, the two containers are not on a network they share. Its logs:\n{}",
            guard.name,
            guard.logs()
        ))
    }
}

/// One host-mode runner offering [`HOST_LABEL`], shared by every test in this binary, attached to
/// the [`shared`](crate::shared) instance. See the module docs for why sharing is the safe default.
///
/// `Err` is the unavailability reason, remembered: a second test in the same binary gets the
/// same answer without paying a second 60 s timeout.
pub fn shared_runner(inst: &'static Instance) -> Result<&'static RunnerGuard<'static>, String> {
    static SHARED: OnceLock<Result<RunnerGuard<'static>, String>> = OnceLock::new();
    SHARED.get_or_init(|| inst.attach_runner(&[HOST_LABEL])).as_ref().map_err(Clone::clone)
}

/// Get the [`shared_runner`] for `inst`, or skip the test — the runner's counterpart of
/// [`instance_or_skip!`](crate::instance_or_skip), with the same contract: the skip is printed on
/// stdout, and **`FJO_ITEST_REQUIRE` turns it into a panic**, so `cargo xtask itest` cannot go
/// green over a suite whose runner never started. Put it before `cover!`.
#[macro_export]
macro_rules! runner_or_skip {
    ($inst:expr) => {
        match $crate::runner::shared_runner($inst) {
            Ok(r) => r,
            Err(reason) => {
                let msg = format!(
                    "no Actions runner available: {reason}\n\
                     Make the image {} pullable (or set FJO_ITEST_RUNNER_IMAGE), or run with \
                     --allow-skip to accept skipping the tests that need jobs to execute.",
                    $crate::runner::RUNNER_IMAGE
                );
                if ::std::env::var_os("FJO_ITEST_REQUIRE").is_some() {
                    panic!("FJO_ITEST_REQUIRE is set but {msg}");
                }
                println!("SKIPPED ({}): {msg}", ::std::module_path!());
                eprintln!("SKIPPED: {msg}");
                return;
            }
        }
    };
}

// ------------------------------------------------------------------------- workflow helpers

/// A workflow file for the host-mode runner: `on:` is spliced in verbatim, and each `(job, script)`
/// becomes a job on [`RUNS_ON`] with one `run:` step holding `script` as a block scalar.
///
/// `defaults.run.shell` is pinned to [`RUN_SHELL`]. `on` is YAML for the `on:` value, e.g.
/// `"[push]"`, `"[push, pull_request]"`, or `"\n  workflow_dispatch:"`.
///
/// ```
/// let yaml = fjo_itest::runner::workflow_yaml("[push]", &[("ok", "echo fine"), ("bad", "exit 1")]);
/// assert!(yaml.contains("runs-on: fjo-itest"));
/// ```
pub fn workflow_yaml(on: &str, jobs: &[(&str, &str)]) -> String {
    let mut y = format!("name: CI\non: {on}\ndefaults:\n  run:\n    shell: {RUN_SHELL}\njobs:\n");
    for (name, script) in jobs {
        y.push_str(&format!("  {name}:\n    runs-on: {RUNS_ON}\n    steps:\n      - run: |\n"));
        for line in script.lines() {
            y.push_str(&format!("          {line}\n"));
        }
    }
    y
}

/// A marker unique to this process and call, for asserting that a job's output (and only that
/// job's) reached a log.
pub fn unique_marker(tag: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!("fjo-marker-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// A shell line that prints `marker` without `marker` appearing in the line itself.
///
/// A job log can carry each step's command text as well as its output, so `echo MARKER` would
/// put `MARKER` in the log even if the step never ran its echo. Printing it in two halves means
/// the joined marker can only be in the log because the shell printed it.
pub fn echo_unseen(marker: &str) -> String {
    let mid = marker.len() / 2;
    let (a, b) = marker.split_at(mid);
    format!("printf '%s%s\\n' '{a}' '{b}'")
}

/// Statuses after which a Forgejo run or job will not change on its own. Forgejo has no
/// `completed` status with a separate `conclusion`, as Gitea does: the status *is* the outcome.
pub fn is_finished(status: &str) -> bool {
    matches!(status, "success" | "failure" | "cancelled" | "skipped")
}

impl TestRepo<'_> {
    /// Wait until at least `at_least` Actions runs exist for `sha` and every one of them is
    /// finished, returning them (the API's run objects, newest first).
    ///
    /// Through the raw API with `?head_sha=`. `at_least` is needed because runs are created
    /// asynchronously after a push: "all runs for this commit are finished" is vacuously true
    /// for the moment before the first one exists.
    ///
    /// `Err` on timeout describes what was seen, so the failure says whether the runs never
    /// appeared (the workflow did not trigger) or appeared and never finished (no runner took
    /// them).
    pub fn wait_for_runs_finished(
        &self,
        sha: &str,
        at_least: usize,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, String> {
        let started = Instant::now();
        let mut seen = String::from("no runs yet");
        while started.elapsed() < timeout {
            let (code, body) = self.api("GET", &format!("actions/runs?head_sha={sha}"), None);
            if code == 200 {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let runs = v["workflow_runs"].as_array().cloned().unwrap_or_default();
                seen = runs
                    .iter()
                    .map(|r| format!("#{} {} {}", r["id"], r["event"], r["status"]))
                    .collect::<Vec<_>>()
                    .join(", ");
                if runs.len() >= at_least
                    && runs.iter().all(|r| r["status"].as_str().is_some_and(is_finished))
                {
                    return Ok(runs);
                }
            } else {
                seen = format!("HTTP {code}: {body}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "after {timeout:?}, {} still did not have {at_least} finished run(s) for {sha}: \
             [{seen}]",
            self.slug()
        ))
    }

    /// Wait until every job of run `id` is finished, returning them.
    ///
    /// Needed on top of [`wait_for_runs_finished`](Self::wait_for_runs_finished) because a
    /// Forgejo run reads `failure` as soon as one job fails, while its other jobs may still be
    /// running (see the module docs) — and a test about *those* jobs' logs would race them.
    pub fn wait_for_jobs_finished(
        &self,
        id: i64,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, String> {
        let started = Instant::now();
        let mut seen = String::from("no jobs yet");
        while started.elapsed() < timeout {
            let (code, body) = self.api("GET", &format!("actions/runs/{id}/jobs"), None);
            if code == 200 {
                let jobs: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap_or_default();
                seen = jobs
                    .iter()
                    .map(|j| format!("{} {}", j["name"], j["status"]))
                    .collect::<Vec<_>>()
                    .join(", ");
                if !jobs.is_empty()
                    && jobs.iter().all(|j| j["status"].as_str().is_some_and(is_finished))
                {
                    return Ok(jobs);
                }
            } else {
                seen = format!("HTTP {code}: {body}");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!("after {timeout:?}, run {id}'s jobs had not all finished: [{seen}]"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The YAML is assembled by hand, so the one thing worth pinning without Docker is that it is
    /// shaped the way the runner reads it: job under `jobs`, the step's script indented under a
    /// block scalar, and the host-mode label's *name* (not the `:host` spec) in `runs-on`.
    #[test]
    fn a_built_workflow_targets_the_host_label_by_name_with_the_pinned_shell() {
        let y = workflow_yaml("[push]", &[("ok", "echo a\necho b"), ("bad", "exit 1")]);
        assert!(y.contains("on: [push]\n"), "{y}");
        assert!(y.contains("    shell: sh\n"), "{y}");
        assert!(y.contains("  ok:\n    runs-on: fjo-itest\n"), "{y}");
        assert!(y.contains("      - run: |\n          echo a\n          echo b\n"), "{y}");
        assert!(y.contains("  bad:\n"), "{y}");
        assert!(!y.contains(":host"), "runs-on names the label, not the spec: {y}");
    }

    /// The runner config carries the connection the registration route returned, and the label
    /// specs under it — the spec form (`:host`), unlike `runs-on`.
    #[test]
    fn the_runner_config_names_the_connection_and_its_labels() {
        let y = config_yaml("http://10.0.0.2:3000", "u-u-i-d", "tok", &[HOST_LABEL]);
        assert!(y.contains("      url: http://10.0.0.2:3000\n"), "{y}");
        assert!(y.contains("      uuid: u-u-i-d\n      token: tok\n"), "{y}");
        assert!(y.contains("      labels:\n        - fjo-itest:host\n"), "{y}");
        assert!(y.contains("cache:\n  enabled: false\n"), "{y}");
    }

    #[test]
    fn an_unseen_echo_never_contains_the_marker_it_prints() {
        let m = unique_marker("x");
        let line = echo_unseen(&m);
        assert!(!line.contains(&m), "{line}");
        let out = std::process::Command::new("sh").args(["-c", &line]).output().expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), m);
    }
}

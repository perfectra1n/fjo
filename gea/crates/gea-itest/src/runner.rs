//! A real Actions runner, so a run can actually *execute* rather than sit `queued` forever.
//!
//! Without this the suite can drive every part of the run lifecycle except the outcome: a run is
//! born `queued`, no job ever starts, there is no conclusion to read and no log to fetch. That
//! was a deliberate, documented limit of `live_actions.rs`, and it left the commands whose whole
//! job is to report an outcome — `run logs --failed`, `run watch --exit-status`, `run jobs`'
//! conclusions — tested only against `FakeTransport` fixtures we wrote ourselves.
//!
//! # The shape: `gitea/act_runner` in **host mode**
//!
//! [`Instance::attach_runner`] starts the official runner image as a sibling container. Its
//! labels are host-mode labels ([`HOST_LABEL`], `gea-itest:host`), which tell the runner to
//! execute steps *inside its own container* instead of starting a job container per job. That is
//! the entire reason this is cheap and portable:
//!
//! * **No Docker-in-Docker.** A `docker://` label would make the runner talk to a Docker daemon to
//!   start job images, which means mounting the host's socket into it (a privilege escalation we
//!   would be handing a test fixture) or running the `-dind` variant privileged. Host mode needs
//!   neither, so it works anywhere the Gitea container itself does.
//! * **No job images to pull.** The `ubuntu-latest` runner images are ~1 GB each; host mode pulls
//!   nothing beyond the runner itself (~46 MB).
//!
//! The price is the step environment: the runner image is Alpine with `bash`, `sh` (busybox) and
//! `git`, and **no `node`**, so `uses:` actions (checkout, setup-*) cannot run. Test workflows use
//! `run:` steps only. Measured on 0.6.1: with no `shell:` the runner picks
//! `bash --noprofile --norc -e -o pipefail {0}` because bash is present; [`RUN_SHELL`] pins `sh`
//! anyway so a future image without bash does not change what a test means.
//!
//! # How it reaches Gitea, and why `ROOT_URL` does not matter
//!
//! `GITEA_INSTANCE_URL` is the Gitea container's **own** address on the Docker network,
//! `http://<container_ip>:3000`, not [`Instance::base_url`]. `base_url` is the host-published
//! port, which a sibling container cannot reach — and which is also what Gitea's `ROOT_URL` says,
//! so the obvious worry is that Gitea hands the runner URLs built from it. Measured: it does not
//! matter for anything a host-mode `run:` step does. The runner fetches tasks, reports state and
//! uploads logs through the Connect service at `GITEA_INSTANCE_URL` and nowhere else — its log
//! line reads `task 1 repo is geatest/r1 https://github.com http://172.17.0.2:3000` — and every
//! log a job printed was downloadable afterwards. What *would* break is a step that follows a
//! server-supplied URL, such as `actions/checkout` cloning `${{ github.server_url }}`; that cannot
//! run here anyway (no `node`), and a test that needs the repository contents can `git clone`
//! `http://<ip>:3000/...` itself.
//!
//! # Why one shared runner per process, and `attach_runner` as the building block
//!
//! [`shared_runner`] (and [`runner_or_skip!`](crate::runner_or_skip)) start one runner per test
//! binary against the one [`shared`](crate::shared) instance, with [`HOST_LABEL`]. That mirrors the
//! instance's own model for the same reason — startup is paid once — and for one more that is
//! specific to runners: **any runner carrying a label will take any job asking for it.** If two
//! tests each attached their own `gea-itest` runner, test A's job could be claimed by test B's
//! runner, and when test B finished and dropped its guard, A's job would die mid-step and conclude
//! `failure` for a reason that has nothing to do with A. A single runner that lives as long as the
//! process cannot be pulled out from under anybody.
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
//! printed on stdout, recorded as nothing covered, and a **panic under `GEA_ITEST_REQUIRE`**, which
//! `cargo xtask itest` sets unless `--allow-skip`. A server misbehaving (it refuses to mint a
//! registration token) is a test failure instead, and panics, exactly as
//! [`Instance::register_runner`] does.
//!
//! # Measured on the pinned versions (Gitea 1.27.3, act_runner 0.6.1, warm image cache)
//!
//! * Runner listed `online` ~1–2 s after the container starts.
//! * A push-triggered two-job workflow (`echo`, `exit 1`) is `completed` ~3 s after the push.
//! * The registration-token route is **POST** `admin/actions/runners/registration-token`; a GET
//!   on the same path is routed to "runner by id" and answers 404 "runner with id 0".
//! * A same-repository pull request triggers **two** runs for the head commit: `push` (with
//!   `head_branch` = the branch) when the branch is pushed, and `pull_request` when the PR opens.
//!   The `pull_request` run has **no `head_branch` key at all** in the API reply (nor do its jobs),
//!   `?branch=` does not find it, and `?head_sha=` and `?event=pull_request` do. Both runs carry
//!   the PR in `pull_requests`. The step sees `GITHUB_REF=refs/pull/<n>/head`,
//!   `GITHUB_HEAD_REF=<branch>`, and the commit status contexts are
//!   `CI / ok (push)` and `CI / ok (pull_request)`.
//! * A run's `conclusion` key is **absent** (not empty) until the run completes.
//! * The run log of a step contains the step's *command text* as well as its output, so a marker
//!   written literally in a `run:` line is in the log even if the step never printed it — see
//!   [`echo_unseen`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::{
    GITEA_PORT, Instance, KEEP_LABEL, LABEL, PID_LABEL, TestRepo, daemon_unreachable, split_image,
};

/// The runner release these tests target. Pinned for the same reason as
/// [`GITEA_IMAGE`](crate::GITEA_IMAGE): a floating tag would let a runner release change what a
/// test means with no commit here to show for it. Override with `GEA_ITEST_RUNNER_IMAGE`.
pub const ACT_RUNNER_IMAGE: &str = "docker.io/gitea/act_runner:0.6.1";

/// What a test workflow puts in `runs-on:`. The label a runner offers is this plus a scheme —
/// see [`HOST_LABEL`].
pub const RUNS_ON: &str = "gea-itest";

/// The label the shared runner registers with: [`RUNS_ON`], executed in host mode.
///
/// `name:host` is act_runner's syntax for "run jobs with this label directly in the runner's own
/// environment". Gitea stores and lists only the name part (`gea-itest`), which is what a
/// workflow's `runs-on` is matched against.
pub const HOST_LABEL: &str = "gea-itest:host";

/// The shell test workflows pin in `defaults.run.shell`. See the module docs for why it is
/// pinned even though the image's default (bash) would also work.
pub const RUN_SHELL: &str = "sh";

/// How long a started runner has to be listed as online. Measured at ~1–2 s warm; the rest is
/// for a cold `docker pull` of a 46 MB image, which happens before this clock starts anyway, and
/// for `run.sh`'s own registration retry (5 s between attempts). Well inside nextest's 180 s
/// per-test kill, so the failure is this module's diagnosis and not an anonymous timeout.
pub const ONLINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Jobs one runner executes at once. act_runner's default is 1, which would serialise every test
/// sharing the runner behind whichever one is running; host-mode jobs here are a few `echo`s, so
/// parallelism is free.
pub const CAPACITY: u32 = 8;

/// The runner image, honouring `GEA_ITEST_RUNNER_IMAGE`.
fn runner_image() -> String {
    std::env::var("GEA_ITEST_RUNNER_IMAGE").unwrap_or_else(|_| ACT_RUNNER_IMAGE.to_owned())
}

/// act_runner's config, written into the container as `CONFIG_FILE`.
///
/// * `capacity` — see [`CAPACITY`].
/// * `cache.enabled: false` — the cache server binds a port and advertises an address to jobs;
///   no test uses `actions/cache` (it cannot: no `node`), so it would be a moving part for nothing.
/// * `fetch_interval: 1s` — the default 2s, backing off to 5s, is most of a test's wall time when
///   the job itself takes milliseconds.
/// * `timeout: 10m` — a job that hangs should fail the run long before the 3h default, and well
///   after anything a test workflow legitimately does.
/// * **No `labels:` key.** When present it overrides what the runner registered with, at daemon
///   start; absent, the labels come from registration (`GITEA_RUNNER_LABELS`), so the caller's
///   `labels` argument is the one source of truth.
fn config_yaml() -> String {
    format!(
        "log:\n  level: info\n\
         runner:\n  file: .runner\n  capacity: {CAPACITY}\n  timeout: 10m\n  \
         fetch_timeout: 5s\n  fetch_interval: 1s\n  fetch_interval_max: 2s\n  \
         log_report_interval: 1s\n  state_report_interval: 1s\n\
         cache:\n  enabled: false\n"
    )
}

/// A running act_runner, registered against an [`Instance`].
///
/// Dropping it removes the container and then deletes the runner's record from Gitea, so a later
/// listing does not accumulate dead `offline` runners. The shared runner in [`shared_runner`] is
/// never dropped (it lives in a `OnceLock`, like the shared instance) and is collected by
/// [`reap_stale`](crate::reap_stale) on the next run, which finds it by the same labels.
pub struct RunnerGuard<'a> {
    inst: &'a Instance,
    /// The name it registered under, unique per process.
    pub name: String,
    /// Gitea's id for it, as found in the admin listing.
    pub id: i64,
    /// The labels it registered with, scheme included (e.g. `gea-itest:host`).
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
            eprintln!("GEA_ITEST_KEEP set: leaving runner {} ({})", self.name, container.id());
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
    /// Start an act_runner registered against this instance, offering `labels`, and wait until
    /// Gitea lists it online.
    ///
    /// `labels` are act_runner label specs, e.g. [`HOST_LABEL`]. Prefer [`shared_runner`] unless
    /// the test needs a runner of its own — and then give it a label no other test asks for; the
    /// module docs explain what goes wrong otherwise.
    ///
    /// `Err` means **no runner is available here**, and is meant to become a skip (see
    /// [`runner_or_skip!`](crate::runner_or_skip)): the image could not be pulled, the container
    /// died, or it never came online. The message carries the runner's logs. A server that
    /// refuses to mint a registration token is not "unavailable" but broken, and panics.
    ///
    /// Only for an instance this harness started: the runner reaches Gitea at the container's own
    /// address, which an instance attached through `GEA_TEST_HOST` does not have.
    pub fn attach_runner(&self, labels: &[&str]) -> Result<RunnerGuard<'_>, String> {
        let Some(ip) = self.container_ip() else {
            return Err("this instance was not started by the harness (GEA_TEST_HOST), so there \
                        is no container address for a runner to reach it at; attach your own \
                        runner offering `gea-itest` to that instance instead"
                .to_owned());
        };
        if labels.is_empty() || labels.iter().any(|l| l.contains([',', ' '])) {
            // `run.sh` splices GITEA_RUNNER_LABELS into a command line unquoted.
            panic!("runner labels must be non-empty and free of commas and spaces: {labels:?}");
        }

        let (code, body) = self.api("POST", "admin/actions/runners/registration-token", None);
        assert!(
            (200..300).contains(&code),
            "could not mint an instance registration token: HTTP {code}: {body}"
        );
        let token = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["token"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("the registration-token reply names no token: {body}"));

        static N: AtomicU32 = AtomicU32::new(0);
        let name =
            format!("{LABEL}-runner-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let keep = std::env::var_os("GEA_ITEST_KEEP").is_some();
        let (image, tag) = split_image(&runner_image());
        let request = GenericImage::new(image, tag)
            .with_container_name(&name)
            .with_label(LABEL, "1")
            .with_label(PID_LABEL, std::process::id().to_string())
            .with_label(KEEP_LABEL, if keep { "1" } else { "" })
            .with_env_var("GITEA_INSTANCE_URL", format!("http://{ip}:{GITEA_PORT}"))
            .with_env_var("GITEA_RUNNER_REGISTRATION_TOKEN", token)
            .with_env_var("GITEA_RUNNER_NAME", &name)
            .with_env_var("GITEA_RUNNER_LABELS", labels.join(","))
            .with_env_var("CONFIG_FILE", "/config.yaml")
            .with_copy_to("/config.yaml", config_yaml().into_bytes());

        let started = Instant::now();
        let container = request.start().map_err(|e| {
            let daemon = daemon_unreachable().map(|w| format!(" ({w})")).unwrap_or_default();
            format!("could not start the runner container from {}: {e}{daemon}", runner_image())
        })?;

        let mut guard = RunnerGuard {
            inst: self,
            name,
            id: 0,
            labels: labels.iter().map(|s| (*s).to_owned()).collect(),
            container: Some(container),
            keep,
        };
        guard.id = self.wait_runner_online(&guard, started)?;
        Ok(guard)
    }

    /// Poll the admin runner listing until `guard` is there and not `offline`, returning its id.
    ///
    /// "Not offline" rather than "== online": the listing's status is derived from the runner's
    /// last contact and whether it is running a task, and a runner that came up and immediately
    /// claimed a job is every bit as available as one that is idle.
    fn wait_runner_online(&self, guard: &RunnerGuard<'_>, started: Instant) -> Result<i64, String> {
        let mut last = String::from("not yet listed");
        while started.elapsed() < ONLINE_TIMEOUT {
            let (code, body) = self.api("GET", "admin/actions/runners?limit=50", None);
            if (200..300).contains(&code) {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mine = v["runners"]
                    .as_array()
                    .and_then(|rs| rs.iter().find(|r| r["name"] == guard.name.as_str()));
                if let Some(r) = mine {
                    let status = r["status"].as_str().unwrap_or("?").to_owned();
                    if status != "offline"
                        && let Some(id) = r["id"].as_i64()
                    {
                        return Ok(id);
                    }
                    last = format!("listed, status {status}");
                }
            } else {
                last = format!("the admin runner listing answered HTTP {code}: {body}");
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
             reach Gitea at the container's own address; if its logs below show connection \
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
/// stdout, and **`GEA_ITEST_REQUIRE` turns it into a panic**, so `cargo xtask itest` cannot go
/// green over a suite whose runner never started. Put it before `cover!`.
#[macro_export]
macro_rules! runner_or_skip {
    ($inst:expr) => {
        match $crate::runner::shared_runner($inst) {
            Ok(r) => r,
            Err(reason) => {
                let msg = format!(
                    "no Actions runner available: {reason}\n\
                     Make the image {} pullable (or set GEA_ITEST_RUNNER_IMAGE), or run with \
                     --allow-skip to accept skipping the tests that need jobs to execute.",
                    $crate::runner::ACT_RUNNER_IMAGE
                );
                if ::std::env::var_os("GEA_ITEST_REQUIRE").is_some() {
                    panic!("GEA_ITEST_REQUIRE is set but {msg}");
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
/// let yaml = gea_itest::runner::workflow_yaml("[push]", &[("ok", "echo fine"), ("bad", "exit 1")]);
/// assert!(yaml.contains("runs-on: gea-itest"));
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
    format!("gea-marker-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// A shell line that prints `marker` without `marker` appearing in the line itself.
///
/// The job log carries each step's command text as well as its output (measured: the
/// `::group::Run echo ...` header and a `Wrote command ... to 'workflow/0.sh'` line), so
/// `echo MARKER` would put `MARKER` in the log even if the step never ran its echo. Printing it in
/// two halves means the joined marker can only be in the log because the shell printed it.
pub fn echo_unseen(marker: &str) -> String {
    let mid = marker.len() / 2;
    let (a, b) = marker.split_at(mid);
    format!("printf '%s%s\\n' '{a}' '{b}'")
}

impl TestRepo<'_> {
    /// Wait until at least `at_least` Actions runs exist for `sha` and every one of them is
    /// `completed`, returning them (the API's run objects, newest first).
    ///
    /// Through the raw API with `?head_sha=`, which — measured — finds `pull_request` runs too,
    /// where `?branch=` does not (they carry no `head_branch`). `at_least` is needed because runs
    /// are created asynchronously after a push: "all runs for this commit are complete" is
    /// vacuously true for the moment before the first one exists.
    ///
    /// `Err` on timeout describes what was seen, so the failure says whether the runs never
    /// appeared (the workflow did not trigger) or appeared and never finished (no runner took
    /// them).
    pub fn wait_for_runs_completed(
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
                if runs.len() >= at_least && runs.iter().all(|r| r["status"] == "completed") {
                    return Ok(runs);
                }
            } else {
                seen = format!("HTTP {code}: {body}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "after {timeout:?}, {} still did not have {at_least} completed run(s) for {sha}: \
             [{seen}]",
            self.slug()
        ))
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
        assert!(y.contains("  ok:\n    runs-on: gea-itest\n"), "{y}");
        assert!(y.contains("      - run: |\n          echo a\n          echo b\n"), "{y}");
        assert!(y.contains("  bad:\n"), "{y}");
        assert!(!y.contains(":host"), "runs-on names the label, not the spec: {y}");
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

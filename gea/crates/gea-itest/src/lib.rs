//! Harness for tests that run against a real Gitea.
//!
//! Unit tests elsewhere use `FakeTransport` and are hermetic. These tests exist for the one
//! thing mocks structurally cannot prove: **that the server agrees with the specification we
//! generated from**. A mock will happily confirm our own misreading of the spec.
//!
//! Two modes:
//!
//! - `GEA_TEST_HOST` + `GEA_TEST_TOKEN` set — run against that instance. Fast, and useful
//!   for pointing at a real server you control.
//! - Otherwise — boot a throwaway container, bootstrap an admin and a token, and tear it down.
//!
//! Tests skip (rather than fail) when neither is available, so `cargo test --workspace` works
//! on a machine without Docker.

// This crate is `publish = false`, so its rustdoc exists for contributors, who read it with
// `--document-private-items`. Module docs here deliberately link to the private helpers they
// describe — that is the useful thing to link to when explaining how a module works — and those
// links resolve under that flag. Suppressing the lint keeps the links navigable rather than
// demoting sixteen of them to inert code spans. The published crates (gitea-core, -model,
// -client) do NOT carry this allow: docs.rs renders no private items, so there a link to one is
// genuinely broken for the only audience that sees it.
#![allow(rustdoc::private_intra_doc_links)]

pub mod coverage;

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use testcontainers::bollard::Docker;
use testcontainers::bollard::query_parameters::{
    ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
};
use testcontainers::core::{ContainerPort, ExecCommand};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// The Gitea release these tests — and the vendored specification — target.
pub const GITEA_IMAGE: &str = "docker.io/gitea/gitea:1.27.3";

/// How long to wait for Gitea to answer before giving up and explaining why.
///
/// Warm, this is under a second; the headroom is for a first boot that also runs migrations.
/// It used to be 180s, and that number now has a second constraint on it: `.config/nextest.toml`
/// kills an `gea-itest` test at 180s, so a health wait of 180s would mean nextest reporting an
/// anonymous kill instead of [`Instance::wait_healthy`] reporting a named cause — trading the
/// better message for the worse one, which is the whole thing commit 5b2bf90 was about. 90s
/// leaves the other half of that budget to a cold runner's `docker pull`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(90);

/// The port Gitea listens on *inside* the container. The port it is reachable on from here is
/// whatever the daemon published it as, which is a different question — see [`Instance::boot`].
const GITEA_PORT: u16 = 3000;

/// The image to boot, honouring `GEA_ITEST_IMAGE` (set by `cargo xtask itest --image`).
///
/// Overriding it is how you find out whether a newer Gitea has diverged from the spec we
/// generated against — which is a question worth being able to ask cheaply.
fn image() -> String {
    std::env::var("GEA_ITEST_IMAGE").unwrap_or_else(|_| GITEA_IMAGE.to_owned())
}

/// Split `repo:tag` the way a registry does: on the last colon, but only if no `/` follows it,
/// so `localhost:5000/gitea` is not mistaken for a tag.
fn split_image(spec: &str) -> (String, String) {
    match spec.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name.to_owned(), tag.to_owned()),
        _ => (spec.to_owned(), "latest".to_owned()),
    }
}

/// Applied as a docker label so [`reap_stale`] can find our leftovers without touching anything
/// else on the machine.
const LABEL: &str = "gea-itest";

/// The pid of the process that started the container, so [`reap_stale`] can tell a corpse from
/// a container a *live* sibling run is still using.
const PID_LABEL: &str = "gea-itest.pid";

/// Set when `GEA_ITEST_KEEP` asked for the container to survive, so [`reap_stale`] leaves it
/// alone. Without this the flag would be nearly meaningless: the shared instance lives in a
/// `OnceLock` and never drops, so *every* container outlives its run — and the next run's reap
/// would then delete the one you kept before you had looked at it.
const KEEP_LABEL: &str = "gea-itest.keep";

const ADMIN_USER: &str = "geatest";
const ADMIN_PASS: &str = "gea-test-password-1";
const ADMIN_MAIL: &str = "geatest@example.invalid";

/// Why no instance could be obtained, recorded by [`Instance::boot`] so the skip message can
/// quote the real reason rather than re-deriving a plausible one.
static UNAVAILABLE: OnceLock<String> = OnceLock::new();

/// A Gitea instance under test, and how to reach it.
pub struct Instance {
    pub base_url: String,
    pub token: String,
    pub user: String,
    /// `None` when we attached to a pre-existing instance rather than starting one.
    container: Option<Container<GenericImage>>,
    /// Set when `GEA_ITEST_KEEP` asked us to leave the container running for inspection.
    keep: bool,
}

// Hand-written rather than derived: `Container`'s own `Debug` calls `ports()`, which is a
// round-trip to the daemon. A type that talks to Docker when you format it is a trap in a
// panic message, which is exactly where `Debug` gets used.
impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance")
            .field("base_url", &self.base_url)
            .field("user", &self.user)
            .field("container", &self.container.as_ref().map(|c| c.id().to_owned()))
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

impl Instance {
    /// Attach to `GEA_TEST_HOST` if configured, else boot a container. `Ok(None)` means
    /// neither is available and the caller should skip.
    pub fn acquire(extra_env: &[(&str, &str)]) -> Result<Option<Self>, String> {
        if let (Ok(host), Ok(token)) =
            (std::env::var("GEA_TEST_HOST"), std::env::var("GEA_TEST_TOKEN"))
        {
            let base_url = if host.starts_with("http") { host } else { format!("https://{host}") };
            let user = std::env::var("GEA_TEST_USER").unwrap_or_else(|_| ADMIN_USER.to_owned());
            return Ok(Some(Self { base_url, token, user, container: None, keep: true }));
        }
        Self::boot(extra_env)
    }

    /// Start a throwaway Gitea with `testcontainers`.
    ///
    /// # Why `testcontainers` and not `docker run`
    ///
    /// The previous version published a port and then assumed `localhost` was where it landed.
    /// It is not, always: `--publish` binds the port on the machine running the *Docker daemon*.
    /// When the tests themselves run inside a container with the daemon's socket mounted — the
    /// default for Gitea/Gitea Actions' `act_runner`, and for any docker-out-of-docker setup —
    /// the Gitea container is a *sibling*, the published port lands on the daemon's host, and
    /// `localhost:<port>` answers nothing while Gitea is perfectly healthy. That is the
    /// failure commit 5b2bf90 taught the harness to *describe*; this is the one meant to fix it.
    ///
    /// [`Container::get_host`] answers the question properly: a `tcp://` `DOCKER_HOST` gives its
    /// own host, and a unix socket gives `localhost` normally but the **bridge gateway** when
    /// `/.dockerenv` says we are ourselves inside a container — which is exactly the address a
    /// sibling reaches the daemon's published ports on.
    ///
    /// # Why the host port is chosen here rather than by `testcontainers`
    ///
    /// `testcontainers` will happily assign a random port and report it, which would let
    /// [`pick_port`] go. It stays for one reason: `GITEA__server__ROOT_URL` has to be in the
    /// environment *before* Gitea starts, and it has to contain the port. Choosing the port
    /// here is what lets the common case — a daemon on this machine, where `get_host()` says
    /// `localhost` — come out right on the first start.
    ///
    /// # Why this may start the container twice
    ///
    /// `ROOT_URL` is what Gitea builds `clone_url`, `html_url` and `ssh_url` out of, it is
    /// read once at startup, and it therefore has to be decided before there is a container to
    /// ask where it is reachable. Getting it wrong is not cosmetic: every URL the API hands back
    /// would name an address that does not resolve from here, and `gea repo clone` — or
    /// anything else that follows a server-supplied URL — would fail a long way from the cause.
    ///
    /// So this guesses `http://localhost:<port>/`, checks the guess against what
    /// [`Container::get_host`] actually says, and starts over with the real address when the two
    /// disagree. Patching the generated `app.ini` in place and restarting looks cheaper and is
    /// not: the image runs `environment-to-ini` on *every* start, so the second start rewrites
    /// `ROOT_URL` from the same environment variable and silently undoes the edit. The
    /// environment is the only thing Gitea will listen to, and it can only be set at creation.
    ///
    /// The second start is paid only where the guess is wrong, which is precisely the
    /// docker-out-of-docker and remote-daemon topologies this change exists for. On a developer's
    /// machine the guess is right and there is exactly one container, as before.
    fn boot(extra_env: &[(&str, &str)]) -> Result<Option<Self>, String> {
        reap_stale();
        let port = pick_port();
        let mut root_url = format!("http://localhost:{port}/");

        let mut container = match Self::start_container(port, &root_url, extra_env) {
            Ok(c) => c,
            Err(e) => {
                // Distinguish "there is no Docker here" (skip, so `cargo test --workspace` still
                // works on a laptop without one) from "Docker is here and this went wrong"
                // (fail, loudly). Asking the daemon directly is the only reliable test: a
                // missing daemon surfaces as a transport error buried several layers down, not
                // as a variant anything can match on.
                if let Some(why) = daemon_unreachable() {
                    let _ = UNAVAILABLE.set(why);
                    return Ok(None);
                }
                return Err(format!("could not start the Gitea container: {e}"));
            }
        };

        let mut base_url = reachable_url(&container)?;
        if format!("{base_url}/") != root_url {
            root_url = format!("{base_url}/");
            // Dropping removes it, synchronously, which also releases the published port before
            // the replacement asks for it again.
            drop(container);
            container = Self::start_container(port, &root_url, extra_env)
                .map_err(|e| format!("could not restart Gitea at {root_url}: {e}"))?;
            base_url = reachable_url(&container)?;
            if format!("{base_url}/") != root_url {
                return Err(format!(
                    "the container moved: ROOT_URL was set from {root_url}, and after \
                     restarting it is reachable at {base_url}/ instead. Gitea's idea of its \
                     own URL cannot be made to agree with ours, so every clone URL it hands \
                     back would name somewhere unreachable."
                ));
            }
        }

        // Filled in below rather than via `Self { token, ..inst }`: `Instance` implements
        // `Drop`, so functional-update syntax would try to move fields out of a type that
        // must be dropped as a whole (E0509).
        let mut inst = Self {
            base_url,
            token: String::new(),
            user: ADMIN_USER.to_owned(),
            container: Some(container),
            keep: std::env::var_os("GEA_ITEST_KEEP").is_some(),
        };

        inst.wait_healthy(HEALTH_TIMEOUT)?;
        let token = inst.bootstrap_admin()?;
        inst.token = token;
        Ok(Some(inst))
    }

    /// One attempt at a container, with `ROOT_URL` fixed at `root_url`.
    ///
    /// Gitea normally walks you through a web installer. `INSTALL_LOCK` skips it, and the rest
    /// of these pin the configuration the installer would otherwise ask for.
    ///
    /// Deliberately no `--rm` equivalent: `testcontainers` removes the container when the handle
    /// drops, not when the process inside it dies, so a Gitea that exits during startup still
    /// has its logs to hand. `start()` is given no readiness condition for the same reason — it
    /// cannot fail before we are holding a handle those logs can be read through, and the
    /// diagnosis in [`Instance::wait_healthy`] is better than any wait strategy's.
    ///
    /// Also deliberately no `GITEA__database__PATH`: the image's default
    /// (`/data/gitea/gitea.db`) sits in a directory the `git` user owns. Pointing it at
    /// `/data/gitea.db` instead fails with "unable to open database file", Gitea retries ten
    /// times and exits, and the whole thing looks like a timeout.
    fn start_container(
        port: u16,
        root_url: &str,
        extra_env: &[(&str, &str)],
    ) -> Result<Container<GenericImage>, testcontainers::TestcontainersError> {
        let keep = std::env::var_os("GEA_ITEST_KEEP").is_some();
        let (name, tag) = split_image(&image());
        // The port is already unique among live containers — nothing else can be bound to it —
        // which makes it a better discriminator here than a counter would be. Not just the pid:
        // pids are recycled, and a leftover holding the name would make the next run fail to
        // create rather than fail to reap.
        let container_name = format!("{LABEL}-{}-{port}", std::process::id());
        let mut request = GenericImage::new(name, tag)
            .with_exposed_port(ContainerPort::Tcp(GITEA_PORT))
            .with_mapped_port(port, ContainerPort::Tcp(GITEA_PORT))
            .with_container_name(container_name)
            .with_label(LABEL, "1")
            .with_label(PID_LABEL, std::process::id().to_string())
            .with_label(KEEP_LABEL, if keep { "1" } else { "" })
            .with_env_var("GITEA__security__INSTALL_LOCK", "true")
            .with_env_var("GITEA__database__DB_TYPE", "sqlite3")
            .with_env_var("GITEA__server__ROOT_URL", root_url)
            .with_env_var("GITEA__server__OFFLINE_MODE", "true")
            .with_env_var("GITEA__service__DISABLE_REGISTRATION", "true")
            // Actions ON, though no runner ever attaches.
            //
            // This used to be `false`, with the note "nothing here needs a runner, and it
            // shortens startup". The first half stopped being true: with the unit disabled
            // Gitea does not merely refuse to *run* anything, it stops routing — every path
            // under `/repos/{owner}/{repo}/actions/` answers 404, and `PATCH` with
            // `has_actions: true` returns 200 while leaving the field `false`. So four
            // operations were unreachable for a reason that had nothing to do with runners.
            //
            // Enabling it costs nothing here: `POST .../dispatches` returns 201 and the run is
            // born `queued` and stays there forever with no runner, which is exactly the
            // fixture the run lifecycle needs.
            .with_env_var("GITEA__actions__ENABLED", "true")
            // Let a migration name this instance as its source.
            //
            // `[migrations] ALLOW_LOCALNETWORKS` defaults to false, which refuses a clone URL
            // pointing at the container itself ("You can not import from disallowed hosts").
            // That made `repoMigrate` untestable without reaching the real internet — and with
            // it `repoMirrorSync`, which needs a pull mirror that only a migration can create. Migrating a repository on this instance to itself needs no
            // network at all, which is the cheapest of the available answers.
            .with_env_var("GITEA__migrations__ALLOW_LOCALNETWORKS", "true");

        // Settings only one caller wants, folded in last so they can also override the above.
        for (key, value) in extra_env {
            request = request.with_env_var(*key, *value);
        }

        // Deliberately no `with_startup_timeout`: it bounds a *readiness condition*, and
        // there are none here, so setting it would promise a guarantee it does not give.
        // [`Instance::wait_healthy`] is the clock, and it is the one with the diagnosis.
        request.start()
    }

    /// Poll `/api/healthz` until Gitea reports itself up. Typically two seconds; the generous
    /// timeout is for a cold cache where first boot also runs migrations.
    ///
    /// # Why this is not [`testcontainers`]' `HttpWaitStrategy`
    ///
    /// It could be, and the diff would be smaller. But a wait strategy that times out says
    /// "container startup timeout" and nothing else, and this function exists because that kind
    /// of message cost a real CI run three minutes and an afternoon of misreading. Keeping the
    /// loop keeps the diagnosis.
    ///
    /// # Why this reports *how* it failed
    ///
    /// The first version ran `curl -fsS` with both streams sent to `/dev/null` and treated the
    /// result as a bool, so four unrelated failures — curl absent, connection refused, a read
    /// timeout, and a non-2xx status — all arrived as the same "did not become healthy within
    /// 180s", followed by a dump of Gitea's own logs. Those logs say `Starting new Web server`,
    /// because Gitea is fine; the reader is then left to conclude the web server is crashing,
    /// which it is not. That is the same "never swallow the real reason" failure this project
    /// criticises `tea` for, committed by our own test harness.
    ///
    /// So each probe now keeps curl's exit code, and the error names what was tried and what
    /// each attempt said.
    ///
    /// # Why the container's own address is probed too
    ///
    /// `base_url` now comes from [`Container::get_host`] rather than being assumed to be
    /// `localhost`, which should mean the sibling-container case simply works. "Should" is not
    /// "does": `get_host` decides via `/.dockerenv`, which podman does not create, and it reads
    /// the gateway of the `bridge` network, which is not necessarily the one this process is on.
    /// If that resolution ever comes out wrong the symptom is identical to a hang, so the
    /// diagnosis stays: probing the container's own address distinguishes "Gitea is down" from
    /// "Gitea is up and we are looking in the wrong place", and says which.
    fn wait_healthy(&self, timeout: Duration) -> Result<(), String> {
        let published = format!("{}/api/healthz", self.base_url);
        let start = Instant::now();
        let mut last = Probe::Unattempted;
        while start.elapsed() < timeout {
            last = probe(&published);
            if last == Probe::Ok {
                return Ok(());
            }
            // If Gitea gave up (it retries the database ten times, then exits), waiting out
            // the full timeout tells the user nothing. Fail immediately with the logs, which
            // name the actual cause.
            if !self.is_running() {
                return Err(format!(
                    "the Gitea container exited during startup after {:?}. Logs:\n{}",
                    start.elapsed(),
                    self.logs()
                ));
            }
            // curl missing is not going to fix itself, and waiting three minutes to say so
            // would be the exact unhelpfulness this function was rewritten to avoid.
            if last == Probe::CurlMissing {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        // Second question: is it Gitea, or is it the route to Gitea?
        let direct = self
            .container_ip()
            .map(|ip| (ip.clone(), probe(&format!("http://{ip}:{GITEA_PORT}/api/healthz"))));
        let mut msg = format!(
            "Gitea did not become healthy within {timeout:?}.\n\
             \n  probed {published}\n    -> {last}\n"
        );
        match &direct {
            Some((ip, Probe::Ok)) => msg.push_str(&format!(
                "  probed http://{ip}:{GITEA_PORT}/api/healthz (the container's own address)\n    ->                  answered\n\
                 \nGitea is UP. What failed is the route to it: {} is not reachable from this \n\
                 process, even though testcontainers resolved it as where the published port \n\
                 landed. That resolution reads /.dockerenv and the `bridge` network's gateway, \n\
                 so it comes out wrong under podman (no /.dockerenv) or when this process sits \n\
                 on a different user-defined network.\n\
                 \nwhat to do:\n\
                 1. set DOCKER_HOST to a tcp:// address the daemon answers on, which \n\
                    testcontainers will then use verbatim, or\n\
                 2. put this process and the Gitea container on the same Docker network, or\n\
                 3. point the suite at an instance you already have:\n\
                    GEA_TEST_HOST=git.example.org GEA_TEST_TOKEN=... cargo xtask itest\n",
                self.base_url
            )),
            Some((ip, p)) => msg.push_str(&format!(
                "  probed http://{ip}:{GITEA_PORT}/api/healthz (the container's own address)\n    -> {p}\n\
                 \nNeither address answered, so this is Gitea rather than the route. Its logs \n\
                 follow.\n\nlogs:\n{}",
                self.logs()
            )),
            None => msg.push_str(&format!(
                "  the container's own address could not be read, so it is not known whether \n\
                 Gitea is up or merely unreachable. Its logs follow.\n\nlogs:\n{}",
                self.logs()
            )),
        }
        Err(msg)
    }

    /// The container's address on its Docker network, for the sibling-container check above.
    fn container_ip(&self) -> Option<String> {
        self.container.as_ref()?.get_bridge_ip_address().ok().map(|ip| ip.to_string())
    }

    fn is_running(&self) -> bool {
        let Some(container) = &self.container else { return true };
        container.is_running().unwrap_or(false)
    }

    /// Create an admin and mint a token. `generate-access-token --raw` prints the bare token,
    /// which is the only time Gitea will ever show it.
    fn bootstrap_admin(&self) -> Result<String, String> {
        self.exec(&[
            "gitea",
            "admin",
            "user",
            "create",
            "--admin",
            "--username",
            ADMIN_USER,
            "--password",
            ADMIN_PASS,
            "--email",
            ADMIN_MAIL,
            "--must-change-password=false",
        ])?;

        let raw = self.exec(&[
            "gitea",
            "admin",
            "user",
            "generate-access-token",
            "--username",
            ADMIN_USER,
            "--scopes",
            "all",
            "--raw",
        ])?;

        let token = raw.trim().lines().last().unwrap_or_default().trim().to_owned();
        if token.is_empty() {
            return Err(format!("no token in output: {raw:?}"));
        }
        Ok(token)
    }

    /// Run something in the container as the `git` user.
    ///
    /// Gitea's data is owned by `git` inside the image and its CLI refuses to run as root.
    /// `docker exec --user` has no equivalent on [`ExecCommand`], so the image's own `su-exec`
    /// does the dropping instead.
    fn exec(&self, argv: &[&str]) -> Result<String, String> {
        let Some(container) = &self.container else {
            return Err("cannot exec against an instance we did not start".to_owned());
        };
        let cmd = ["su-exec", "git"].iter().chain(argv).map(|s| s.to_string());
        let mut res = container
            .exec(ExecCommand::new(cmd))
            .map_err(|e| format!("exec {argv:?} failed: {e}"))?;
        // Draining stdout blocks until the process exits, so the exit code is only meaningful
        // afterwards. Both streams are tiny for everything run here.
        let out = String::from_utf8_lossy(
            &res.stdout_to_vec().map_err(|e| format!("reading stdout of {argv:?}: {e}"))?,
        )
        .into_owned();
        let err = String::from_utf8_lossy(
            &res.stderr_to_vec().map_err(|e| format!("reading stderr of {argv:?}: {e}"))?,
        )
        .into_owned();
        match res.exit_code() {
            Ok(Some(0)) | Ok(None) => Ok(out),
            Ok(Some(code)) => Err(format!("{argv:?} exited {code}: {}{}", out.trim(), err.trim())),
            Err(e) => Err(format!("could not read the exit code of {argv:?}: {e}")),
        }
    }

    pub fn logs(&self) -> String {
        let Some(container) = &self.container else { return String::new() };
        let out = container.stdout_to_vec().unwrap_or_default();
        let err = container.stderr_to_vec().unwrap_or_default();
        let all = format!("{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
        // `docker logs --tail 60` had no equivalent here, and the whole log of a failed start is
        // mostly migration chatter that pushes the interesting last lines off the screen.
        let lines: Vec<&str> = all.lines().collect();
        lines[lines.len().saturating_sub(60)..].join("\n")
    }

    /// The address this instance reaches **itself** at, from inside its own container.
    ///
    /// Not the same as [`Instance::base_url`], and the difference is the whole point.
    /// `base_url` is the published port on the *host*; a process inside the container cannot
    /// reach it, so a clone URL built from it fails with
    /// `fatal: unable to access ... Connection refused` a long way from the cause.
    ///
    /// The one caller that needs this is migration: `POST /repos/migrate` makes **Gitea**
    /// clone the address, so the URL has to make sense where Gitea is standing. Measured:
    /// with `base_url` the migration is a 422, and with this it is a 201.
    pub fn internal_url(&self) -> String {
        format!("http://localhost:{GITEA_PORT}")
    }

    /// Register a runner the way `act_runner register` does, without running one.
    ///
    /// Gitea has no REST route that creates a runner: the REST API only mints a *registration token*
    /// (`POST {scope}/actions/runners/registration-token`), and the runner itself redeems it over the
    /// Connect-RPC service at `/api/actions`. Connect speaks JSON as well as protobuf, so the redeeming
    /// half is one `curl` too — and nothing has to stay connected, which is what makes the listing
    /// testable. The runner is then registered but has never reported in, so it lists as `offline`.
    ///
    /// `scope` is the REST prefix the token is minted under, e.g. `repos/o/r` or `admin`. Returns the
    /// runner's id.
    pub fn register_runner(&self, scope: &str, name: &str, labels: &[&str]) -> i64 {
        let (code, body) =
            self.api("POST", &format!("{scope}/actions/runners/registration-token"), None);
        assert!(
            (200..300).contains(&code),
            "could not mint a registration token: HTTP {code}: {body}"
        );
        let token =
            serde_json::from_str::<serde_json::Value>(&body).expect("a token object")["token"]
                .as_str()
                .expect("the registration token")
                .to_owned();
        self.redeem_runner_token(&token, name, labels)
    }

    /// The second half of [`Instance::register_runner`], for a caller that minted the token
    /// itself — through `gea`, say, which is how the per-scope tests cover the minting route.
    pub fn redeem_runner_token(&self, token: &str, name: &str, labels: &[&str]) -> i64 {
        let request =
            serde_json::json!({"name": name, "token": token, "labels": labels}).to_string();
        let root = self.api_base();
        let root = root.trim_end_matches("/api/v1");
        let out = std::process::Command::new("curl")
            .args(["-sS", "--fail-with-body", "--max-time", "30", "-X", "POST"])
            .args(["-H", "Content-Type: application/json", "-H", "Connect-Protocol-Version: 1"])
            .args(["-d", &request])
            .arg(format!("{root}/api/actions/runner.v1.RunnerService/Register"))
            .output()
            .expect("curl should run");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "the runner service refused the registration: {text}");
        let reply: serde_json::Value = serde_json::from_str(&text).expect("a Connect JSON reply");
        // protojson renders an int64 as a string.
        let id = &reply["runner"]["id"];
        id.as_i64()
            .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| panic!("the registration reply names no runner id: {reply}"))
    }

    /// A whole runner lifecycle at one scope, through `gea raw` only: mint a registration token,
    /// redeem it, find the runner in the listing and by id, disable it and find it through the
    /// server's own `disabled` filter, then delete it.
    ///
    /// Gitea gives the repository, organization and user scopes the same five routes under
    /// different names — `get-repo-runner`, `get-org-runner`, `get-user-runner` — so the three
    /// tests that drive them share this rather than three copies drifting apart. `group` is the
    /// `gea raw` group, `noun` the scope word in the command names, and `target` the positional
    /// arguments that name the scope (`[owner, repo]`, `[org]`, or nothing for the user).
    pub fn drive_runner_lifecycle(&self, group: &str, noun: &str, target: &[&str]) {
        let raw = |cmd: &str, extra: &[&str]| {
            let mut argv: Vec<String> = vec!["raw".into(), group.into(), cmd.into()];
            argv.extend(target.iter().map(|s| (*s).to_owned()));
            argv.extend(extra.iter().map(|s| (*s).to_owned()));
            self.gea(&argv)
        };

        let minted = raw("create-runner-registration-token", &[]);
        minted.assert_ok(&format!("gea raw {group} create-runner-registration-token"));
        // Deliberately not echoed into a failure message, even for a throwaway container: a test
        // that prints credentials teaches the habit of printing credentials.
        let token = minted.json()["token"].as_str().unwrap_or_default().to_owned();
        assert!(!token.is_empty(), "the registration token came back empty");

        let name = self.unique_repo_name(&format!("{noun}-runner"));
        let id = self.redeem_runner_token(&token, &name, &["docker"]).to_string();

        let listed = raw(&format!("get-{noun}-runners"), &[]);
        listed.assert_ok(&format!("gea raw {group} get-{noun}-runners"));
        let listed = listed.json();
        let names: Vec<&str> = listed["runners"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| r["name"].as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&name.as_str()), "{name} is not listed at its own scope: {listed}");

        let got = raw(&format!("get-{noun}-runner"), &[&id]);
        got.assert_ok(&format!("gea raw {group} get-{noun}-runner"));
        let got = got.json();
        assert_eq!(got["name"], name.as_str(), "the id addressed another runner: {got}");
        assert_eq!(got["status"], "offline", "a runner that never checked in: {got}");
        assert_eq!(got["disabled"], false, "a new runner starts enabled: {got}");

        let edited = raw(&format!("update-{noun}-runner"), &[&id, "--disabled=true"]);
        edited.assert_ok(&format!("gea raw {group} update-{noun}-runner --disabled true"));
        assert_eq!(edited.json()["disabled"], true, "the edit did not stick: {}", edited.stdout);
        let disabled = raw(&format!("get-{noun}-runners"), &["--disabled=true"]);
        disabled.assert_ok(&format!("gea raw {group} get-{noun}-runners --disabled true"));
        assert!(
            disabled.stdout.contains(&name),
            "the server's `disabled` filter lost a disabled runner: {}",
            disabled.stdout
        );

        raw(&format!("delete-{noun}-runner"), &[&id])
            .assert_ok(&format!("gea raw {group} delete-{noun}-runner"));
        let gone = raw(&format!("get-{noun}-runner"), &[&id]);
        assert!(!gone.ok(), "the runner survived its delete:\n{}\n{}", gone.stdout, gone.stderr);
    }

    pub fn api_base(&self) -> String {
        format!("{}/api/v1", self.base_url)
    }

    /// The admin's web password, when this harness created the account.
    ///
    /// `None` when we attached to a pre-existing instance through `GEA_TEST_HOST`, because then
    /// the account is somebody else's and we only ever had a token for it.
    ///
    /// Exists for one test: the OAuth browser login, which is the only thing in the suite that
    /// needs a *web session* rather than an API token. Everything else authenticates with
    /// [`Instance::token`], and should keep doing so — a password is not a better token.
    pub fn web_password(&self) -> Option<&'static str> {
        self.container.as_ref().map(|_| ADMIN_PASS)
    }

    /// A repository name unique within this process, so tests can run in parallel against one
    /// instance without colliding.
    pub fn unique_repo_name(&self, prefix: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!("{prefix}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let Some(container) = self.container.take() else { return };
        if self.keep {
            eprintln!(
                "GEA_ITEST_KEEP set: leaving {} running at {}",
                container.id(),
                self.base_url
            );
            // `Container`'s own `Drop` removes it, so keeping it means never running that.
            std::mem::forget(container);
            return;
        }
        drop(container);
    }
}

/// Talk to the daemon the same way `testcontainers` does, for the two things its API does not
/// expose: asking whether there is a daemon at all, and reaping.
///
/// `testcontainers` re-exports `bollard` precisely so this does not need a second dependency or
/// a second opinion about where the socket is. The runtime is built and thrown away per call:
/// both callers run once per process, at boot, before anything else is happening.
fn on_docker<F, T>(f: impl FnOnce(Docker) -> F) -> Result<T, String>
where
    F: Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not build a runtime to talk to Docker: {e}"))?;
    let docker =
        Docker::connect_with_defaults().map_err(|e| format!("could not reach Docker: {e}"))?;
    Ok(rt.block_on(f(docker)))
}

/// `Some(reason)` when there is no daemon to talk to — the condition that makes these tests
/// skip rather than fail. `None` means the daemon answered, so whatever went wrong was real.
///
/// This replaces the old `docker version` shell-out, and with it the `$DOCKER` escape hatch:
/// the harness no longer runs a CLI at all, so naming a different binary cannot redirect it.
/// `DOCKER_HOST` is the knob now, which is the one `bollard` and `testcontainers` both read.
fn daemon_unreachable() -> Option<String> {
    let host = std::env::var("DOCKER_HOST").unwrap_or_else(|_| "the default socket".to_owned());
    let hint = if std::env::var_os("DOCKER").is_some() {
        "\n  note: $DOCKER is set, but it no longer selects the engine — the harness speaks to \
         the daemon over its API rather than through a CLI, so set DOCKER_HOST instead \
         (podman: DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podman/podman.sock)"
    } else {
        ""
    };
    match on_docker(|d| async move { d.version().await.map(|_| ()).map_err(|e| e.to_string()) }) {
        Err(e) => Some(format!("Docker at {host} could not be addressed ({e}){hint}")),
        Ok(Err(e)) => Some(format!("Docker at {host} did not answer ({e}){hint}")),
        Ok(Ok(())) => None,
    }
}

/// Remove containers left behind by an earlier run.
///
/// `testcontainers` removes a container when its handle drops, and — unlike the Java original —
/// the Rust implementation ships **no Ryuk sidecar** at version 0.28: there is no reaper
/// container anywhere in the crate, only that `Drop` and an opt-in `watchdog` feature that
/// catches SIGINT/SIGTERM. The good news is that nothing here needs a privileged sidecar, so
/// restricted CI that forbids one is not a problem. The bad news is that the handle is the only
/// thing standing between a container and immortality, and it is not enough: the shared instance
/// in [`shared`] lives in a `OnceLock`, which never drops, and nothing drops at all when a test
/// process is killed.
///
/// So reaping by label survives the rewrite. What changes is *which* containers it takes, and
/// that part is load-bearing rather than tidy. The previous version swept every labelled
/// container unconditionally. That is correct under `cargo test`, which runs one test binary at
/// a time, and destructive under `cargo nextest`, which runs every test in its own process:
/// each new boot deleted the server the already-running tests were talking to — measured at
/// 11+ live containers and 8 failures in a suite that is green under `cargo test`.
///
/// A container is therefore reaped only when its owner is provably gone: the pid that started
/// it no longer exists, or it is old enough that no run could still be using it. The pid test is
/// the one that keeps a dev box clean between runs; the age test is the backstop for platforms
/// without `/proc` and for a pid that has since been recycled. A container started under
/// `GEA_ITEST_KEEP` is never reaped at all — see [`KEEP_LABEL`].
fn reap_stale() {
    /// Long enough that no live run is ever caught, short enough to self-heal between runs.
    const STALE_AFTER: i64 = 600;
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - STALE_AFTER)
        .unwrap_or(0);

    let _ = on_docker(|docker| async move {
        // `HashMap` is disallowed workspace-wide because generator output has to be
        // deterministic. This is not generator output: it is the exact type `bollard`'s
        // `filters()` takes, and a `BTreeMap` would not compile.
        #[allow(clippy::disallowed_types)]
        let filters =
            std::collections::HashMap::from([("label".to_owned(), vec![format!("{LABEL}=1")])]);
        let opts = ListContainersOptionsBuilder::new().all(true).filters(&filters).build();
        let Ok(found) = docker.list_containers(Some(opts)).await else { return };
        for c in found {
            let Some(id) = c.id else { continue };
            let owner = c
                .labels
                .as_ref()
                .and_then(|l| l.get(PID_LABEL))
                .and_then(|p| p.parse::<u32>().ok());
            let kept =
                c.labels.as_ref().is_some_and(|l| l.get(KEEP_LABEL).is_some_and(|v| v == "1"));
            let too_old = c.created.is_some_and(|created| created <= cutoff);
            if kept || (!too_old && owner.is_none_or(owner_may_still_be_running)) {
                continue;
            }
            let opts = RemoveContainerOptionsBuilder::new().force(true).v(true).build();
            let _ = docker.remove_container(&id, Some(opts)).await;
        }
    });
}

/// Whether the process that started a container could still be using it.
///
/// Deliberately answers "yes" when it cannot tell — reaping a container out from under a running
/// test is a confusing, silent failure, and leaving one behind for the age rule to collect is
/// not. `/proc` makes this exact on Linux, which is where this runs; elsewhere only the age rule
/// applies. A recycled pid reads as alive, which again only delays a collection.
fn owner_may_still_be_running(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// One instance shared by every test in a binary.
///
/// Booting per test is correct but wasteful: each boot is a container start plus a couple of
/// seconds of Gitea initialization, and tests isolate themselves with
/// [`Instance::unique_repo_name`] rather than by needing a fresh server.
///
/// The `OnceLock` means `Drop` never runs for this instance, so the container outlives the test
/// process. That is why [`reap_stale`] exists — the next run cleans up, and the container is
/// labelled so nothing else is touched.
pub fn shared() -> Result<Option<&'static Instance>, String> {
    static SHARED: OnceLock<Result<Option<Instance>, String>> = OnceLock::new();
    match SHARED.get_or_init(|| Instance::acquire(&[])) {
        Ok(Some(i)) => Ok(Some(i)),
        Ok(None) => Ok(None),
        Err(e) => Err(e.clone()),
    }
}

/// What a single health probe found, kept distinct so the failure can name itself.
///
/// curl's exit codes are the source: 7 is "could not connect", 28 is a timeout, 22 is `-f`
/// refusing a non-2xx status, and a spawn failure means curl is not installed at all. Collapsing
/// these into a bool is what made a perfectly healthy Gitea look like a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    Ok,
    Unattempted,
    CurlMissing,
    Refused,
    TimedOut,
    HttpError,
    Other(i32),
}

impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "answered"),
            Self::Unattempted => write!(f, "not attempted"),
            Self::CurlMissing => {
                write!(f, "`curl` is not installed here, so the health check could never succeed")
            }
            Self::Refused => write!(f, "nothing is listening (connection refused)"),
            Self::TimedOut => write!(f, "connected but no reply within 3s"),
            Self::HttpError => write!(f, "replied, but not with a 2xx"),
            Self::Other(c) => write!(f, "curl exited {c}"),
        }
    }
}

/// One health probe. Keeps curl's exit code instead of discarding it.
fn probe(url: &str) -> Probe {
    match Command::new("curl")
        .args(["-fsS", "--max-time", "3", "-o", "/dev/null", url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        // `curl` absent: ENOENT from the spawn itself, not an exit code.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Probe::CurlMissing,
        Err(_) => Probe::Other(-1),
        Ok(st) if st.success() => Probe::Ok,
        Ok(st) => match st.code() {
            Some(7) => Probe::Refused,
            Some(28) => Probe::TimedOut,
            Some(22) => Probe::HttpError,
            Some(127) => Probe::CurlMissing,
            Some(c) => Probe::Other(c),
            None => Probe::Other(-1),
        },
    }
}

/// Where a started container is actually reachable from this process.
///
/// The two questions the old harness never asked, answered by the library rather than assumed:
/// which address reaches the daemon's published ports, and which port did this one land on.
fn reachable_url(container: &Container<GenericImage>) -> Result<String, String> {
    let host = container
        .get_host()
        .map_err(|e| format!("could not work out where the container is reachable: {e}"))?
        .to_string();
    let port = container
        .get_host_port_ipv4(ContainerPort::Tcp(GITEA_PORT))
        .map_err(|e| format!("could not work out which port {GITEA_PORT} was published on: {e}"))?;
    // A bare IPv6 address needs brackets before it is a URL authority.
    if host.contains(':') {
        Ok(format!("http://[{host}]:{port}"))
    } else {
        Ok(format!("http://{host}:{port}"))
    }
}

/// A free-ish TCP port. Binding to 0 and immediately dropping leaves a small race window, but
/// the alternative — letting `testcontainers` assign one — leaves `ROOT_URL` unknowable until
/// after Gitea has already read it. See [`Instance::boot`].
fn pick_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(3000)
}

/// Why no instance could be obtained, for the skip message.
pub fn unavailable_reason() -> String {
    UNAVAILABLE.get().cloned().unwrap_or_else(|| {
        daemon_unreachable()
            .unwrap_or_else(|| "Docker answered, so this should not have been reached".to_owned())
    })
}
/// Skip the test when no instance is available, or fail if the caller demanded one.
///
/// Skipping keeps `cargo test --workspace` working on a machine without Docker. But a skip
/// that looks like a pass is how integration coverage silently disappears — a green CI run
/// that tested nothing is worse than a red one. So:
///
/// - The skip is announced on **stdout** as well as stderr. `cargo test` captures per-test
///   output unless `--nocapture`, and a skip notice that only a developer running with
///   `--nocapture` can see is a skip notice nobody reads.
/// - **`GEA_ITEST_REQUIRE=1` turns a skip into a failure.** CI sets it. That is the switch
///   that makes "we have integration tests" a checkable claim rather than an aspiration.
#[macro_export]
macro_rules! instance_or_skip {
    () => {
        match $crate::shared() {
            Ok(Some(i)) => i,
            Ok(None) => {
                let reason = $crate::unavailable_reason();
                let msg = format!(
                    "no Gitea instance available: {reason}.\n\
                     Set GEA_TEST_HOST + GEA_TEST_TOKEN to use an existing instance, or make \
                     Docker usable so a throwaway one can be started."
                );
                if ::std::env::var_os("GEA_ITEST_REQUIRE").is_some() {
                    panic!("GEA_ITEST_REQUIRE is set but {msg}");
                }
                println!("SKIPPED ({}): {msg}", ::std::stringify!($crate));
                eprintln!("SKIPPED: {msg}");
                return;
            }
            Err(e) => panic!("could not obtain a Gitea instance: {e}"),
        }
    };
}

// ---------------------------------------------------------------------------
// Driving `gea` against the instance
// ---------------------------------------------------------------------------

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The `gea` executable built alongside these tests.
///
/// `env!("CARGO_BIN_EXE_…")` only works inside the package that declares the binary, and
/// `gea-itest` is deliberately a separate package, so the path is derived from the test
/// executable instead: integration-test binaries live in `target/<profile>/deps/`, which puts
/// `gea` two directories up.
pub fn gea_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary has a path");
    let mut dir = exe.parent().expect("…/deps/<test>").to_path_buf();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let bin = dir.join(if cfg!(windows) { "gea.exe" } else { "gea" });
    assert!(
        bin.exists(),
        "the gea binary is missing at {}. Run `cargo xtask itest`, which builds it first, \
         or `cargo build -p gea`.",
        bin.display()
    );
    bin
}

/// A configuration directory that is guaranteed to hold nothing.
///
/// Every `gea` invocation from these tests points `XDG_CONFIG_HOME` here. The isolation is the
/// point: a developer's real `hosts.toml` must never be read (a test that picked up a live login
/// could act on a production instance) and must never be written.
fn scratch_config() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("gea-itest-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a scratch config directory");
        d
    })
    .as_path()
}

/// What a finished `gea` run produced.
#[derive(Debug)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Assert success, reporting both streams when it failed — an integration failure is
    /// expensive to reproduce, so the first report has to carry everything.
    #[track_caller]
    pub fn assert_ok(&self, what: &str) -> &Self {
        assert!(
            self.ok(),
            "{what} should have succeeded but exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code,
            self.stdout,
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn assert_code(&self, want: i32, what: &str) -> &Self {
        assert_eq!(
            self.code,
            Some(want),
            "{what} should have exited {want}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    /// Assert the combined output mentions `needle`, quoting both streams when it does not.
    #[track_caller]
    pub fn assert_says(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle) || self.stderr.contains(needle),
            "expected the output to mention {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    /// Parse stdout as JSON, failing with the raw text when it is not JSON.
    #[track_caller]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout was not JSON ({e})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

impl Instance {
    /// Run `gea` against this instance, authenticated, with an empty configuration directory.
    ///
    /// Credentials arrive through `GEA_HOST`/`GEA_TOKEN` rather than `gea auth login`, so a
    /// test exercising some other command does not also depend on the login flow, and nothing is
    /// written to disk that a later test could inherit.
    pub fn gea<I, S>(&self, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.gea_in(Path::new("."), args)
    }

    /// As [`Instance::gea`], but with a working directory — for the commands that read git.
    pub fn gea_in<I, S>(&self, cwd: &Path, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.gea_env(cwd, &[], args)
    }

    /// The environment every `gea` child gets: where to point, who to be, and enough
    /// determinism that output is comparable.
    ///
    /// This exists because the same mistake was made four times. `GEA_HOST` was being handed a
    /// scheme-stripped `host:port`, leaving gea to guess — and `config::hosts::scheme_for`
    /// guesses https for anything that is not loopback. Locally the container resolves to
    /// `localhost`, so the guess is right and the defect invisible; under docker-out-of-docker it
    /// resolves to the bridge gateway and every invocation speaks TLS at a plain-HTTP server.
    ///
    /// A local test run cannot catch that, which is the whole problem: the bug is only reachable
    /// from a topology developers do not have. So the fix is not vigilance, it is having one
    /// definition. A test below asserts the host carries its scheme.
    pub fn child_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("XDG_CONFIG_HOME", scratch_config().to_string_lossy().into_owned()),
            // WITH the scheme. See above.
            ("GEA_HOST", self.base_url.clone()),
            ("GEA_TOKEN", self.token.clone()),
            ("GEA_CREDENTIAL_STORE", "env".to_owned()),
            // Deterministic output: no colour, no pager, no terminal-width guessing.
            ("NO_COLOR", "1".to_owned()),
            ("GEA_PAGER", "cat".to_owned()),
        ]
    }

    /// The variables that must NOT reach a child, so a developer's real login cannot be used.
    pub const HOSTILE_ENV: &'static [&'static str] =
        &["GITEA_HOST", "GITEA_TOKEN", "GEA_REPO", "GITEA_REPO"];

    /// As [`Instance::gea_in`], but with extra environment for this one invocation.
    ///
    /// Per-invocation rather than per-process because the alternative — `std::env::set_var` —
    /// is `unsafe` under the 2024 edition and, worse, would leak into every other test sharing
    /// this process. `GEA_FORCE_TTY` is the reason it exists: some output is deliberately
    /// terminal-only (padded columns, the truncation banner), so a test that asserts on it has
    /// to ask for terminal rendering rather than read piped output and call the difference a bug.
    pub fn gea_env<I, S>(&self, cwd: &Path, env: &[(&str, &str)], args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        // Pass base_url WITH its scheme. Stripping it made gea guess, and gea guesses https
        // for anything that is not loopback (config::hosts::scheme_for) — correct for a real
        // instance, wrong for this one. Locally that was invisible because the container
        // resolves to "localhost"; under docker-out-of-docker it resolves to the bridge
        // gateway (172.17.0.1), so every invocation attempted TLS against a plain-HTTP server
        // and failed with rustls' "received corrupt message of type InvalidContentType".
        // The harness knows the scheme for a fact. Throwing that away to re-derive it from a
        // heuristic is how a test suite ends up depending on where it happens to be running.
        let host = self.base_url.as_str();
        let mut cmd = Command::new(gea_bin());
        cmd.current_dir(cwd)
            .args(args)
            .env("XDG_CONFIG_HOME", scratch_config())
            .env("GEA_HOST", host)
            .env("GEA_TOKEN", &self.token)
            .env("GEA_CREDENTIAL_STORE", "env")
            // Deterministic output: no colour, no pager, no terminal-width guessing.
            .env("NO_COLOR", "1")
            .env("GEA_PAGER", "cat")
            .env_remove("GITEA_HOST")
            .env_remove("GITEA_TOKEN")
            .env_remove("GEA_REPO")
            .env_remove("GITEA_REPO");
        // The caller's environment is applied LAST, so it can override any of the above.
        // It used to be applied first, which made this silently a no-op for exactly the
        // variables a test would most want to vary — `GEA_HOST` and `GEA_TOKEN` — since the
        // defaults below then overwrote them. A helper whose documented job is "extra
        // environment for this one invocation" must not quietly drop half of what it is given.
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("the gea binary should be executable");
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// A direct API call, made with `curl` rather than our own HTTP stack.
    ///
    /// Out-of-band on purpose: an assertion about what `gea` did to the server is worthless if
    /// it is checked through the same code that may have got it wrong.
    pub fn api(&self, method: &str, path: &str, body: Option<&str>) -> (i32, String) {
        self.api_as(&self.token, method, path, body)
    }

    /// As [`Instance::api`], but as somebody other than the admin.
    ///
    /// Exists for the one thing the admin token cannot do: be a *second* party. Gitea does not
    /// notify you about your own actions, so a test that needs an inbox has to have somebody
    /// else fill it — see `porcelain.rs`'s `--mark-read` test, which is otherwise a test of an
    /// empty list.
    pub fn api_as(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (i32, String) {
        let mut c = Command::new("curl");
        c.args(["-sS", "-w", "\n%{http_code}", "--max-time", "30"])
            .args(["-X", method])
            .args(["-H", &format!("Authorization: token {token}")])
            .args(["-H", "Content-Type: application/json"]);
        if let Some(b) = body {
            c.args(["-d", b]);
        }
        let out = c
            .arg(format!("{}/{}", self.api_base(), path.trim_start_matches('/')))
            .output()
            .expect("curl should run");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = text.lines().collect();
        let code = lines.pop().unwrap_or("0").trim().parse().unwrap_or(0);
        (code, lines.join("\n"))
    }

    /// The response headers for a GET, so a test can assert on `Link` and `X-Total-Count`.
    pub fn api_headers(&self, path: &str) -> String {
        let out = Command::new("curl")
            .args(["-sS", "-D", "-", "-o", "/dev/null", "--max-time", "30"])
            .args(["-H", &format!("Authorization: token {}", self.token)])
            .arg(format!("{}/{}", self.api_base(), path.trim_start_matches('/')))
            .output()
            .expect("curl should run");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// A repository that deletes itself.
///
/// Tests share one instance (see [`shared`]) so they can run in parallel, which only works if
/// each owns a distinctly named repository and cleans it up. `Drop` rather than an explicit call
/// so that a panicking assertion still tears down — otherwise the first failure leaves debris
/// that the next run trips over.
pub struct TestRepo<'a> {
    inst: &'a Instance,
    pub owner: String,
    pub name: String,
}

impl<'a> TestRepo<'a> {
    /// Create an empty private repository with a name unique to this process.
    pub fn create(inst: &'a Instance, prefix: &str) -> Self {
        Self::create_with(inst, prefix, r#""private":true"#)
    }

    /// Create one that already has a `README.md`, so it has a default branch and can take a
    /// pull request without a separate seeding step.
    pub fn create_initialized(inst: &'a Instance, prefix: &str) -> Self {
        Self::create_with(
            inst,
            prefix,
            r#""private":true,"auto_init":true,"default_branch":"main""#,
        )
    }

    fn create_with(inst: &'a Instance, prefix: &str, extra: &str) -> Self {
        let name = inst.unique_repo_name(prefix);
        let (code, body) =
            inst.api("POST", "user/repos", Some(&format!(r#"{{"name":"{name}",{extra}}}"#)));
        assert!(
            (200..300).contains(&code),
            "could not create the test repository {name}: HTTP {code}: {body}"
        );
        Self { inst, owner: inst.user.clone(), name }
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// `-R owner/name`, which is how every test addresses its repository.
    pub fn flag(&self) -> [String; 2] {
        ["-R".to_owned(), self.slug()]
    }

    pub fn api(&self, method: &str, path: &str, body: Option<&str>) -> (i32, String) {
        self.inst.api(
            method,
            &format!("repos/{}/{}", self.slug(), path.trim_start_matches('/')),
            body,
        )
    }

    /// Merge a pull request out of band, waiting out Gitea's mergeability check.
    ///
    /// A pull request's mergeability is computed asynchronously after it is opened, and a merge
    /// asked for before that finishes answers `405 Please try again later` — on a busy container
    /// often enough to make any test that merges straight after opening flaky. So a 405 is
    /// retried for a bounded while; any other answer is returned as it came.
    pub fn merge_pull(&self, number: u64, style: &str) -> (i32, String) {
        let body = format!(r#"{{"do":"{style}"}}"#);
        let path = format!("pulls/{number}/merge");
        let mut answer = self.api("POST", &path, Some(&body));
        for _ in 0..40 {
            if answer.0 != 405 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            answer = self.api("POST", &path, Some(&body));
        }
        answer
    }

    /// Clone the repository into `dir` with credentials in the remote URL, and configure an
    /// identity, so `git commit` and `git push` work unattended.
    pub fn clone_to(&self, dir: &Path) {
        let url = self.push_url();
        git(
            dir.parent().unwrap_or(Path::new(".")),
            &["clone", "--quiet", &url, &dir.to_string_lossy()],
        );
        git(dir, &["config", "user.email", "geatest@example.invalid"]);
        git(dir, &["config", "user.name", "gea integration test"]);
    }

    /// The HTTP remote with the token embedded. Test-only: a token in a remote URL is fine for a
    /// throwaway container and wrong everywhere else.
    pub fn push_url(&self) -> String {
        let bare = self.inst.base_url.trim_start_matches("http://").trim_start_matches("https://");
        format!("http://{}:{}@{}/{}.git", self.inst.user, self.inst.token, bare, self.slug())
    }
}

impl Drop for TestRepo<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api("DELETE", &format!("repos/{}", self.slug()), None);
    }
}

/// Run `git`, panicking with both streams on failure.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        // Never let a developer's global hooks, signing key or template dir change the result.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HOME", std::env::temp_dir())
        .output()
        .unwrap_or_else(|e| panic!("could not run git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Commit a file and push the branch it is on.
pub fn commit_and_push(dir: &Path, branch: &str, file: &str, contents: &str, message: &str) {
    git(dir, &["checkout", "--quiet", "-B", branch]);
    std::fs::write(dir.join(file), contents).expect("write a file in the clone");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", message]);
    git(dir, &["push", "--quiet", "--set-upstream", "origin", branch]);
}

/// A second account with a deliberately narrow token, for the tests that need a real 403.
///
/// Minted over the API with Basic auth rather than `gitea admin user generate-access-token`,
/// so it also works against an instance we merely attached to and cannot `docker exec` into.
/// Gitea fixes a token's scopes at creation, which is what makes this the only way to get an
/// under-privileged credential.
pub struct ScopedUser {
    pub name: String,
    pub token: String,
}

impl Instance {
    /// Create a user and mint a token carrying exactly `scopes`.
    pub fn scoped_user(&self, prefix: &str, scopes: &[&str]) -> Result<ScopedUser, String> {
        let name = format!("{prefix}{}", std::process::id());
        let pass = "gea-itest-scoped-1";
        let (code, body) = self.api(
            "POST",
            "admin/users",
            Some(&format!(
                r#"{{"username":"{name}","email":"{name}@example.invalid","password":"{pass}","must_change_password":false}}"#
            )),
        );
        if !(200..300).contains(&code) && code != 422 {
            return Err(format!("could not create {name}: HTTP {code}: {body}"));
        }

        let list = scopes.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(",");
        let out = Command::new("curl")
            .args(["-sS", "-w", "\n%{http_code}", "--max-time", "30"])
            .args(["-u", &format!("{name}:{pass}")])
            .args(["-X", "POST", "-H", "Content-Type: application/json"])
            .args(["-d", &format!(r#"{{"name":"itest","scopes":[{list}]}}"#)])
            .arg(format!("{}/users/{name}/tokens", self.api_base()))
            .output()
            .map_err(|e| format!("curl: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = text.lines().collect();
        let code: i32 = lines.pop().unwrap_or("0").trim().parse().unwrap_or(0);
        let body = lines.join("\n");
        if !(200..300).contains(&code) {
            return Err(format!("could not mint a token for {name}: HTTP {code}: {body}"));
        }
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("token reply was not JSON: {e}"))?;
        let token = v["sha1"].as_str().ok_or_else(|| format!("no sha1 in {body}"))?.to_owned();
        Ok(ScopedUser { name, token })
    }

    /// Run `gea` as somebody other than the admin.
    pub fn gea_as<I, S>(&self, token: &str, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        // Scheme included, for the reason spelled out in `gea_env`.
        let host = self.base_url.as_str();
        let out = Command::new(gea_bin())
            .args(args)
            .env("XDG_CONFIG_HOME", scratch_config())
            .env("GEA_HOST", host)
            .env("GEA_TOKEN", token)
            .env("GEA_CREDENTIAL_STORE", "env")
            .env("NO_COLOR", "1")
            .env("GEA_PAGER", "cat")
            .env_remove("GITEA_HOST")
            .env_remove("GITEA_TOKEN")
            .output()
            .expect("the gea binary should be executable");
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// The bug this exists to prevent cost two CI round trips and could not be reproduced
    /// locally, because it only appears when the instance is NOT on loopback.
    ///
    /// `GEA_HOST` was handed a scheme-stripped `host:port`, so gea fell back to guessing, and
    /// it guesses https for anything that is not loopback. On a developer's machine the
    /// container resolves to `localhost`, the guess is right, and everything passes. Under
    /// docker-out-of-docker it resolves to the bridge gateway and every invocation attempts TLS
    /// against a plain-HTTP server.
    ///
    /// Asserting the scheme is present catches it here, on any machine, with no Docker.
    #[test]
    fn the_child_environment_tells_gea_the_scheme_instead_of_making_it_guess() {
        let inst = Instance {
            base_url: "http://172.17.0.1:39683".to_owned(),
            token: "t".to_owned(),
            user: "u".to_owned(),
            container: None,
            keep: true,
        };
        let env = inst.child_env();
        let host = env
            .iter()
            .find(|(k, _)| *k == "GEA_HOST")
            .map(|(_, v)| v.clone())
            .expect("GEA_HOST must be set");
        assert!(
            host.starts_with("http://") || host.starts_with("https://"),
            "GEA_HOST must carry its scheme, or gea guesses https for any non-loopback \
             host and speaks TLS at a plain-HTTP server: {host}"
        );
        assert_eq!(host, inst.base_url, "the host must be base_url verbatim");
    }

    /// A developer's real credentials must not be reachable from a test.
    #[test]
    fn the_hostile_variables_are_all_named() {
        for k in Instance::HOSTILE_ENV {
            assert!(k.starts_with("GEA_") || k.starts_with("GITEA_"), "unexpected: {k}");
        }
        assert!(Instance::HOSTILE_ENV.contains(&"GITEA_TOKEN"));
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// A registry that carries a port looks exactly like a tag to anything that splits on the
    /// first — or the last — colon without checking what follows. `GEA_ITEST_IMAGE` is how
    /// somebody points these tests at a mirror, and a local mirror is the most likely place to
    /// see `host:5000/...`, so getting this wrong would break the one override that exists.
    #[test]
    fn an_image_from_a_registry_with_a_port_is_not_mistaken_for_a_tag() {
        assert_eq!(
            split_image("docker.io/gitea/gitea:1.27.3"),
            ("docker.io/gitea/gitea".to_owned(), "1.27.3".to_owned())
        );
        assert_eq!(
            split_image("localhost:5000/gitea/gitea"),
            ("localhost:5000/gitea/gitea".to_owned(), "latest".to_owned())
        );
        assert_eq!(
            split_image("localhost:5000/gitea/gitea:1.27.3"),
            ("localhost:5000/gitea/gitea".to_owned(), "1.27.3".to_owned())
        );
        assert_eq!(split_image("gitea"), ("gitea".to_owned(), "latest".to_owned()));
    }

    /// The distinction the harness lost: a port nothing listens on must report *refused*, not a
    /// generic failure. This is what separates "Gitea is down" from "Gitea is unreachable
    /// from here", and it is the whole reason a 180-second timeout used to be unexplainable.
    ///
    /// Binding and dropping a listener yields a port that is almost certainly free, which is
    /// exactly the condition being tested; no Docker and no network are involved.
    #[test]
    fn a_port_with_nothing_on_it_is_reported_as_refused_not_as_a_bare_failure() {
        let port = pick_port();
        let got = probe(&format!("http://127.0.0.1:{port}/api/healthz"));
        assert!(
            matches!(got, Probe::Refused | Probe::CurlMissing),
            "expected a named cause, got {got:?}"
        );
        assert_ne!(got, Probe::Ok);
    }

    /// Every variant must say something a reader can act on; `Display` is what the failure
    /// message is built from, so an empty or duplicated rendering would put us back where we
    /// started.
    #[test]
    fn every_probe_outcome_explains_itself() {
        let all = [
            Probe::Ok,
            Probe::Unattempted,
            Probe::CurlMissing,
            Probe::Refused,
            Probe::TimedOut,
            Probe::HttpError,
            Probe::Other(42),
        ];
        let mut seen: Vec<String> = Vec::new();
        for p in all {
            let s = p.to_string();
            assert!(!s.trim().is_empty(), "{p:?} renders as nothing");
            assert!(!seen.contains(&s), "{p:?} renders the same as an earlier variant: {s}");
            seen.push(s);
        }
    }
}

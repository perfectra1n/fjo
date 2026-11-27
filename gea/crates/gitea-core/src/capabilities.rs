//! What this instance can do — discovered, never inferred from a version number.
//!
//! # Feature-detect; never parse version strings
//!
//! This is the rule the module exists to enforce. `tea` gated behaviour on the instance's
//! version string, and it went wrong in every way that approach can: a Gitea fork reporting a
//! Gitea version, a Gitea version numbered past the Gitea range, `+dev` suffixes on
//! self-built instances, and reverse proxies serving a version endpoint from a different
//! deployment. The result was a `--no-version-check` flag — a switch that turns off the safety
//! feature, which every user eventually has to set, which means the feature was never load-bearing
//! in the first place.
//!
//! So: [`Capabilities`] carries *numbers the instance reported about itself* and nothing derived
//! from a version. [`Instance::version`] exists and is displayed in error messages ("this
//! instance is gitea 9.0.1, and this endpoint was added later") because naming the version is
//! useful **to a human**. There is deliberately no version comparison API here for code to
//! branch on. If you find yourself wanting one, the right move is to attempt the request and
//! classify the `404` as [`crate::ErrorKind::RouteNotFound`].
//!
//! # Both endpoints are optional
//!
//! `GET /settings/api` requires no scope on a current Gitea but did not always exist, can be
//! disabled, and returns `403` on instances that require authentication for everything.
//! `GET /version` is unauthenticated and always routed, but a proxy may hide it. (`/nodeinfo`,
//! which carries a software name, is served only when the instance enables federation — off by
//! default on Gitea — so it would leave the instance label empty on nearly every server.)
//! Either or both being absent is normal, and must degrade to [`Capabilities::conservative`]
//! rather than to an error — a CLI that cannot list issues because an optional metadata endpoint
//! is missing is a broken CLI.

use std::time::Duration;

use serde::Deserialize;

use crate::http::{Client, Request};

/// How long a probe is trusted. Instance settings change when an admin edits `app.ini` and
/// restarts, which is not something a single CLI invocation — or a day of them — needs to
/// notice.
pub const TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Conservative defaults, matching Gitea's own shipped values.
///
/// These are the numbers to fall back to when `/settings/api` is unavailable. They are the
/// upstream defaults precisely so that a fallback behaves like an unconfigured instance rather
/// than like an optimistic guess: guessing *high* on `max_response_items` would send a `limit`
/// the server clamps, which is the ambiguity [`crate::http::paginate`] exists to survive.
pub const DEFAULT_MAX_RESPONSE_ITEMS: u32 = 50;
pub const DEFAULT_PAGING_NUM: u32 = 30;
pub const DEFAULT_GIT_TREES_PER_PAGE: u32 = 1000;
pub const DEFAULT_MAX_BLOB_SIZE: u64 = 10_485_760;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The hard ceiling the instance applies to any `limit` query parameter. Requests above it
    /// are clamped **silently** — no warning, no header, no error.
    pub max_response_items: u32,
    /// The page size used when `limit` is unset.
    pub default_paging_num: u32,
    pub default_git_trees_per_page: u32,
    pub default_max_blob_size: u64,
    /// Whether `/settings/api` actually answered.
    ///
    /// Load-bearing, not merely informational: pagination sends a `limit` only when this is
    /// true. Fields above are upstream defaults when it is false, and defaults are a guess.
    pub settings_known: bool,
    pub instance: Option<Instance>,
}

/// Instance identity from `/version`. **Display only** — see the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `gitea`, or `forgejo` for a Forgejo server (see [`software_of`]).
    pub software: String,
    pub version: String,
}

impl Instance {
    /// `gitea 1.27.3`, for an error message. Never parsed back.
    pub fn label(&self) -> String {
        if self.version.is_empty() {
            self.software.clone()
        } else {
            format!("{} {}", self.software, self.version)
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

impl Capabilities {
    /// What to assume when we could not ask.
    pub fn conservative() -> Self {
        Self {
            max_response_items: DEFAULT_MAX_RESPONSE_ITEMS,
            default_paging_num: DEFAULT_PAGING_NUM,
            default_git_trees_per_page: DEFAULT_GIT_TREES_PER_PAGE,
            default_max_blob_size: DEFAULT_MAX_BLOB_SIZE,
            settings_known: false,
            instance: None,
        }
    }

    /// The instance label for a diagnostic, or `None` when `/version` did not answer.
    pub fn instance_label(&self) -> Option<String> {
        self.instance.as_ref().map(Instance::label)
    }
}

// ------------------------------------------------------------------------------- wire shapes

/// `GET /settings/api`. Every field is `#[serde(default)]` so a newer or older instance that
/// omits one still yields usable capabilities instead of a decode error.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GeneralApiSettings {
    max_response_items: u32,
    default_paging_num: u32,
    default_git_trees_per_page: u32,
    default_max_blob_size: u64,
}

/// `GET /version`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ServerVersion {
    version: String,
}

/// Which software answered, from the only place `/version` says so.
///
/// Forgejo reports its own version with the Gitea API level it tracks appended as build
/// metadata — `11.0.1+gitea-1.22.0` — and Gitea never appends anything of the kind. That suffix
/// is a label for a human reading an error, which is all this is used for.
pub fn software_of(version: &str) -> &'static str {
    if version.contains("+gitea-") { "forgejo" } else { "gitea" }
}

/// Probe both endpoints, tolerating either being absent.
///
/// Errors are swallowed **by design** and this is the one place in the crate where that is
/// correct: the caller asked "what can this instance do", and "I could not find out" is a
/// complete, actionable answer expressed as [`Capabilities::conservative`]. Propagating the
/// error would turn an optional metadata endpoint into a hard dependency for every command.
///
/// # The two requests overlap, and `join!` is not `try_join!`
///
/// The endpoints are independent, so sending `/version` only after `/settings/api` answers
/// charges every process a serial round trip before its first paginated call — over a WAN to a
/// self-hosted instance that is the dominant cost of a short command. [`futures::join!`] drives
/// both to completion; [`futures::try_join!`] would cancel the survivor the moment the other
/// returned an error, and **either endpoint being absent is normal** (see the module header), so
/// a `404` on `/settings/api` would take `/version`'s instance label down with it and empty the
/// "this instance is gitea 1.21.0" line out of every error message on old instances.
pub(crate) async fn probe(client: &Client) -> Capabilities {
    let mut caps = Capabilities::conservative();

    let (settings, version) = futures::join!(
        client.json::<GeneralApiSettings>(Request::get("/settings/api")),
        client.json::<ServerVersion>(Request::get("/version")),
    );

    if let Ok(s) = settings {
        // A zero means the instance sent the field with no value, or sent a shape we mis-read.
        // Keep the upstream default rather than adopting a zero, which would make
        // `effective_limit` ask for `limit=0` and return nothing at all.
        if s.max_response_items > 0 {
            caps.max_response_items = s.max_response_items;
        }
        if s.default_paging_num > 0 {
            caps.default_paging_num = s.default_paging_num;
        }
        if s.default_git_trees_per_page > 0 {
            caps.default_git_trees_per_page = s.default_git_trees_per_page;
        }
        if s.default_max_blob_size > 0 {
            caps.default_max_blob_size = s.default_max_blob_size;
        }
        caps.settings_known = true;
    }

    if let Ok(v) = version
        && !v.version.is_empty()
    {
        caps.instance =
            Some(Instance { software: software_of(&v.version).to_owned(), version: v.version });
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use crate::http::auth::Auth;
    use crate::http::transport::{Canned, FakeTransport, HttpRequest, Response, Transport};
    use futures::future::BoxFuture;
    use http::Method;
    use std::sync::Arc;

    const SETTINGS: &str = r#"{
        "max_response_items": 50,
        "default_paging_num": 30,
        "default_git_trees_per_page": 1000,
        "default_max_blob_size": 10485760
    }"#;

    const VERSION: &str = r#"{"version": "1.27.3"}"#;

    fn client(t: FakeTransport) -> Client {
        Client::builder("https://git.example.org", Auth::None)
            .transport(Arc::new(t))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn both_endpoints_answering_yields_full_capabilities() {
        let c = client(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/version", Canned::json(200, VERSION)),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(caps.settings_known);
        assert_eq!(caps.max_response_items, 50);
        assert_eq!(caps.instance_label().as_deref(), Some("gitea 1.27.3"));
    }

    /// Bug this prevents: every error on a stock Gitea saying "instance: unknown". The probe used
    /// to read `/nodeinfo`, which Gitea routes only with `[federation] ENABLED` — off by default —
    /// so the label was empty on nearly every server. `/version` is always there.
    #[tokio::test]
    async fn a_gitea_without_federation_is_still_labelled_and_a_forgejo_is_told_apart() {
        let gitea = client(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/nodeinfo", Canned::html(404, "404 page not found"))
                .on(Method::GET, "/api/v1/version", Canned::json(200, VERSION)),
        );
        let caps = gitea.capabilities().await.unwrap();
        assert_eq!(caps.instance_label().as_deref(), Some("gitea 1.27.3"));

        let forgejo = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/version",
            Canned::json(200, r#"{"version":"11.0.1+gitea-1.22.0"}"#),
        ));
        let caps = forgejo.capabilities().await.unwrap();
        assert_eq!(caps.instance_label().as_deref(), Some("forgejo 11.0.1+gitea-1.22.0"));
    }

    /// An older instance, an instance with the endpoint disabled, or one behind a proxy that
    /// hides it. This must not break a single command.
    #[tokio::test]
    async fn both_endpoints_absent_falls_back_to_conservative_defaults() {
        let c = client(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::html(404, "<html>404</html>"))
                .on(Method::GET, "/api/v1/version", Canned::html(404, "<html>404</html>")),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(!caps.settings_known, "we must know that we do not know");
        assert_eq!(caps.max_response_items, DEFAULT_MAX_RESPONSE_ITEMS);
        assert_eq!(caps.instance, None);
    }

    /// An instance that requires authentication for everything answers `403` here. Same
    /// treatment: fall back, do not fail.
    #[tokio::test]
    async fn an_unauthorized_settings_endpoint_is_not_an_error() {
        let c = client(
            FakeTransport::new()
                .on(
                    Method::GET,
                    "/api/v1/settings/api",
                    Canned::json(403, r#"{"message":"token required"}"#),
                )
                .on(Method::GET, "/api/v1/version", Canned::json(200, VERSION)),
        );
        let caps = c.capabilities().await.unwrap();
        assert!(!caps.settings_known);
        assert_eq!(caps.instance_label().as_deref(), Some("gitea 1.27.3"));
    }

    /// `max_response_items: 0` would make `effective_limit` request `limit=0` and return
    /// nothing at all — a total-data-loss bug wearing a successful exit code.
    #[tokio::test]
    async fn a_zero_valued_field_keeps_the_upstream_default() {
        let c = client(
            FakeTransport::new()
                .on(
                    Method::GET,
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":0}"#),
                )
                .on(Method::GET, "/api/v1/version", Canned::new(404)),
        );
        let caps = c.capabilities().await.unwrap();
        assert_eq!(caps.max_response_items, DEFAULT_MAX_RESPONSE_ITEMS);
        assert!(caps.settings_known, "the endpoint did answer");
    }

    /// The probe must happen once per client, not once per call — otherwise every paginated
    /// stream adds two round trips.
    #[tokio::test]
    async fn capabilities_are_cached_per_client() {
        let t = Arc::new(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/version", Canned::json(200, VERSION)),
        );
        let c = Client::builder("https://git.example.org", Auth::None)
            .transport(t.clone())
            .build()
            .unwrap();
        for _ in 0..5 {
            c.capabilities().await.unwrap();
        }
        assert_eq!(t.call_count(), 2, "one probe of each endpoint, cached thereafter");
    }

    /// A transport that suspends mid-request, wrapping a [`FakeTransport`].
    ///
    /// **Do not simplify this back to a plain `FakeTransport`** — doing so silently disarms the
    /// test below. `FakeTransport` resolves without ever returning `Pending`, so under `join!`
    /// the first branch runs its entire probe, cache fill included, during the very first poll,
    /// and every later branch then finds a warm cache. Such a test reports "probed once"
    /// whether or not the single-flight gate exists: it is exactly the blind spot that let
    /// duplicate probing survive in a suite already full of call-count assertions. One yield is
    /// enough to reproduce what a real socket does — leave the winner suspended while the cache
    /// is still empty, so the losers are polled into the same cold miss.
    struct Yielding(Arc<FakeTransport>);

    impl Transport for Yielding {
        fn execute(&self, req: HttpRequest) -> BoxFuture<'_, Result<Response>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.0.execute(req).await
            })
        }
    }

    /// `gea status` fans out into four concurrent [`crate::http::paginate`] walks, and each one
    /// asks for capabilities before its first request. Without a single-flight gate all four
    /// miss the cold cache and all four probe, spending eight requests on what two answer — a
    /// cost that grows with every command that learns to overlap its reads.
    #[tokio::test]
    async fn concurrent_first_calls_probe_once_not_once_each() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(Method::GET, "/api/v1/settings/api", Canned::json(200, SETTINGS))
                .on(Method::GET, "/api/v1/version", Canned::json(200, VERSION)),
        );
        let c = Client::builder("https://git.example.org", Auth::None)
            .transport(Arc::new(Yielding(fake.clone())))
            .build()
            .unwrap();

        let (a, b, d, e) =
            futures::join!(c.capabilities(), c.capabilities(), c.capabilities(), c.capabilities());
        for caps in [a, b, d, e] {
            assert_eq!(caps.unwrap().max_response_items, 50, "every caller gets the real answer");
        }
        assert_eq!(
            fake.call_count(),
            2,
            "four concurrent callers must share one probe, not run one probe each"
        );
    }
}

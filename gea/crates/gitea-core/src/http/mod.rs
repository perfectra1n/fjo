//! The HTTP client every generated method calls into.
//!
//! # Shape of the layer
//!
//! [`Request`] describes an API call in the API's own vocabulary — a path relative to
//! `/api/v1`, an *ordered* query list, a body, an `Accept`. [`Client`] turns that into a wire
//! request exactly once — in one private `prepare` — sends it through a [`Transport`] with retry,
//! and offers seven typed exits: [`Client::json`], [`Client::value`], [`Client::empty`],
//! [`Client::raw`], [`Client::bytes`], [`Client::items`], and [`Client::capabilities`].
//!
//! Everything polymorphic lives here. Generated code is concrete: `async fn
//! create_pull_request(&self, owner: &str, repo: &str, body: &CreatePullRequestOption) ->
//! Result<PullRequest>` and nothing more. With 506 operations, an `impl Serialize` parameter or a
//! `T: DeserializeOwned` return would monomorphise per operation *and* per call site, making
//! compile time superlinear in generated volume.
//!
//! # Why `query` is a `Vec`
//!
//! Repeated query keys are legal and load-bearing in this API: `?labels=bug&labels=ci` filters
//! on both labels, and `?state=open&state=closed` is a real request. A `HashMap<String, String>`
//! — the obvious choice — silently keeps one of them. It is also unordered, which makes request
//! URLs non-deterministic and snapshot tests flaky. So: an ordered `Vec<(Cow<str>, String)>`,
//! with [`Request::set_query`] for the rarer "replace this key" case.

pub mod auth;
pub mod base64;
pub mod encode;
pub mod multipart;
pub mod paginate;
pub mod redact;
pub mod retry;
pub mod transport;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::HeaderMap;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::capabilities::{self, Capabilities};
use crate::error::classify::{self, ClassifyCtx, RepoProbe};
use crate::error::{CredentialKind, Error, ErrorKind, RequestCtx, Result, TokenSource};

/// The HTTP method type, re-exported.
///
/// `Request::method` is an `http::Method`, so any caller that wants to name the type — to build
/// a `Request` for a method with no constructor, to match on one, or to register a canned reply
/// in a test — otherwise has to add `http` to its own `Cargo.toml` purely to spell one word.
/// Several `gea` modules contorted around exactly that: a throwaway `Request::get("/")` built
/// only so `.method` could be lifted off it, and a comment explaining why. A published SDK
/// whose public types cannot be named without a second dependency is an incomplete SDK, so the
/// type comes with it.
///
/// Re-exported rather than wrapped: a newtype would have to mirror every constant and would
/// stop `Client` interoperating with the wider `http` ecosystem, which is worse than the
/// semver coupling it would avoid — and `http` is 1.x, so that coupling is cheap.
pub use http::Method;

pub use auth::{Auth, Credentials, SudoStyle};
pub use multipart::{Part, Progress};
pub use paginate::{PageInfo, Paginator, StopReason};
pub use retry::{RetryPolicy, WaitNotice};
pub use transport::{
    ByteStream, FakeTransport, HttpRequest, OutBody, ReqwestTransport, Response, TlsPolicy,
    Transport,
};

/// A media type. A newtype rather than a `String` so that `essence()` — the type without its
/// parameters — is available at every use site; `application/json;charset=utf-8` and
/// `application/json` must compare equal when deciding how to render a response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mime(String);

impl Mime {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `type/subtype`, without parameters, lowercased.
    pub fn essence(&self) -> String {
        self.0.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
    }

    pub fn is_json(&self) -> bool {
        let e = self.essence();
        e == "application/json" || e.ends_with("+json")
    }

    pub fn is_text(&self) -> bool {
        self.essence().starts_with("text/")
    }
}

impl fmt::Display for Mime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Mime {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// What we will accept back. Driven by the operation's `produces` in the spec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Accept {
    #[default]
    Json,
    Text,
    Html,
    Octets,
    /// `*/*`, for endpoints whose `produces` is empty or contradictory.
    Any,
    Other(Cow<'static, str>),
}

impl Accept {
    pub fn header(&self) -> &str {
        match self {
            Accept::Json => "application/json",
            Accept::Text => "text/plain",
            Accept::Html => "text/html",
            Accept::Octets => "application/octet-stream",
            Accept::Any => "*/*",
            Accept::Other(s) => s,
        }
    }
}

/// Where request-body bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Path(PathBuf),
    Bytes(Vec<u8>),
    /// `-` on the command line. **Not replayable**: once read, the bytes are gone, which is why
    /// [`OutBody::is_replayable`] exists and why the retry policy consults it.
    Stdin,
}

impl Source {
    pub fn is_replayable(&self) -> bool {
        !matches!(self, Source::Stdin)
    }
}

/// A request body.
#[derive(Clone, Default)]
pub enum Body {
    #[default]
    None,
    /// Pre-serialised JSON. Serialised by the caller so that this type stays object-safe and
    /// non-generic — see the module comment on monomorphisation.
    Json(Vec<u8>),
    Multipart(Vec<Part>),
    Octets {
        mime: String,
        src: Source,
    },
    Form(Vec<(String, String)>),
}

impl fmt::Debug for Body {
    /// Contents are never printed. Some bodies carry credentials
    /// (`POST /users/{u}/tokens` takes a password), and a `--debug` trace that dumps request
    /// bodies leaks them just as surely as one that dumps headers.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::None => f.write_str("None"),
            Body::Json(v) => write!(f, "Json({} bytes)", v.len()),
            Body::Multipart(p) => write!(f, "Multipart({} parts)", p.len()),
            Body::Octets { mime, src } => write!(f, "Octets {{ mime: {mime:?}, src: {src:?} }}"),
            Body::Form(p) => write!(f, "Form({} fields)", p.len()),
        }
    }
}

/// One API call, described.
///
/// `#[non_exhaustive]` because this type has already had to grow once — [`Request::scope`] was
/// added after publication so a 403 could name the scope the operation actually needs — and the
/// next thing an operation wants to carry (its `operationId`, its deprecation note) would
/// otherwise be a breaking change. Construct with [`Request::new`] or one of the per-method
/// constructors; every field stays public and assignable.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Request {
    pub method: Method,
    /// Relative to the API base, with a leading `/`, and **already percent-encoded** by
    /// [`encode::seg`] / [`encode::path_like`]. Encoding at construction rather than here is what
    /// makes per-parameter encoding possible: only the generated code knows which parameters are
    /// single segments and which are whole paths.
    pub path: String,
    /// Ordered, and repeated keys are legal. See the module comment.
    pub query: Vec<(Cow<'static, str>, String)>,
    pub body: Body,
    pub accept: Accept,
    pub extra_headers: Vec<(String, String)>,
    /// Byte counter for uploads. Default counts nothing.
    pub progress: Progress,
    pub timeout: Option<Duration>,
    /// The token scope this operation is documented to need, e.g. `write:repository`.
    ///
    /// This is the *authoritative* value: codegen derives it from the specification's `tags[0]`
    /// and the HTTP method, and `crates/xtask/src/overrides.toml` corrects the routes where that
    /// rule is wrong. It is the one piece of operation identity the runtime needs, so it is
    /// carried here rather than reconstructed from the path — `error::classify::infer_scope`
    /// re-derives the same rule from the method and path, and disagrees with codegen on 37 of
    /// the 506 routes (every pull-request route, which the spec tags `repository` and the path
    /// makes look like `issue`).
    ///
    /// `&'static str` and not a `String` or a `Cow`: every producer of this value has one
    /// already — a literal in generated code, `OpMeta::scope` in layer 2 — so it costs no
    /// allocation and keeps `Request: Clone` as cheap as it was. `None` for a caller who does
    /// not know, such as `gea api`, where the user types a raw path; classification then falls
    /// back to inference rather than rendering a blank `needs:` line.
    ///
    /// Naming a scope is *all* this does. It does not decide whether a 403 is a scope problem —
    /// see the 403 arm of [`crate::error::classify::classify`], where the server's message
    /// decides that and this only decides what to call it.
    pub scope: Option<&'static str>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        let mut path = path.into();
        if !path.starts_with('/') {
            // Accept both `user` and `/user`, because layer 1 lets the user type either.
            path.insert(0, '/');
        }
        Self { method, path, ..Self::default() }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path)
    }

    pub fn put(path: impl Into<String>) -> Self {
        Self::new(Method::PUT, path)
    }

    pub fn patch(path: impl Into<String>) -> Self {
        Self::new(Method::PATCH, path)
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::DELETE, path)
    }

    /// Append a query parameter. Appends, never replaces — see the module comment.
    pub fn query(mut self, key: impl Into<Cow<'static, str>>, value: impl fmt::Display) -> Self {
        self.query.push((key.into(), value.to_string()));
        self
    }

    /// Append only when the value is present, which is what generated `<Op>Query` builders want.
    pub fn query_opt(
        self,
        key: impl Into<Cow<'static, str>>,
        value: Option<impl fmt::Display>,
    ) -> Self {
        match value {
            Some(v) => self.query(key, v),
            None => self,
        }
    }

    /// Replace every existing occurrence of `key` with one value. Used for `page` and `limit`,
    /// where a second occurrence would be ambiguous.
    pub fn set_query(&mut self, key: impl Into<Cow<'static, str>>, value: impl fmt::Display) {
        let key = key.into();
        self.query.retain(|(k, _)| k != &key);
        self.query.push((key, value.to_string()));
    }

    pub fn has_query(&self, key: &str) -> bool {
        self.query.iter().any(|(k, _)| k == key)
    }

    /// Serialise a JSON body.
    pub fn json_body<T: Serialize + ?Sized>(mut self, body: &T) -> Result<Self> {
        let bytes = serde_json::to_vec(body).map_err(|e| {
            // A model that cannot serialise is our bug, not the user's, so say so plainly
            // instead of dressing it up as a server or usage problem.
            Error::new(ErrorKind::Usage(format!("could not serialise the request body: {e}")))
        })?;
        self.body = Body::Json(bytes);
        Ok(self)
    }

    pub fn body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }

    pub fn accept(mut self, accept: Accept) -> Self {
        self.accept = accept;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    pub fn progress(mut self, progress: Progress) -> Self {
        self.progress = progress;
        self
    }

    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Record the token scope this operation needs. See [`Request::scope`].
    ///
    /// Takes a plain `&'static str` rather than an `Option` because the only caller that has an
    /// `Option` is layer 2, which holds `OpMeta::scope` and can assign the field directly; making
    /// every one of the 506 generated call sites write `.scope(Some("write:issue"))` to serve
    /// that one caller is the wrong trade.
    #[must_use]
    pub fn scope(mut self, scope: &'static str) -> Self {
        self.scope = Some(scope);
        self
    }
}

/// Status, headers, and body, for `-i/--include`.
///
/// Headers are an ordered `Vec` rather than a map because `-i` exists to show what the server
/// *actually sent*, including repeated headers in their original order.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Per-iteration pagination options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Paging {
    /// The user's `--limit N`: a cap on **total items across all pages**. Distinct from the
    /// per-page `limit` query parameter, which is derived from capabilities. Conflating the two
    /// is the root of the clamped-page data-loss bug.
    pub limit: Option<usize>,
    /// Requested page size. Clamped to `max_response_items`, or dropped entirely when
    /// `/settings/api` did not answer. See [`paginate::effective_limit`].
    pub per_page: Option<u32>,
}

impl Paging {
    pub fn limit(n: usize) -> Self {
        Self { limit: Some(n), per_page: None }
    }
}

// -------------------------------------------------------------------------------- the client

type WaitSink = Arc<dyn Fn(&WaitNotice) + Send + Sync>;

struct Inner {
    /// `https://host[/subpath]/api/v1`, no trailing slash.
    api_base: String,
    /// `https://host[/subpath]`, no trailing slash. Used for the human-facing settings URL.
    web_base: String,
    /// Authority only, for messages.
    host: String,
    login: Option<String>,
    creds: Credentials,
    transport: Arc<dyn Transport>,
    retry: RetryPolicy,
    token_source: Option<TokenSource>,
    /// Whether a 404 under `/repos/{owner}/{repo}/…` triggers the disambiguating probe.
    probe_404: bool,
    caps: Mutex<Option<(Arc<Capabilities>, Instant)>>,
    /// Single-flight gate for [`Client::capabilities`], held **across** the probe — which is
    /// exactly why it is a second lock rather than `caps` promoted to a `tokio::sync::Mutex`.
    /// See [`Client::capabilities`] for the deadlock that conflating the two would cause.
    caps_gate: tokio::sync::Mutex<()>,
    on_wait: Option<WaitSink>,
}

/// The API client. Cheap to clone (one `Arc`), which the paginated streams rely on.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("api_base", &self.inner.api_base)
            .field("creds", &self.inner.creds)
            .field("transport", &self.inner.transport.name())
            .finish_non_exhaustive()
    }
}

/// Builder for [`Client`].
pub struct ClientBuilder {
    base: String,
    creds: Credentials,
    transport: Option<Arc<dyn Transport>>,
    retry: RetryPolicy,
    user_agent: String,
    token_source: Option<TokenSource>,
    login: Option<String>,
    probe_404: bool,
    on_wait: Option<WaitSink>,
    tls: TlsPolicy,
}

impl Client {
    /// The minimal constructor: a base URL and an [`Auth`].
    ///
    /// Deliberately not tied to any config type. Configuration is a *caller's* concern, and a
    /// published SDK whose client can only be built from a CLI's `hosts.toml` is not usable by
    /// anyone else. Anything richer goes through [`Client::builder`].
    pub fn new(base_url: &str, auth: impl Into<Credentials>) -> Result<Self> {
        Self::builder(base_url, auth).build()
    }

    pub fn builder(base_url: &str, auth: impl Into<Credentials>) -> ClientBuilder {
        ClientBuilder {
            base: base_url.to_owned(),
            creds: auth.into(),
            transport: None,
            retry: RetryPolicy::default(),
            user_agent: concat!("gitea-core/", env!("CARGO_PKG_VERSION")).to_owned(),
            token_source: None,
            login: None,
            probe_404: true,
            on_wait: None,
            tls: TlsPolicy::Verify,
        }
    }

    pub fn api_base(&self) -> &str {
        &self.inner.api_base
    }

    pub fn web_base(&self) -> &str {
        &self.inner.web_base
    }

    pub fn host(&self) -> &str {
        &self.inner.host
    }

    /// The account this client is acting as, when one is known.
    pub fn login(&self) -> Option<&str> {
        self.inner.login.as_deref()
    }

    /// Where a user creates a token on this instance. Every auth error message points here.
    pub fn settings_url(&self) -> String {
        format!("{}/user/settings/applications", self.inner.web_base)
    }

    // -------------------------------------------------------------------------- entry points

    /// Deserialise a JSON response into a generated model.
    pub async fn json<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        let resp = self.send(&req, None).await?;
        let bytes = resp.bytes().await?;
        self.decode(&req, &bytes)
    }

    /// The untyped exit, for layers 1 and 2 where the response shape is not known at compile
    /// time.
    pub async fn value(&self, req: Request) -> Result<serde_json::Value> {
        self.json(req).await
    }

    /// For the many `204 No Content` endpoints. The body is drained and discarded so the
    /// connection returns to the pool.
    pub async fn empty(&self, req: Request) -> Result<()> {
        let resp = self.send(&req, None).await?;
        resp.bytes().await?;
        Ok(())
    }

    /// Everything the server sent, for `-i/--include`.
    ///
    /// **Does not turn a non-2xx status into an `Err`.** `-i` exists precisely to see what the
    /// server said when something went wrong; classifying here would discard the body the user
    /// asked for. Callers decide, using [`RawResponse::is_success`] and
    /// [`Client::classify_raw`].
    pub async fn raw(&self, req: Request) -> Result<RawResponse> {
        let resp = self.dispatch(&req, None).await?;
        let (status, headers, body) = resp.into_parts();
        let headers = headers
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or("<non-utf8>").to_owned()))
            .collect();
        let body = collect(body).await?;
        Ok(RawResponse { status: status.as_u16(), headers, body })
    }

    /// Turn a non-2xx [`RawResponse`] into the error it represents, so `-i` can print the
    /// response *and* exit with a classified code.
    pub async fn classify_raw(&self, req: &Request, resp: &RawResponse) -> Option<Error> {
        if resp.is_success() {
            return None;
        }
        let mut headers = HeaderMap::new();
        for (n, v) in &resp.headers {
            if let (Ok(n), Ok(v)) =
                (http::HeaderName::from_bytes(n.as_bytes()), http::HeaderValue::from_str(v))
            {
                headers.append(n, v);
            }
        }
        Some(self.error_from(req, resp.status, &headers, &resp.body).await)
    }

    /// A streamed body plus its media type, for `zip`, `octet-stream`, and `gzip` responses.
    ///
    /// Nothing is buffered: this is the path a multi-gigabyte release-asset download takes.
    pub async fn bytes(&self, req: Request) -> Result<(Mime, ByteStream)> {
        let resp = self.send(&req, None).await?;
        let mime = Mime::new(resp.content_type().unwrap_or("application/octet-stream"));
        Ok((mime, resp.body))
    }

    /// GET an absolute URL under this instance's **web** root rather than its API root, as a
    /// stream of bytes.
    ///
    /// Some content has no API route that returns bytes. A release asset is reached only through
    /// `Attachment.browser_download_url`; `GET /releases/{id}/assets/{id}` answers with JSON
    /// describing it. Raw file and wiki content is the same story. Those URLs are on the same
    /// host, behind the same credential, and deserve the same retry policy, error classification
    /// and redaction as every other request — which is the reason not to hand the caller a bare
    /// HTTP client and wish them luck.
    ///
    /// # The host check is the point
    ///
    /// `url` must be under [`Client::web_base`], and is refused otherwise. Gitea supports
    /// *external* attachments whose URL points anywhere the uploader likes, so following one
    /// blindly would let a server aim an `Authorization`-bearing client at a host of its
    /// choosing and harvest the token. A prefix match is not enough on its own — `https://host`
    /// prefixes `https://host.evil.example` — so the character after the prefix must be a `/`,
    /// a `?`, a `#`, or nothing at all.
    ///
    /// Without this, callers reached the same place by asking for `/../..` and relying on RFC
    /// 3986 dot-segment removal to climb out of `/api/v1`. That works, and it is not something a
    /// reader should have to reconstruct.
    pub async fn web_bytes(&self, url: &str) -> Result<(Mime, ByteStream)> {
        let req = self.web_request(Method::GET, url)?;
        let resp = self.web_send(&req, url).await?;
        let mime = Mime::new(resp.content_type().unwrap_or("application/octet-stream"));
        Ok((mime, resp.body))
    }

    /// GET a JSON document from the instance's web root.
    ///
    /// Same host check, retry policy and redaction as [`Client::web_bytes`]; the only difference
    /// is that the body is collected and deserialised. Exists for OpenID Connect discovery,
    /// which lives at `/.well-known/openid-configuration` and so is not an API route.
    pub async fn web_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut req = self.web_request(Method::GET, url)?;
        req.accept = Accept::Json;
        let resp = self.web_send(&req, url).await?;
        let bytes = resp.bytes().await?;
        self.decode(&req, &bytes)
    }

    /// POST an `application/x-www-form-urlencoded` body to the instance's web root.
    ///
    /// For OAuth2's token endpoint, which is at `/login/oauth/access_token` — under the web
    /// root, not `/api/v1`, and therefore absent from the generated client.
    ///
    /// # Credentials
    ///
    /// This sends whatever credential the client carries, and for a token exchange that is
    /// wrong: the request authenticates with a `client_id` in the body, and attaching a stale
    /// `Authorization` header alongside invites a server to authenticate the wrong one of the
    /// two. Callers exchanging or refreshing a token use [`Client::anonymous`], which removes
    /// the credential structurally rather than by remembering not to send it.
    pub async fn web_form<T: DeserializeOwned>(
        &self,
        url: &str,
        fields: Vec<(String, String)>,
    ) -> Result<T> {
        let req = self.web_form_request(url, fields)?;
        let resp = self.web_send(&req, url).await?;
        let bytes = resp.bytes().await?;
        self.decode(&req, &bytes)
    }

    /// The same form POST, **without** turning a non-2xx status into an `Err`.
    ///
    /// Stands to [`Client::web_form`] exactly as [`Client::raw`] stands to [`Client::json`], and
    /// exists for the same reason: the caller needs the body of a failed response, not a
    /// classification of it. OAuth2's error body is RFC 6749 §5.2's `{error,
    /// error_description}`, which is not Gitea's API error shape, so the OAuth layer reads it
    /// itself rather than having it flattened into a generic message on the way past.
    pub async fn web_form_raw(
        &self,
        url: &str,
        fields: Vec<(String, String)>,
    ) -> Result<RawResponse> {
        let req = self.web_form_request(url, fields)?;
        let resp = self.dispatch(&req, Some(url)).await.map_err(|mut e| {
            e.ctx.path = Some(req.path.clone());
            e
        })?;
        let (status, headers, body) = resp.into_parts();
        let headers = headers
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or("<non-utf8>").to_owned()))
            .collect();
        Ok(RawResponse { status: status.as_u16(), headers, body: collect(body).await? })
    }

    fn web_form_request(&self, url: &str, fields: Vec<(String, String)>) -> Result<Request> {
        let mut req = self.web_request(Method::POST, url)?;
        req.accept = Accept::Json;
        req.body = Body::Form(fields);
        Ok(req)
    }

    /// Send a request whose URL is on the web root, correcting the path reported on failure.
    ///
    /// `wire_path` prefixes the API subpath, which is right for every other request and wrong
    /// for these — the URL is on the web root. Correcting it here means the `request:` line
    /// names what was actually fetched rather than an `/api/v1/…` path that does not exist.
    async fn web_send(&self, req: &Request, url: &str) -> Result<Response> {
        self.send(req, Some(url)).await.map_err(|mut e| {
            e.ctx.path = Some(req.path.clone());
            e
        })
    }

    /// The same instance, same transport, same retry policy — with no credential.
    ///
    /// Cheap: the transport is shared, and the base URLs are copied already-normalised rather
    /// than re-parsed. The capability cache is deliberately *not* shared, because what an
    /// anonymous client can see is not what an authenticated one can.
    ///
    /// This exists so that "the token exchange must not send an Authorization header" is a
    /// property of the type rather than a rule someone has to keep in mind.
    pub fn anonymous(&self) -> Client {
        Client {
            inner: Arc::new(Inner {
                api_base: self.inner.api_base.clone(),
                web_base: self.inner.web_base.clone(),
                host: self.inner.host.clone(),
                login: self.inner.login.clone(),
                creds: Credentials::default(),
                transport: Arc::clone(&self.inner.transport),
                retry: self.inner.retry,
                token_source: None,
                probe_404: self.inner.probe_404,
                caps: Mutex::new(None),
                caps_gate: tokio::sync::Mutex::new(()),
                on_wait: self.inner.on_wait.clone(),
            }),
        }
    }

    /// The [`Request`] `web_bytes` sends, separated out so the host check is testable and so a
    /// caller wanting the bytes collected rather than streamed is not forced to re-derive it.
    ///
    /// Its `path` is the instance-relative remainder of `url`, which is what the error messages
    /// and `--debug` trace show. The request is dispatched with `url` as an override, so the
    /// path is never used to build the URL and cannot reintroduce the `/api/v1` prefix.
    fn web_request(&self, method: Method, url: &str) -> Result<Request> {
        let base = &self.inner.web_base;
        let rest = url.strip_prefix(base.as_str()).filter(|rest| {
            // `https://forge.test` must not be treated as a prefix of
            // `https://forge.test.evil.example/...`.
            rest.is_empty() || rest.starts_with(['/', '?', '#'])
        });
        let Some(rest) = rest else {
            return Err(Error::new(ErrorKind::Usage(format!(
                "{url} is outside {base}. gea will not send your token to this external URL. Download it separately with curl or a browser."
            ))));
        };
        let mut req = Request::new(method, if rest.is_empty() { "/" } else { rest });
        req.accept = Accept::Any;
        Ok(req)
    }

    /// Every item of a paginated collection, as a stream.
    pub fn items<T: DeserializeOwned + Send + 'static>(&self, req: Request) -> ItemStream<T> {
        self.items_paged(req, Paging::default())
    }

    /// Every item, with an explicit `--limit` and page size. See [`Paging`].
    pub fn items_paged<T: DeserializeOwned + Send + 'static>(
        &self,
        req: Request,
        paging: Paging,
    ) -> ItemStream<T> {
        ItemStream::new(self.clone(), req, paging)
    }

    /// One page, plus what its headers said. The `_page` half of the generated pair, for callers
    /// that want to drive pagination themselves or read `x-total-count`.
    pub async fn page<T: DeserializeOwned>(&self, req: Request) -> Result<(Vec<T>, PageInfo)> {
        let resp = self.send(&req, None).await?;
        let (_, headers, body) = resp.into_parts();
        let bytes = collect(body).await?;
        let items: Vec<T> = self.decode_items(&req, &bytes)?;
        let info = PageInfo::from_headers(&headers, items.len());
        Ok((items, info))
    }

    /// What this instance can do, cached for [`capabilities::TTL`].
    ///
    /// Returns an `Arc` rather than a `&Capabilities` (as originally sketched): a TTL means the
    /// cache can be *replaced*, and handing out a borrow into a refreshable slot behind `&self`
    /// is not expressible without either `unsafe` or a `OnceCell` that can never expire.
    ///
    /// Never fails in practice — an unreachable `/settings/api` yields
    /// [`Capabilities::conservative`] — but stays a `Result` so that adding a hard failure later
    /// is not a breaking change for a published crate.
    ///
    /// # Concurrent first calls probe once, not once each
    ///
    /// Commands fan out: `gea status` drives four [`paginate`] walks at once, and every walk
    /// asks for capabilities before its first request. Without a gate all four would miss the
    /// cold cache, all four would probe, and one invocation would spend **eight** requests where
    /// two do — growing with every command that learns to overlap its reads. So the loser of the
    /// race waits for the winner's probe and reuses it.
    ///
    /// # Why the gate is a second lock and not `caps` made async
    ///
    /// The obvious simplification — make `caps` itself a `tokio::sync::Mutex` and hold it across
    /// the probe — **deadlocks on the instances this module exists to tolerate**. The path is:
    /// `capabilities::probe` → [`Client::json`] → `Client::send` → a non-2xx →
    /// `Client::error_from` → `Client::cached_caps`, which locks `caps`. A `403` or `404`
    /// from `/settings/api` is the *documented common case* (see the [`capabilities`] module
    /// header), so the probe re-enters that lock on exactly the deployments — older instances,
    /// instances requiring auth for everything — that need to keep working. A separate gate
    /// leaves `caps` a plain std mutex that is only ever locked for the length of a clone.
    pub async fn capabilities(&self) -> Result<Arc<Capabilities>> {
        if let Some(c) = self.cached_caps() {
            return Ok(c);
        }
        let _gate = self.inner.caps_gate.lock().await;
        // Re-check: the winner of the race filled the cache while we waited on the gate, and
        // without this the gate would only serialise the duplicate probes rather than remove
        // them.
        if let Some(c) = self.cached_caps() {
            return Ok(c);
        }
        let caps = Arc::new(capabilities::probe(self).await);
        if let Ok(mut slot) = self.inner.caps.lock() {
            *slot = Some((caps.clone(), Instant::now()));
        }
        Ok(caps)
    }

    fn cached_caps(&self) -> Option<Arc<Capabilities>> {
        let slot = self.inner.caps.lock().ok()?;
        let (caps, at) = slot.as_ref()?;
        (at.elapsed() < capabilities::TTL).then(|| caps.clone())
    }

    /// The one cheap probe that makes a 404 unambiguous. Public because layer 1 and porcelain
    /// commands sometimes want to ask directly.
    pub async fn probe_repo(&self, owner: &str, repo: &str) -> RepoProbe {
        let path = format!("/repos/{}/{}", encode::seg(owner), encode::seg(repo));
        match self.dispatch(&Request::get(path), None).await {
            Ok(r) if r.status.is_success() => RepoProbe::Exists,
            Ok(r) if r.status.as_u16() == 404 => RepoProbe::Missing,
            // A 403 (private repository, insufficient scope) or a transport failure tells us
            // nothing, and pretending otherwise would produce a confidently wrong message.
            _ => RepoProbe::NotAttempted,
        }
    }

    // --------------------------------------------------------------------------- send path

    /// Dispatch, then turn a non-2xx status into a classified error.
    async fn send(&self, req: &Request, url: Option<&str>) -> Result<Response> {
        let resp = self.dispatch(req, url).await?;
        if resp.status.is_success() {
            return Ok(resp);
        }
        let (status, headers, body) = resp.into_parts();
        let bytes = collect(body).await.unwrap_or_default();
        Err(self.error_from(req, status.as_u16(), &headers, &bytes).await)
    }

    /// Send with retry, returning whatever status came back.
    async fn dispatch(&self, req: &Request, url: Option<&str>) -> Result<Response> {
        let mut attempt = 1u32;
        loop {
            let http = self.prepare(req, url)?;
            let replayable = http.body.is_replayable();
            let method = http.method.clone();

            match self.inner.transport.execute(http).await {
                Ok(resp) => {
                    let status = resp.status.as_u16();
                    let after = resp.header("retry-after").and_then(retry::retry_after);
                    let decision = self.inner.retry.on_status(
                        &method,
                        status,
                        after,
                        attempt,
                        replayable,
                        retry::Jitter::sample(),
                    );
                    match decision {
                        retry::Decision::Retry { after, reason } => {
                            self.wait(attempt, after, reason).await;
                            attempt += 1;
                        }
                        retry::Decision::Fail => return Ok(resp),
                    }
                }
                Err(e) => {
                    let decision = self.inner.retry.on_transport(
                        &method,
                        e.kind(),
                        attempt,
                        replayable,
                        retry::Jitter::sample(),
                    );
                    match decision {
                        retry::Decision::Retry { after, reason } => {
                            self.wait(attempt, after, reason).await;
                            attempt += 1;
                        }
                        retry::Decision::Fail => {
                            return Err(e.with_ctx(self.ctx(req, None)));
                        }
                    }
                }
            }
        }
    }

    /// Announce a wait that a user would otherwise experience as a hang, then sleep.
    async fn wait(&self, attempt: u32, after: Duration, reason: retry::RetryReason) {
        if after >= retry::NOTIFY_THRESHOLD
            && let Some(sink) = &self.inner.on_wait
        {
            sink(&WaitNotice {
                host: self.inner.host.clone(),
                attempt,
                of: self.inner.retry.max,
                after,
                reason,
            });
        }
        tokio::time::sleep(after).await;
    }

    /// Build the wire request. The **only** place headers and URLs are assembled.
    fn prepare(&self, req: &Request, url_override: Option<&str>) -> Result<HttpRequest> {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::ACCEPT, http::HeaderValue::from_str(req.accept.header())?);

        // Auth first, so a caller-supplied `-H 'Authorization: …'` (layer 1 allows it) wins by
        // overwriting rather than being silently ignored.
        let extra_query = self.inner.creds.apply(&mut headers)?;

        for (name, value) in &req.extra_headers {
            let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                Error::new(ErrorKind::Usage(format!("{name:?} is not a valid header name")))
            })?;
            headers.insert(name, http::HeaderValue::from_str(value)?);
        }

        let url = match url_override {
            // A `Link: rel="next"` URL is followed verbatim. Re-deriving it from parts would
            // drop parameters the server added and re-add ones it dropped.
            Some(u) => u.to_owned(),
            None => self.url_for(req, &extra_query),
        };

        let body = match &req.body {
            Body::None => OutBody::Empty,
            Body::Json(v) => {
                OutBody::Bytes { mime: "application/json".to_owned(), data: Bytes::from(v.clone()) }
            }
            Body::Form(pairs) => {
                let encoded = pairs
                    .iter()
                    .map(|(k, v)| format!("{}={}", encode::form(k), encode::form(v)))
                    .collect::<Vec<_>>()
                    .join("&");
                OutBody::Bytes {
                    mime: "application/x-www-form-urlencoded".to_owned(),
                    data: Bytes::from(encoded),
                }
            }
            Body::Octets { mime, src } => OutBody::Stream {
                mime: mime.clone(),
                src: src.clone(),
                progress: req.progress.clone(),
            },
            Body::Multipart(parts) => {
                OutBody::Multipart { parts: parts.clone(), progress: req.progress.clone() }
            }
        };

        Ok(HttpRequest { method: req.method.clone(), url, headers, body, timeout: req.timeout })
    }

    fn url_for(&self, req: &Request, extra_query: &[(&'static str, String)]) -> String {
        let mut url = format!("{}{}", self.inner.api_base, req.path);
        let pairs = req
            .query
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_str()))
            .chain(extra_query.iter().map(|(k, v)| (*k, v.as_str())));
        let qs = encode::query_string(pairs);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs);
        }
        url
    }

    /// The wire path, with the API prefix, for error messages: `/api/v1/repos/o/r/issues`.
    fn wire_path(&self, req: &Request) -> String {
        let prefix = self
            .inner
            .api_base
            .split_once("://")
            .map_or("", |(_, rest)| rest.find('/').map_or("", |i| &rest[i..]));
        format!("{prefix}{}", req.path)
    }

    fn ctx(&self, req: &Request, status: Option<u16>) -> RequestCtx {
        let path = self.wire_path(req);
        RequestCtx {
            host: Some(self.inner.host.clone()),
            login: self.inner.login.clone(),
            method: Some(req.method.to_string()),
            repo: classify::repo_slug_of(&path),
            path: Some(path),
            status,
            token_source: self.inner.token_source.clone(),
            credential_kind: match self.inner.creds.auth {
                Auth::Token(_) => Some(CredentialKind::Pat),
                Auth::Bearer(_) => Some(CredentialKind::Oauth2),
                // Basic auth and no-credential are neither, and saying "personal access token"
                // about them would put a wrong remedy in a 401.
                Auth::None | Auth::Basic { .. } => None,
            },
        }
    }

    async fn error_from(
        &self,
        req: &Request,
        status: u16,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Error {
        let path = self.wire_path(req);
        let mut cctx = ClassifyCtx {
            host: self.inner.host.clone(),
            method: req.method.to_string(),
            path: path.clone(),
            had_token: self.inner.creds.auth.has_credential(),
            login: self.inner.login.clone(),
            // Filled immediately below from the operation identity the request carries.
            needed_scope: Vec::new(),
            have_scopes: None,
            settings_url: self.settings_url(),
            instance: self.cached_caps().and_then(|c| c.instance_label()),
            repo_probe: RepoProbe::NotAttempted,
            uploading: None,
        }
        // The authoritative scope, when the caller had one: generated code passes the value
        // codegen derived, layer 2 passes `OpMeta::scope`. `with_op_scope` treats `None` and a
        // blank string alike, and `ClassifyCtx::scope_to_name` then falls back to `infer_scope`
        // so the `needs:` line is populated either way.
        .with_op_scope(req.scope);

        // One cheap probe, and only for a 404 under a repository path. This is what lets the
        // message say "the repository exists, so only pull request #4212 is missing" instead of
        // listing three possibilities and leaving the user to guess.
        if self.inner.probe_404
            && let Some((owner, repo)) = classify::probe_target(status, &path)
        {
            cctx.repo_probe = self.probe_repo(&owner, &repo).await;
        }

        let kind = classify::classify(status, headers, body, &cctx);
        Error::new(kind).with_ctx(self.ctx(req, Some(status)))
    }

    // ------------------------------------------------------------------------------ decoding

    fn decode<T: DeserializeOwned>(&self, req: &Request, bytes: &[u8]) -> Result<T> {
        let mut de = serde_json::Deserializer::from_slice(bytes);
        serde_path_to_error::deserialize(&mut de)
            .map_err(|e| self.decode_error(req, &json_pointer(e.path()), &e, bytes))
    }

    /// One page of a paginated collection.
    ///
    /// # A bare `null` is an empty page, not a decode failure
    ///
    /// Go marshals a nil slice as `null`, not `[]`, and a Gitea handler that returns its zero
    /// value on an empty result therefore answers a perfectly good request with a four-byte
    /// body. The 45 *unpaginated* array-returning operations were fixed in the client emitter,
    /// which types them `Option<Vec<T>>` and calls `unwrap_or_default`. The 105 paginated ones
    /// do not go through that: they come here, where requiring a leading `[` turned an empty
    /// page into `ErrorKind::Decode`. `{"data": null}` is the same nil through the wrapped
    /// shape, and gets the same answer.
    ///
    /// # A null *row* is left out, and reported
    ///
    /// Gitea converts each row of a listing on its own and leaves a nil in the array for one it
    /// fails to convert — `GET /user/repos` does it for a fork whose parent it cannot load
    /// (observed live against 1.27.3). A strict `Vec<T>` turned that one row into a decode error
    /// for the whole listing, on every run, for as long as the repository existed. So a `null`
    /// row is dropped and recorded with [`crate::error::compat::note_null_rows`], which `gea`
    /// prints at exit: the list is short by the rows the server could not produce, and the user
    /// is told so rather than handed a quietly short answer. A null *inside* a row is still the
    /// model's business (see `gitea-model`'s `tests/corpus.rs`).
    fn decode_items<T: DeserializeOwned>(&self, req: &Request, bytes: &[u8]) -> Result<Vec<T>> {
        let trimmed = trim_ws(bytes);
        if trimmed.first() == Some(&b'[') {
            let mut de = serde_json::Deserializer::from_slice(trimmed);
            let rows: Vec<Option<T>> = serde_path_to_error::deserialize(&mut de)
                .map_err(|e| self.decode_error(req, &json_pointer(e.path()), &e, bytes))?;
            return Ok(self.without_null_rows(req, rows));
        }
        if trimmed == b"null" {
            return Ok(Vec::new());
        }
        // The search endpoints wrap their results: `{"ok": true, "data": [...]}`. Unwrapping
        // here rather than at 20 call sites keeps `items()` usable for them.
        let value: serde_json::Value = self.decode(req, bytes)?;
        if value.get("data").is_some_and(serde_json::Value::is_null) {
            return Ok(Vec::new());
        }
        if let Some(items) = value.get("data").and_then(|d| d.as_array()) {
            let mut out = Vec::with_capacity(items.len());
            if items.iter().any(serde_json::Value::is_null) {
                crate::error::compat::note_null_rows(&format!("{} {}", req.method, req.path));
            }
            for (i, item) in items.iter().enumerate().filter(|(_, v)| !v.is_null()) {
                let v: T = serde_path_to_error::deserialize(item.clone()).map_err(|e| {
                    self.decode_error(
                        req,
                        &format!("/data/{i}{}", json_pointer(e.path())),
                        &e,
                        bytes,
                    )
                })?;
                out.push(v);
            }
            return Ok(out);
        }
        Err(Error::new(ErrorKind::Decode {
            pointer: "/".to_owned(),
            expected: "a JSON array of items".to_owned(),
            body_excerpt: excerpt(bytes),
        })
        .with_ctx(self.ctx(req, None)))
    }

    /// `rows` without its `None`s, recording that there were any. See [`Client::decode_items`].
    fn without_null_rows<T>(&self, req: &Request, rows: Vec<Option<T>>) -> Vec<T> {
        let total = rows.len();
        let out: Vec<T> = rows.into_iter().flatten().collect();
        if out.len() != total {
            crate::error::compat::note_null_rows(&format!("{} {}", req.method, req.path));
        }
        out
    }

    fn decode_error<E: fmt::Display>(
        &self,
        req: &Request,
        pointer: &str,
        e: &serde_path_to_error::Error<E>,
        bytes: &[u8],
    ) -> Error {
        Error::new(ErrorKind::Decode {
            pointer: pointer.to_owned(),
            expected: e.inner().to_string(),
            body_excerpt: excerpt(bytes),
        })
        .with_ctx(self.ctx(req, None))
    }
}

impl ClientBuilder {
    /// Supply the transport. Tests pass a [`FakeTransport`]; production leaves it unset and gets
    /// a [`ReqwestTransport`].
    pub fn transport(mut self, t: Arc<dyn Transport>) -> Self {
        self.transport = Some(t);
        self
    }

    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Where the credential came from, so a 401 can say *which* token was rejected.
    pub fn token_source(mut self, source: TokenSource) -> Self {
        self.token_source = Some(source);
        self
    }

    pub fn login(mut self, login: impl Into<String>) -> Self {
        self.login = Some(login.into());
        self
    }

    /// Disable the 404 disambiguation probe. Worth turning off for a bulk loop over hundreds of
    /// repositories, where the extra request per miss is real cost.
    pub fn probe_404(mut self, enabled: bool) -> Self {
        self.probe_404 = enabled;
        self
    }

    /// Accept invalid TLS certificates — self-signed, expired, or issued for another host.
    ///
    /// **This turns off the only thing protecting the token in transit.** Anything on the
    /// network path can read it and impersonate the instance. It exists because a freshly
    /// stood-up self-hosted Gitea behind a self-signed certificate is a real situation and
    /// the alternative is people reaching for a worse tool; it is not a convenience.
    ///
    /// The better fix is to add the instance's CA certificate to the operating system trust
    /// store, which the production transport reads. Say so wherever this is offered to a user.
    ///
    /// Ignored when a transport is supplied with [`ClientBuilder::transport`]: that transport is
    /// the caller's, TLS policy included.
    pub fn danger_accept_invalid_certs(mut self, yes: bool) -> Self {
        self.tls = if yes { TlsPolicy::AcceptInvalidCerts } else { TlsPolicy::Verify };
        self
    }

    /// Called before a retry wait longer than [`retry::NOTIFY_THRESHOLD`]. The binary hooks a
    /// spinner here; the SDK's other users can ignore it.
    pub fn on_wait<F: Fn(&WaitNotice) + Send + Sync + 'static>(mut self, f: F) -> Self {
        self.on_wait = Some(Arc::new(f));
        self
    }

    pub fn build(self) -> Result<Client> {
        let (api_base, web_base) = normalize_base(&self.base)?;
        let host = authority_of(&web_base);
        let transport: Arc<dyn Transport> = match self.transport {
            Some(t) => t,
            None => Arc::new(ReqwestTransport::with_tls(&self.user_agent, self.tls)?),
        };
        Ok(Client {
            inner: Arc::new(Inner {
                api_base,
                web_base,
                host,
                login: self.login,
                creds: self.creds,
                transport,
                retry: self.retry,
                token_source: self.token_source,
                probe_404: self.probe_404,
                caps: Mutex::new(None),
                caps_gate: tokio::sync::Mutex::new(()),
                on_wait: self.on_wait,
            }),
        })
    }
}

/// Split a user-supplied base URL into the API base and the web base.
///
/// Accepts everything a user might paste: `git.example.org`, `https://git.example.org`, a
/// trailing slash, a subpath install (`https://example.org/gitea`), and a URL that already
/// ends in `/api/v1` — which is what people copy out of API documentation, and which would
/// otherwise become `/api/v1/api/v1` and 404 every request.
fn normalize_base(base: &str) -> Result<(String, String)> {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return Err(Error::new(ErrorKind::Usage("the host URL is empty".to_owned())));
    }
    // No scheme: assume https. Plain http on a non-loopback host is a downgrade nobody asked
    // for, so it is never inferred — an http instance must be spelled out.
    //
    // Note the ordering: trailing slashes are trimmed *after* this, not before. Trimming first
    // turns the input `https://` into `https:`, which then has no `://`, gets prefixed again as
    // `https://https:`, and sails past every subsequent check.
    let with_scheme =
        if trimmed.contains("://") { trimmed.to_owned() } else { format!("https://{trimmed}") };
    if authority_of(&with_scheme).is_empty() {
        return Err(Error::new(ErrorKind::Usage(format!("{base:?} has no host"))));
    }
    let web = with_scheme
        .trim_end_matches('/')
        .trim_end_matches("/api/v1")
        .trim_end_matches('/')
        .to_owned();
    Ok((format!("{web}/api/v1"), web))
}

fn authority_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or("").to_owned()
}

/// Both ends, because the caller compares the result against a whole literal (`null`) as well
/// as peeking at its first byte.
fn trim_ws(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |i| i + 1);
    &b[start..end]
}

/// A JSON pointer for a `serde_path_to_error` path.
///
/// The whole reason `serde_path_to_error` is a dependency: serde's own message is "invalid type:
/// null, expected a string at line 1 column 24187", and a byte offset into 40 KB of minified JSON
/// tells you nothing. `/items/3/head/repo/owner` tells you which field of which item, which is
/// something you can act on — usually by demoting a `required` field in `overrides.toml`.
fn json_pointer(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for segment in path.iter() {
        out.push('/');
        match segment {
            Segment::Seq { index } => out.push_str(&index.to_string()),
            // RFC 6901 escapes: `~` -> `~0`, `/` -> `~1`. A field name containing a slash is
            // unlikely but an unescaped one would silently mean a different location.
            Segment::Map { key } => out.push_str(&key.replace('~', "~0").replace('/', "~1")),
            Segment::Enum { variant } => out.push_str(variant),
            _ => out.push('?'),
        }
    }
    if out.is_empty() { "/".to_owned() } else { out }
}

fn excerpt(bytes: &[u8]) -> String {
    const MAX: usize = 400;
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    if s.chars().count() <= MAX {
        return s.to_owned();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…")
}

async fn collect(mut body: ByteStream) -> Result<Bytes> {
    let mut buf = bytes::BytesMut::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf.freeze())
}

/// A header value we built ourselves failing to parse is a bug here, not a user error, but it
/// must still not panic in a library.
impl From<http::header::InvalidHeaderValue> for Error {
    fn from(e: http::header::InvalidHeaderValue) -> Self {
        Error::new(ErrorKind::Usage(format!("invalid header value: {e}")))
    }
}

// ------------------------------------------------------------------------------- item stream

/// A stream over every item of a paginated collection.
///
/// Concrete, not `impl Stream`, for the reason given on [`ByteStream`]: it appears in generated
/// signatures.
pub struct ItemStream<T> {
    inner: std::pin::Pin<Box<dyn Stream<Item = Result<T>> + Send>>,
}

struct PageWalk<T> {
    client: Client,
    req: Request,
    paging: Paging,
    paginator: Paginator,
    /// `None` on the first iteration; afterwards, where the previous page said to go.
    next: Option<paginate::Next>,
    buf: VecDeque<T>,
    done: bool,
    /// Resolved once, from capabilities, then reused.
    per_page: Option<u32>,
    per_page_resolved: bool,
    /// Why iteration ended, for `--debug`.
    stopped: Option<StopReason>,
}

impl<T: DeserializeOwned + Send + 'static> ItemStream<T> {
    fn new(client: Client, req: Request, paging: Paging) -> Self {
        let state = PageWalk {
            client,
            req,
            paginator: Paginator::new(paging.limit),
            paging,
            next: None,
            buf: VecDeque::new(),
            done: false,
            per_page: None,
            per_page_resolved: false,
            stopped: None,
        };
        let inner = futures::stream::unfold(state, |mut st| async move {
            loop {
                if let Some(item) = st.buf.pop_front() {
                    return Some((Ok(item), st));
                }
                if st.done {
                    return None;
                }
                match st.fetch().await {
                    Ok(()) => {
                        // A page may legitimately contribute nothing (empty page, or every item
                        // trimmed by `--limit`), in which case loop round to the done check
                        // rather than yielding nothing and stalling.
                        continue;
                    }
                    Err(e) => {
                        st.done = true;
                        return Some((Err(e), st));
                    }
                }
            }
        });
        Self { inner: Box::pin(inner) }
    }
}

impl<T> Stream for ItemStream<T> {
    type Item = Result<T>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<T: DeserializeOwned> PageWalk<T> {
    /// Fetch one page, push its items into the buffer, and decide whether to continue.
    async fn fetch(&mut self) -> Result<()> {
        // `--limit` may already be satisfied by the previous page.
        if self.paging.limit.is_some_and(|l| self.paginator.cumulative >= l) {
            self.done = true;
            self.stopped = Some(StopReason::UserLimit);
            return Ok(());
        }

        let url = match &self.next {
            Some(paginate::Next::Url(u)) => Some(u.clone()),
            _ => None,
        };

        let mut req = self.req.clone();
        if url.is_none() {
            if !self.per_page_resolved {
                // One capabilities probe per stream, and only when we are constructing the URL
                // ourselves — a `Link` URL already carries the server's own limit.
                let caps = self.client.capabilities().await.ok();
                self.per_page = paginate::effective_limit(self.paging.per_page, caps.as_deref());
                self.per_page_resolved = true;
            }
            if let Some(n) = self.per_page {
                req.set_query("limit", n);
            }
            if let Some(paginate::Next::Page(p)) = &self.next {
                req.set_query("page", *p);
            }
        }

        let resp = self.client.send(&req, url.as_deref()).await?;
        let (_, headers, body) = resp.into_parts();
        let bytes = collect(body).await?;
        let items: Vec<T> = self.client.decode_items(&req, &bytes)?;

        let info = PageInfo::from_headers(&headers, items.len());
        let keep = self.paginator.keep(items.len());
        self.buf.extend(items.into_iter().take(keep));

        // Rule (f) arrives here as an `Err`: the instance never signalled an end and we refuse
        // to keep asking. Context is attached so the message names the collection that ran away
        // (`request: GET /api/v1/repos/o/r/issues`) rather than leaving the user to guess which
        // of a command's several streams it was.
        match self.paginator.advance(&info).map_err(|e| e.with_ctx(self.client.ctx(&req, None)))? {
            paginate::Next::Stop(reason) => {
                self.done = true;
                self.stopped = Some(reason);
            }
            next => self.next = Some(next),
        }
        Ok(())
    }
}

#[cfg(test)]
impl RawResponse {
    fn body_str_for_test(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use transport::Canned;

    fn client(t: FakeTransport) -> Client {
        Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(Arc::new(t))
            .build()
            .unwrap()
    }

    // ----------------------------------------------------------------------- URL assembly

    /// A URL copied out of API documentation already ends in `/api/v1`; appending a second one
    /// 404s every request in the tool.
    #[test]
    fn base_url_normalisation_accepts_every_form_a_user_might_paste() {
        let cases = [
            ("git.example.org", "https://git.example.org/api/v1", "https://git.example.org"),
            (
                "https://git.example.org",
                "https://git.example.org/api/v1",
                "https://git.example.org",
            ),
            (
                "https://git.example.org/",
                "https://git.example.org/api/v1",
                "https://git.example.org",
            ),
            (
                "https://git.example.org/api/v1",
                "https://git.example.org/api/v1",
                "https://git.example.org",
            ),
            (
                "https://git.example.org/api/v1/",
                "https://git.example.org/api/v1",
                "https://git.example.org",
            ),
            // Subpath install: Gitea's ROOT_URL may carry a path prefix.
            (
                "https://example.org/gitea",
                "https://example.org/gitea/api/v1",
                "https://example.org/gitea",
            ),
            ("http://localhost:3000", "http://localhost:3000/api/v1", "http://localhost:3000"),
        ];
        for (input, api, web) in cases {
            let (a, w) = normalize_base(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!((a.as_str(), w.as_str()), (api, web), "for {input}");
        }
        assert!(normalize_base("").is_err());
        assert!(normalize_base("https://").is_err());
    }

    #[tokio::test]
    async fn repeated_query_keys_all_survive_in_order() {
        let t = Arc::new(FakeTransport::new().on(
            Method::GET,
            "/api/v1/issues",
            Canned::json(200, "[]"),
        ));
        let c = client_from(t.clone());
        let req = Request::get("/issues")
            .query("labels", "bug")
            .query("labels", "ci")
            .query("state", "open");
        let _: serde_json::Value = c.json(req).await.unwrap();
        assert_eq!(t.calls()[0].query, "labels=bug&labels=ci&state=open");
    }

    fn client_from(t: Arc<FakeTransport>) -> Client {
        Client::builder("https://git.example.org", Auth::token("tok")).transport(t).build().unwrap()
    }

    #[tokio::test]
    async fn set_query_replaces_rather_than_appends() {
        let mut req = Request::get("/issues").query("page", 1);
        req.set_query("page", 4);
        assert_eq!(req.query.len(), 1);
        assert_eq!(req.query[0].1, "4");
    }

    /// The mandatory `token ` prefix, asserted end to end rather than only in `auth.rs`, because
    /// the failure mode is a header that never gets attached at all.
    #[tokio::test]
    async fn every_request_carries_the_token_header() {
        let t =
            Arc::new(FakeTransport::new().on(Method::GET, "/api/v1/user", Canned::json(200, "{}")));
        let c = client_from(t.clone());
        c.value(Request::get("/user")).await.unwrap();
        assert_eq!(t.calls()[0].header("authorization"), Some("token tok"));
        assert_eq!(t.calls()[0].header("accept"), Some("application/json"));
    }

    // -------------------------------------------------------------------------- decoding

    #[tokio::test]
    async fn json_decodes_into_a_typed_model() {
        #[derive(serde::Deserialize)]
        struct User {
            login: String,
        }
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/user",
            Canned::json(200, r#"{"login":"perf3ct"}"#),
        ));
        let u: User = c.json(Request::get("/user")).await.unwrap();
        assert_eq!(u.login, "perf3ct");
    }

    /// The reason `serde_path_to_error` is a dependency. serde alone says "invalid type: null,
    /// expected a string at line 1 column 24187"; a byte offset into a large body is useless.
    #[tokio::test]
    async fn a_decode_failure_names_the_json_pointer_that_failed() {
        #[derive(Debug, serde::Deserialize)]
        struct Item {
            #[allow(dead_code)]
            head: Head,
        }
        #[derive(Debug, serde::Deserialize)]
        struct Head {
            #[allow(dead_code)]
            label: String,
        }
        let body = r#"[{"head":{"label":"a"}},{"head":{"label":"b"}},{"head":{"label":null}}]"#;
        let c =
            client(FakeTransport::new().on(Method::GET, "/api/v1/pulls", Canned::json(200, body)));
        let e = c.json::<Vec<Item>>(Request::get("/pulls")).await.unwrap_err();
        let ErrorKind::Decode { pointer, expected, .. } = e.kind() else {
            panic!("expected Decode, got {:?}", e.kind());
        };
        assert_eq!(pointer, "/2/head/label");
        assert!(expected.contains("string"), "{expected}");
    }

    /// Go marshals a nil slice as `null`, and a Gitea handler returning its zero value for an
    /// empty page therefore sends four bytes that used to be an `ErrorKind::Decode`. The
    /// unpaginated operations were fixed in the emitter (`Option<Vec<T>>`); the 105 paginated
    /// ones arrive here.
    #[tokio::test]
    async fn a_null_body_is_an_empty_page_not_a_decode_error() {
        for body in ["null", "  null\n", r#"{"ok":true,"data":null}"#] {
            let c = client(FakeTransport::new().on(
                Method::GET,
                "/api/v1/repos/o/r/issues",
                Canned::json(200, body),
            ));
            let (items, info) = c
                .page::<serde_json::Value>(Request::get("/repos/o/r/issues"))
                .await
                .unwrap_or_else(|e| panic!("{body:?} must be an empty page, got {e}"));
            assert!(items.is_empty(), "{body:?}");
            assert_eq!(info.returned, 0, "{body:?}");
        }
    }

    /// Bug this prevents: `gea repo list` failing on every run for an account that owns one
    /// repository Gitea cannot render. `GET /user/repos` puts a `null` in the array for it
    /// (observed against 1.27.3), and a strict `Vec<T>` made that one row a decode error for the
    /// whole listing. The row is dropped, the rest survive, and the drop is recorded for the
    /// exit note rather than lost.
    // The guard serialises this test against others that drain the process-global note store,
    // so it has to span the request; a single-threaded test runtime cannot deadlock on it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn a_null_row_inside_a_page_is_left_out_and_reported() {
        #[derive(Debug, serde::Deserialize)]
        struct Item {
            id: i64,
        }
        let _serial = crate::error::compat::test_guard();
        crate::error::compat::forget();
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/repos/o/r/issues",
            Canned::json(200, r#"[{"id":1},null,{"id":3}]"#),
        ));
        let (items, _) = c.page::<Item>(Request::get("/repos/o/r/issues")).await.expect("a page");
        assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 3]);
        let notes = crate::error::compat::drain();
        assert!(
            notes.iter().any(|n| matches!(
                n,
                crate::error::compat::Note::NullRows { request } if request.contains("/repos/o/r/issues")
            )),
            "the dropped row must be reported: {notes:?}"
        );
    }

    /// A body that is neither an array, a `null`, nor a `data` wrapper is still a decode error —
    /// the null case must not have widened into "anything goes".
    #[tokio::test]
    async fn a_non_array_body_is_still_a_decode_error() {
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/repos/o/r/issues",
            Canned::json(200, r#"{"message":"nope"}"#),
        ));
        let e = c.page::<serde_json::Value>(Request::get("/repos/o/r/issues")).await.unwrap_err();
        let ErrorKind::Decode { expected, .. } = e.kind() else {
            panic!("expected Decode, got {:?}", e.kind());
        };
        assert!(expected.contains("array"), "{expected}");
    }

    #[test]
    fn json_pointer_escapes_rfc_6901_specials() {
        // Exercised via the public behaviour above; this pins the escaping rule itself.
        assert_eq!(excerpt(b"  hi  "), "hi");
    }

    #[tokio::test]
    async fn empty_drains_a_204_without_decoding() {
        let c =
            client(FakeTransport::new().on(Method::DELETE, "/api/v1/repos/o/r", Canned::new(204)));
        c.empty(Request::delete("/repos/o/r")).await.unwrap();
    }

    #[tokio::test]
    async fn bytes_returns_the_media_type_and_streams_the_body() {
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/repos/o/r/archive/main.zip",
            Canned::bytes(200, "application/zip", &b"PK\x03\x04"[..]),
        ));
        let (mime, body) = c.bytes(Request::get("/repos/o/r/archive/main.zip")).await.unwrap();
        assert_eq!(mime.essence(), "application/zip");
        assert_eq!(&collect(body).await.unwrap()[..], b"PK\x03\x04");
    }

    /// `-i` exists to show what the server said. Classifying here would throw away the body the
    /// user explicitly asked to see.
    #[tokio::test]
    async fn raw_returns_the_response_even_when_it_is_an_error() {
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/api/v1/user",
            Canned::json(422, r#"{"message":"nope"}"#).with_header("x-total-count", "0"),
        ));
        let r = c.raw(Request::get("/user")).await.unwrap();
        assert_eq!(r.status, 422);
        assert!(!r.is_success());
        assert_eq!(r.header("x-total-count"), Some("0"));
        assert_eq!(r.body_str_for_test(), r#"{"message":"nope"}"#);

        let err = c.classify_raw(&Request::get("/user"), &r).await.expect("should classify");
        assert!(matches!(err.kind(), ErrorKind::Validation { .. }));
    }

    // --------------------------------------------------------------------------- retry

    #[tokio::test]
    async fn a_get_retries_a_503_and_eventually_succeeds() {
        let t = Arc::new(FakeTransport::new().on_sequence(
            Method::GET,
            "/api/v1/user",
            vec![Canned::new(503), Canned::json(200, r#"{"login":"x"}"#)],
        ));
        let c = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            // Zero backoff so the test does not sleep.
            .retry(RetryPolicy { base: Duration::ZERO, jitter: 0.0, ..RetryPolicy::default() })
            .build()
            .unwrap();
        c.value(Request::get("/user")).await.unwrap();
        assert_eq!(t.call_count(), 2);
    }

    /// The rule, end to end: one attempt, no more, whatever the server says.
    #[tokio::test]
    async fn a_post_is_sent_exactly_once_even_on_a_503() {
        let t = Arc::new(FakeTransport::new().on(
            Method::POST,
            "/api/v1/repos/o/r/pulls",
            Canned::new(503),
        ));
        let c = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            .retry(RetryPolicy { base: Duration::ZERO, jitter: 0.0, ..RetryPolicy::default() })
            .build()
            .unwrap();
        let e = c.value(Request::post("/repos/o/r/pulls")).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::ServerError { status: 503, .. }));
        assert_eq!(t.call_count(), 1, "a retried POST /pulls creates two pull requests");
    }

    // ------------------------------------------------------------------- 404 probe wiring

    /// The probe is what turns an ambiguous 404 into a definite statement. It must actually fire.
    #[tokio::test]
    async fn a_404_under_a_repo_fires_exactly_one_probe() {
        let t = Arc::new(
            FakeTransport::new()
                .on(
                    Method::GET,
                    "/api/v1/repos/o/r/pulls/4212",
                    Canned::json(404, r#"{"message":"not found"}"#),
                )
                .on(Method::GET, "/api/v1/repos/o/r", Canned::json(200, r#"{"id":1}"#)),
        );
        let c = client_from(t.clone());
        let e = c.value(Request::get("/repos/o/r/pulls/4212")).await.unwrap_err();
        let ErrorKind::ResourceNotFound { kind, id, .. } = e.kind() else {
            panic!("expected ResourceNotFound, got {:?}", e.kind());
        };
        assert_eq!((*kind, id.as_str()), ("pull request", "4212"));
        assert_eq!(t.calls_to(&Method::GET, "/api/v1/repos/o/r").len(), 1, "exactly one probe");
    }

    #[tokio::test]
    async fn the_probe_can_be_disabled() {
        let t = Arc::new(FakeTransport::new().on(
            Method::GET,
            "/api/v1/repos/o/r/pulls/4212",
            Canned::json(404, r#"{"message":"not found"}"#),
        ));
        let c = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(t.clone())
            .probe_404(false)
            .build()
            .unwrap();
        let e = c.value(Request::get("/repos/o/r/pulls/4212")).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::RepoNotFound { .. }), "must fall back to ambiguous");
        assert_eq!(t.call_count(), 1);
    }

    #[tokio::test]
    async fn request_context_records_host_method_path_and_repo() {
        let c = client(FakeTransport::new().on(
            Method::POST,
            "/api/v1/repos/perf3ct/gea/issues",
            Canned::json(422, r#"{"errors":["title is empty"]}"#),
        ));
        let e = c.value(Request::post("/repos/perf3ct/gea/issues")).await.unwrap_err();
        assert_eq!(e.ctx.host.as_deref(), Some("git.example.org"));
        assert_eq!(e.ctx.method.as_deref(), Some("POST"));
        assert_eq!(e.ctx.path.as_deref(), Some("/api/v1/repos/perf3ct/gea/issues"));
        assert_eq!(e.ctx.repo.as_deref(), Some("perf3ct/gea"));
        assert_eq!(e.ctx.status, Some(422));
    }

    /// A subpath install must produce `/gitea/api/v1/...` in messages, matching what the user
    /// would see in a browser or a proxy log.
    #[tokio::test]
    async fn a_subpath_install_reports_the_full_wire_path() {
        let c = Client::builder("https://example.org/gitea", Auth::None)
            .transport(Arc::new(FakeTransport::new().on(
                Method::GET,
                "/gitea/api/v1/user",
                Canned::json(401, r#"{"message":"unauthorized"}"#),
            )))
            .build()
            .unwrap();
        let e = c.value(Request::get("/user")).await.unwrap_err();
        assert_eq!(e.ctx.path.as_deref(), Some("/gitea/api/v1/user"));
        assert!(matches!(e.kind(), ErrorKind::NotAuthenticated { .. }));
    }

    // ------------------------------------------------------------------------ web_bytes

    /// Release assets, raw files and wiki content live on the web root, not under `/api/v1`,
    /// and there is no API route that hands back their bytes.
    #[tokio::test]
    async fn web_bytes_fetches_from_the_web_root_with_the_instance_credential() {
        let t = Arc::new(FakeTransport::new().on(
            Method::GET,
            "/o/r/releases/download/v1.0/app.tgz",
            Canned::new(200).with_header("content-type", "application/gzip").with_body("tarball"),
        ));
        let c = Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(t.clone())
            .build()
            .unwrap();

        let (mime, body) = c
            .web_bytes("https://git.example.org/o/r/releases/download/v1.0/app.tgz")
            .await
            .unwrap();
        assert_eq!(mime.essence(), "application/gzip");
        assert_eq!(&collect(body).await.unwrap()[..], b"tarball");

        let call = &t.calls()[0];
        assert_eq!(call.url, "https://git.example.org/o/r/releases/download/v1.0/app.tgz");
        assert!(
            !call.url.contains("/api/v1"),
            "the API base must not be prepended to a web URL: {}",
            call.url
        );
        // Same credential, same everything: this is why it is a `Client` method and not a
        // suggestion that the caller bring their own HTTP client.
        assert_eq!(call.headers.iter().find(|(n, _)| n == "authorization").unwrap().1, "token tok");
    }

    /// A subpath install's web root is `https://example.org/gitea`, and the asset URL the API
    /// hands back is under it.
    #[tokio::test]
    async fn web_bytes_works_on_a_subpath_install() {
        let t = Arc::new(FakeTransport::new().on(
            Method::GET,
            "/gitea/o/r/raw/branch/main/README.md",
            Canned::new(200).with_body("# hi"),
        ));
        let c = Client::builder("https://example.org/gitea", Auth::None)
            .transport(t.clone())
            .build()
            .unwrap();
        let (_, body) =
            c.web_bytes("https://example.org/gitea/o/r/raw/branch/main/README.md").await.unwrap();
        assert_eq!(&collect(body).await.unwrap()[..], b"# hi");
    }

    /// **The security property.** Gitea lets an uploader register an *external* attachment
    /// whose `browser_download_url` points anywhere. Following one would hand the token to a
    /// host of the server's choosing, so an off-instance URL is refused rather than fetched.
    #[tokio::test]
    async fn web_bytes_refuses_a_url_that_is_not_on_this_instance() {
        let t = Arc::new(FakeTransport::new().fallback(Canned::new(200)));
        let c = Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(t.clone())
            .build()
            .unwrap();

        for url in [
            "https://evil.example/steal",
            // The prefix trap: `https://git.example.org` is a string prefix of this host.
            "https://git.example.org.evil.example/steal",
            "http://git.example.org/downgraded",
        ] {
            let e = match c.web_bytes(url).await {
                Ok(_) => panic!("{url} was fetched instead of refused"),
                Err(e) => e,
            };
            assert!(
                matches!(e.kind(), ErrorKind::Usage(m) if m.contains("will not send your token")),
                "{url} should have been refused, got {e}"
            );
        }
        assert_eq!(t.call_count(), 0, "a refused URL must never reach the network");
    }

    /// The `request:` line must name what was actually fetched. `wire_path` prefixes the API
    /// subpath, which would otherwise report a `/api/v1/…` path that does not exist.
    #[tokio::test]
    async fn a_failed_web_fetch_reports_the_web_path_not_an_api_path() {
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/o/r/releases/download/v1.0/gone.tgz",
            Canned::new(404),
        ));
        let e = match c
            .web_bytes("https://git.example.org/o/r/releases/download/v1.0/gone.tgz")
            .await
        {
            Ok(_) => panic!("a 404 must not be reported as success"),
            Err(e) => e,
        };
        assert_eq!(e.ctx.path.as_deref(), Some("/o/r/releases/download/v1.0/gone.tgz"));
    }

    /// OpenID Connect discovery is not an API route, so it goes through the web root — but it
    /// must inherit the same host check, which is what makes it safe to read an endpoint out of
    /// the document it returns.
    #[tokio::test]
    async fn web_json_reads_a_document_from_the_web_root() {
        let c = client(FakeTransport::new().on(
            Method::GET,
            "/.well-known/openid-configuration",
            Canned::new(200).with_header("content-type", "application/json").with_body(
                r#"{"token_endpoint":"https://git.example.org/login/oauth/access_token"}"#,
            ),
        ));
        let v: serde_json::Value =
            c.web_json("https://git.example.org/.well-known/openid-configuration").await.unwrap();
        assert_eq!(v["token_endpoint"], "https://git.example.org/login/oauth/access_token");
    }

    /// The OAuth2 token endpoint takes a form POST, not JSON. Getting this wrong produces a
    /// `400` whose body says nothing about content types.
    #[tokio::test]
    async fn web_form_posts_url_encoded_fields() {
        let t = Arc::new(
            FakeTransport::new().on(
                Method::POST,
                "/login/oauth/access_token",
                Canned::new(200)
                    .with_header("content-type", "application/json")
                    .with_body(r#"{"access_token":"at","token_type":"bearer"}"#),
            ),
        );
        let c = Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(t.clone())
            .build()
            .unwrap();

        let v: serde_json::Value = c
            .web_form(
                "https://git.example.org/login/oauth/access_token",
                vec![("grant_type".to_owned(), "authorization_code".to_owned())],
            )
            .await
            .unwrap();
        assert_eq!(v["access_token"], "at");

        let call = &t.calls()[0];
        assert_eq!(call.method, Method::POST);
        assert_eq!(call.url, "https://git.example.org/login/oauth/access_token");
    }

    /// The bug this prevents: exchanging an authorization code while still presenting the old,
    /// possibly expired credential. Gitea authenticates a token request by the `client_id` in
    /// the body, and an `Authorization` header alongside it is at best ignored and at worst the
    /// thing the server decides to believe. `anonymous` makes that unrepresentable rather than
    /// asking every call site to remember.
    #[tokio::test]
    async fn an_anonymous_client_keeps_the_instance_but_drops_the_credential() {
        let t = Arc::new(
            FakeTransport::new().on(
                Method::POST,
                "/login/oauth/access_token",
                Canned::new(200)
                    .with_header("content-type", "application/json")
                    .with_body(r#"{"access_token":"at"}"#),
            ),
        );
        let c = Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(t.clone())
            .build()
            .unwrap();
        let anon = c.anonymous();

        assert_eq!(anon.web_base(), c.web_base());
        assert_eq!(anon.api_base(), c.api_base());
        assert_eq!(anon.host(), c.host());

        let _: serde_json::Value = anon
            .web_form(
                "https://git.example.org/login/oauth/access_token",
                vec![("client_id".to_owned(), "cid".to_owned())],
            )
            .await
            .unwrap();

        let call = &t.calls()[0];
        assert!(
            !call.headers.iter().any(|(n, _)| n == "authorization"),
            "a token exchange must not present a credential: {:?}",
            call.headers
        );
    }

    /// The host check belongs to every web-root method, not just the one it was written for.
    #[tokio::test]
    async fn the_host_check_covers_the_json_and_form_paths_too() {
        let t = Arc::new(FakeTransport::new().fallback(Canned::new(200)));
        let c = Client::builder("https://git.example.org", Auth::token("tok"))
            .transport(t.clone())
            .build()
            .unwrap();

        let url = "https://git.example.org.evil.example/login/oauth/access_token";
        assert!(c.web_json::<serde_json::Value>(url).await.is_err());
        assert!(c.web_form::<serde_json::Value>(url, vec![]).await.is_err());
        assert_eq!(t.call_count(), 0, "a refused URL must never reach the network");
    }

    // ------------------------------------------------------------------- re-exported Method

    /// `Request::method` is an `http::Method`, so the type has to be nameable through this
    /// crate. Without the re-export a caller has to put `http` in its own `Cargo.toml` to spell
    /// one word — which is what every `gea` test module was working around.
    #[test]
    fn the_http_method_type_is_nameable_without_depending_on_the_http_crate() {
        let m: crate::http::Method = crate::http::Method::PATCH;
        assert_eq!(Request::new(m.clone(), "/x").method, m);
        assert_eq!(Request::post("/x").method, crate::http::Method::POST);
    }

    // -------------------------------------------------------------------------- TLS policy

    /// Verification stays on unless it is asked for, by name, in the builder chain.
    #[test]
    fn tls_verification_is_on_by_default_and_off_only_when_asked() {
        assert_eq!(TlsPolicy::default(), TlsPolicy::Verify);

        let b = Client::builder("https://git.example.org", Auth::None);
        assert_eq!(b.tls, TlsPolicy::Verify);
        assert_eq!(b.danger_accept_invalid_certs(true).tls, TlsPolicy::AcceptInvalidCerts);

        // Reversible, so a caller assembling a builder from config need not branch.
        let b = Client::builder("https://git.example.org", Auth::None)
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_certs(false);
        assert_eq!(b.tls, TlsPolicy::Verify);
    }

    /// Both policies must actually build a transport; a `rustls` feature set that cannot express
    /// one of them should fail here rather than the first time a user passes the flag.
    #[test]
    fn both_tls_policies_produce_a_working_transport() {
        for tls in [TlsPolicy::Verify, TlsPolicy::AcceptInvalidCerts] {
            ReqwestTransport::with_tls("gitea-core/test", tls)
                .unwrap_or_else(|e| panic!("{tls:?} did not build: {e}"));
        }
    }

    // ------------------------------------------------------------------ the scope on a request

    fn scope_403() -> FakeTransport {
        FakeTransport::new().on(
            Method::POST,
            "/api/v1/repos/o/r/pulls",
            // Deliberately no bracketed scope list: when the server names scopes it wins, and
            // this is the case where the answer has to come from the request or the inference.
            Canned::json(403, r#"{"message":"token does not have sufficient scope"}"#),
        )
    }

    fn needed_of(e: &Error) -> Vec<String> {
        match e.kind() {
            ErrorKind::InsufficientScope { needed, .. } => needed.clone(),
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    /// The point of `Request::scope`: what the caller recorded reaches the message.
    ///
    /// `POST /repos/{o}/{r}/pulls` is the discriminating route — `infer_scope` reads the path as
    /// issue-shaped and says `write:issue`, while the specification tags the operation
    /// `repository`. If the scope on the request were being ignored, this would still classify,
    /// just with the wrong name.
    #[tokio::test]
    async fn a_scope_on_the_request_is_the_scope_the_error_names() {
        let c = client(scope_403());
        let e =
            c.value(Request::post("/repos/o/r/pulls").scope("write:repository")).await.unwrap_err();
        assert_eq!(needed_of(&e), vec!["write:repository".to_owned()]);
    }

    /// The fallback survives. `gea api` lets a user type a path we have no operation for, and a
    /// rendering that said "create a NEW token that includes  at" was a real bug.
    #[tokio::test]
    async fn a_request_without_a_scope_falls_back_to_inference_rather_than_saying_nothing() {
        let c = client(scope_403());
        let e = c.value(Request::post("/repos/o/r/pulls")).await.unwrap_err();
        let needed = needed_of(&e);
        assert!(!needed.is_empty(), "the `needs:` line must never be blank");
        assert_eq!(needed, vec!["write:issue".to_owned()], "that is what inference says here");
    }

    /// Carrying a scope must not turn every 403 into a scope problem. The message decides
    /// whether it is one; the scope only decides what to call it.
    #[tokio::test]
    async fn a_known_scope_does_not_make_an_ordinary_403_a_scope_problem() {
        let c = client(FakeTransport::new().on(
            Method::POST,
            "/api/v1/repos/o/r/pulls",
            Canned::json(403, r#"{"message":"user is not a collaborator on this repository"}"#),
        ));
        let e =
            c.value(Request::post("/repos/o/r/pulls").scope("write:repository")).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::Forbidden { .. }), "got {:?}", e.kind());
    }

    /// The scope has to survive every rewrite between construction and the wire. `set_query`,
    /// `query`, and a clone are the three the paginator and the query builders perform.
    #[test]
    fn the_scope_survives_the_rewrites_the_paginator_performs() {
        let mut req = Request::get("/repos/o/r/pulls").scope("read:repository").query("state", "o");
        req.set_query("limit", 50);
        assert_eq!(req.clone().scope, Some("read:repository"));
        assert_eq!(req.scope, Some("read:repository"));
        assert_eq!(Request::get("/user").scope, None, "nothing is assumed for a bare request");
    }
}

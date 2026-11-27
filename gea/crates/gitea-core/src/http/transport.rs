//! The transport seam.
//!
//! [`Transport`] is the single point where this crate touches the network, and it exists so
//! that everything above it — auth injection, retry, pagination, error classification, the
//! entire generated client — can be tested without one. [`FakeTransport`] answers a
//! `(method, path)` pair from a table in memory, which means the pagination and error-handling
//! tests need no `wiremock`, bind no port, spawn no server, and run in microseconds. A test
//! that binds a port is a test that flakes on a busy CI machine and that cannot run in a
//! sandbox; a test that cannot run is a test that stops being written.
//!
//! The trait is intentionally *below* the interesting logic. It takes a fully-formed URL and
//! header map and returns status + headers + a byte stream, and nothing else. Retry lives
//! above it (so a fake can assert how many attempts happened), and classification lives above
//! it (so a fake can serve a real Gitea error body).

use std::fmt;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, Method, StatusCode};

use super::multipart::{self, Part, Progress};
use super::{Source, redact};
use crate::error::{Error, ErrorKind, Phase, Result};

/// A stream of response body chunks.
///
/// **Concrete on purpose.** This is a boxed trait object rather than `impl Stream` because it
/// appears in the return type of generated client methods, and there are 506 of them. An
/// `impl Stream` return type is a distinct anonymous type per function, which monomorphises
/// every downstream combinator per call site — superlinear compile cost across 42k generated
/// lines — and, because the concrete type is unnameable, it also leaks into the published API
/// in a way that makes a future refactor a breaking change. One `Box<dyn>` indirection per
/// *response* is unmeasurable next to a network round trip.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// Wrap already-buffered bytes as a [`ByteStream`].
pub fn once_stream(b: Bytes) -> ByteStream {
    Box::pin(futures::stream::once(async move { Ok(b) }))
}

/// An empty [`ByteStream`], for 204 responses.
pub fn empty_stream() -> ByteStream {
    Box::pin(futures::stream::empty())
}

/// A request as the transport sees it: absolute URL, final headers, materialisable body.
///
/// `Clone` matters — [`super::retry`] re-issues the same request, and a request that could not
/// be cloned would have to be rebuilt from scratch on every attempt (and the rebuild is where
/// a divergence between attempt 1 and attempt 2 would hide).
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    /// Absolute. Built by the client, or taken verbatim from a `Link: rel="next"` header.
    pub url: String,
    pub headers: HeaderMap,
    pub body: OutBody,
    /// Whole-request deadline. `None` means "use the transport's default", which is what
    /// downloads want, since a 2 GB asset legitimately takes minutes.
    pub timeout: Option<Duration>,
}

impl fmt::Debug for HttpRequest {
    /// Redacted, because this is exactly what a `--debug` trace prints.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method.as_str())
            .field("url", &redact::url(&self.url))
            .field("headers", &redact::header_map(&self.headers))
            .field("body", &self.body)
            .finish()
    }
}

/// A body the transport can materialise, described rather than pre-built.
///
/// Describing it means [`FakeTransport`] can *inspect* it (a test can assert what JSON we would
/// have sent) and the retry loop can decide whether replaying is even possible.
#[derive(Clone)]
pub enum OutBody {
    Empty,
    /// Fully buffered: JSON bodies and form encodings.
    Bytes {
        mime: String,
        data: Bytes,
    },
    /// Streamed from a source, for `application/octet-stream` uploads.
    Stream {
        mime: String,
        src: Source,
        progress: Progress,
    },
    /// `multipart/form-data`. The boundary is chosen by the transport.
    Multipart {
        parts: Vec<Part>,
        progress: Progress,
    },
}

impl OutBody {
    /// Whether this body can be sent a second time.
    ///
    /// Anything reading stdin cannot: the bytes are gone after the first attempt, and a retry
    /// would send a truncated or empty body while reporting success. The retry loop refuses to
    /// retry when this is false — a wrong answer here is silent corruption, not a slow request.
    pub fn is_replayable(&self) -> bool {
        match self {
            OutBody::Empty | OutBody::Bytes { .. } => true,
            OutBody::Stream { src, .. } => src.is_replayable(),
            OutBody::Multipart { parts, .. } => parts.iter().all(|p| p.src.is_replayable()),
        }
    }

    pub fn content_type(&self) -> Option<&str> {
        match self {
            OutBody::Empty | OutBody::Multipart { .. } => None,
            OutBody::Bytes { mime, .. } | OutBody::Stream { mime, .. } => Some(mime),
        }
    }
}

impl fmt::Debug for OutBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutBody::Empty => f.write_str("Empty"),
            OutBody::Bytes { mime, data } => {
                // Bodies can carry secrets (`POST /users/{u}/tokens` echoes a password), so a
                // debug trace shows the shape, never the contents.
                write!(f, "Bytes {{ mime: {mime:?}, len: {} }}", data.len())
            }
            OutBody::Stream { mime, src, .. } => {
                write!(f, "Stream {{ mime: {mime:?}, src: {src:?} }}")
            }
            OutBody::Multipart { parts, .. } => write!(f, "Multipart {{ parts: {} }}", parts.len()),
        }
    }
}

/// Status, headers, and an unbuffered body.
///
/// The body stays a stream so that `gea release download` of a 2 GB asset never buffers. The
/// error paths buffer explicitly via [`Response::bytes`], because classification needs the
/// whole (small) error body.
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: ByteStream,
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status.as_u16())
            .field("headers", &redact::header_map(&self.headers))
            .finish_non_exhaustive()
    }
}

impl Response {
    pub fn new(status: StatusCode, headers: HeaderMap, body: ByteStream) -> Self {
        Self { status, headers, body }
    }

    /// Build a fully-buffered response. Used by fakes and by anything that has already read
    /// the body.
    pub fn from_bytes(status: StatusCode, headers: HeaderMap, body: Bytes) -> Self {
        Self { status, headers, body: once_stream(body) }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Drain the body into memory.
    pub async fn bytes(self) -> Result<Bytes> {
        let mut body = self.body;
        let mut buf = bytes::BytesMut::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf.freeze())
    }

    /// Split into head and body, so the head survives buffering the body.
    pub fn into_parts(self) -> (StatusCode, HeaderMap, ByteStream) {
        (self.status, self.headers, self.body)
    }
}

/// The one place this crate talks to a network — or pretends to.
///
/// `Send + Sync` so a `Client` is shareable across tasks, which the paginated streams and any
/// concurrent porcelain command need.
pub trait Transport: Send + Sync {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'_, Result<Response>>;

    /// Named in diagnostics, so a confusing test failure says `FakeTransport` out loud.
    fn name(&self) -> &'static str {
        "transport"
    }
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'_, Result<Response>> {
        (**self).execute(req)
    }

    fn name(&self) -> &'static str {
        (**self).name()
    }
}

// ---------------------------------------------------------------------------------- reqwest

/// Whether the production transport verifies TLS certificates.
///
/// # Why this exists at all
///
/// The honest answer to a self-signed certificate is to put the instance's CA in the operating
/// system trust store, which `reqwest` is configured to read (`rustls-native-certs`), and that
/// stays the advice. But "I am standing up a Gitea instance on my laptop behind a certificate
/// I generated ten seconds ago" is a real first-five-minutes experience, and a client that
/// cannot be talked into connecting sends people to `curl` — or, worse, to a build with
/// verification hard-wired off.
///
/// # Why it is not a `bool`
///
/// Two reasons, both about the call site. A named variant is greppable: `AcceptInvalidCerts`
/// appears in a diff and in a search; `true` does not. And it removes the class of bug where
/// the meaning of the flag is inverted — `new(ua, false)` reads as "not dangerous" to one
/// author and "do not verify" to the next.
///
/// This disables verification **entirely**, including hostname checking: anything on the path
/// can read the token. It is never a default and never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TlsPolicy {
    /// Verify certificates against the OS trust store. Always the default.
    #[default]
    Verify,
    /// Accept any certificate, valid or not. See the warning above.
    AcceptInvalidCerts,
}

/// Production transport.
pub struct ReqwestTransport {
    inner: reqwest::Client,
}

impl fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReqwestTransport")
    }
}

impl ReqwestTransport {
    /// Sensible CLI defaults: a short connect timeout (a wrong host should fail fast, not hang
    /// for 60 seconds) and **no** whole-request timeout, because a release-asset download or a
    /// repository migration legitimately runs for minutes. Per-request deadlines are set by the
    /// caller through [`HttpRequest::timeout`].
    pub fn new(user_agent: &str) -> Result<Self> {
        Self::with_tls(user_agent, TlsPolicy::Verify)
    }

    /// As [`ReqwestTransport::new`], with an explicit TLS verification policy.
    ///
    /// Separate from `new` so that turning verification off is a thing a reader of the call site
    /// can *see*. A boolean parameter on `new` would put `false` at every ordinary call site and
    /// make the dangerous case the one that looks like a typo.
    pub fn with_tls(user_agent: &str, tls: TlsPolicy) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(tls == TlsPolicy::AcceptInvalidCerts)
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Usage(format!("could not initialise the HTTP client: {e}")))
            })?;
        Ok(Self { inner })
    }

    pub fn from_client(inner: reqwest::Client) -> Self {
        Self { inner }
    }
}

impl Transport for ReqwestTransport {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'_, Result<Response>> {
        Box::pin(async move {
            let host = host_of(&req.url);
            let scheme = scheme_of(&req.url);
            let mut rb = self.inner.request(req.method.clone(), &req.url).headers(req.headers);
            if let Some(t) = req.timeout {
                rb = rb.timeout(t);
            }
            rb = match req.body {
                OutBody::Empty => rb,
                OutBody::Bytes { mime, data } => {
                    rb.header(http::header::CONTENT_TYPE, mime).body(data)
                }
                OutBody::Stream { mime, src, progress } => {
                    let (stream, len) = multipart::source_stream(&src, &progress).await?;
                    let mut rb = rb.header(http::header::CONTENT_TYPE, mime);
                    if let Some(n) = len {
                        rb = rb.header(http::header::CONTENT_LENGTH, n);
                    }
                    rb.body(reqwest::Body::wrap_stream(stream))
                }
                OutBody::Multipart { parts, progress } => {
                    rb.multipart(multipart::build_form(&parts, &progress).await?)
                }
            };

            let resp = rb.send().await.map_err(|e| classify_reqwest(&e, &host, &scheme))?;
            let status = resp.status();
            let headers = resp.headers().clone();
            // `bytes_stream` keeps large downloads out of RAM. A mid-body failure becomes a
            // `Timeout { phase: Body }` rather than a connect error, which matters because the
            // retry policy treats them differently: bytes already delivered means the request
            // *was* processed.
            let body =
                resp.bytes_stream().map_err(move |e| classify_reqwest_body(&e, &host)).boxed();
            Ok(Response::new(status, headers, body))
        })
    }

    fn name(&self) -> &'static str {
        "ReqwestTransport"
    }
}

/// Best-effort authority extraction, for error messages only. Never used for routing, so a
/// malformed URL yielding a slightly odd host string is harmless.
fn scheme_of(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, _)) if !scheme.is_empty() => scheme.to_ascii_lowercase(),
        // A URL we cannot read a scheme from is https as far as advice is concerned, which is
        // what the client would have used anyway.
        _ => "https".to_owned(),
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_owned()
}

/// Split a reqwest failure into the taxonomy the renderer can give advice for.
///
/// reqwest's own predicates only distinguish connect/timeout/body, so the finer distinctions —
/// DNS versus TCP versus certificate, and specifically *private CA* — come from walking the
/// source chain. String matching on an error chain is unlovely, but the alternative is telling
/// a user behind a corporate CA "connection failed", which sends them to debug their network
/// instead of their trust store.
fn classify_reqwest(e: &reqwest::Error, host: &str, scheme: &str) -> Error {
    let chain = error_chain(e);
    let lower = chain.to_ascii_lowercase();

    if e.is_timeout() {
        // No response head yet, so nothing was necessarily processed: retryable for
        // idempotent methods.
        return Error::new(ErrorKind::Timeout {
            host: host.to_owned(),
            after: Duration::ZERO,
            phase: Phase::Headers,
        });
    }

    if is_dns(&lower) {
        return Error::new(ErrorKind::Dns { host: host.to_owned() });
    }

    if is_tls(&lower) {
        return Error::new(ErrorKind::Tls {
            host: host.to_owned(),
            cause: chain,
            // `UnknownIssuer` is what rustls reports for a certificate signed by a CA that is
            // not in the trust store — i.e. the corporate/self-hosted case, which has a
            // completely different remedy from an expired or mismatched certificate.
            looks_like_private_ca: lower.contains("unknownissuer")
                || lower.contains("unknown issuer")
                || lower.contains("self-signed")
                || lower.contains("self signed")
                || lower.contains("unable to get local issuer"),
        });
    }

    if lower.contains("proxy") {
        return Error::new(ErrorKind::Proxy { proxy: host.to_owned(), cause: chain });
    }

    if e.is_connect() || e.is_request() {
        let (h, port) = split_port(host);
        let looks_like_plaintext = reads_as_plaintext(&chain);
        return Error::new(ErrorKind::Connect {
            host: h,
            port,
            cause: chain,
            looks_like_plaintext,
            scheme: scheme.to_owned(),
        });
    }

    let looks_like_plaintext = reads_as_plaintext(&chain);
    Error::new(ErrorKind::Connect {
        host: host.to_owned(),
        port: 0,
        cause: chain,
        looks_like_plaintext,
        scheme: scheme.to_owned(),
    })
}

/// A failure *after* the response head arrived. The request reached the server and was
/// processed, so this is never safely retryable for a mutating method.
fn classify_reqwest_body(e: &reqwest::Error, host: &str) -> Error {
    if e.is_timeout() {
        return Error::new(ErrorKind::Timeout {
            host: host.to_owned(),
            after: Duration::ZERO,
            phase: Phase::Body,
        });
    }
    Error::new(ErrorKind::Io(std::io::Error::other(error_chain(e))))
}

fn is_dns(lower: &str) -> bool {
    lower.contains("dns error")
        || lower.contains("failed to lookup address")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("no such host")
}

fn is_tls(lower: &str) -> bool {
    lower.contains("tls")
        || lower.contains("certificate")
        || lower.contains("handshake")
        || lower.contains("invalid peer")
        || lower.contains("unknownissuer")
}

/// Flatten a `source()` chain into one line, deduplicating the repetition that
/// reqwest/hyper/rustls layering produces.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        let s = err.to_string();
        if !parts.iter().any(|p| p == &s || p.contains(&s)) {
            parts.push(s);
        }
        cur = err.source();
    }
    parts.join(": ")
}

fn split_port(authority: &str) -> (String, u16) {
    match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) && !p.is_empty() => {
            (h.to_owned(), p.parse().unwrap_or(0))
        }
        _ => (authority.to_owned(), 0),
    }
}

// ------------------------------------------------------------------------------------- fake

/// A canned response.
#[derive(Clone, Debug)]
pub struct Canned {
    pub status: StatusCode,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Canned {
    pub fn new(status: u16) -> Self {
        Self {
            status: StatusCode::from_u16(status).expect("test used an invalid status code"),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .with_header("content-type", "application/json;charset=utf-8")
            .with_body(body.into())
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .with_header("content-type", "text/plain;charset=utf-8")
            .with_body(body.into())
    }

    pub fn html(status: u16, body: impl Into<String>) -> Self {
        Self::new(status).with_header("content-type", "text/html").with_body(body.into())
    }

    pub fn bytes(status: u16, mime: &str, body: impl Into<Bytes>) -> Self {
        Self::new(status).with_header("content-type", mime).with_body(body)
    }

    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }
}

/// One request the fake saw, kept so tests can assert on what we *would* have sent.
#[derive(Clone, Debug)]
pub struct RecordedCall {
    pub method: Method,
    pub url: String,
    /// Path only, no query — the same key routes match on.
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    /// Present for buffered bodies; `None` for streamed ones, which the fake does not read.
    pub body: Option<Bytes>,
}

impl RecordedCall {
    /// A query parameter's value, or `None`. Repeated keys return the first.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|p| {
            let (k, v) = p.split_once('=')?;
            (k == key).then_some(v)
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn body_str(&self) -> String {
        self.body.as_deref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
    }
}

type Handler = Box<dyn Fn(&RecordedCall) -> Canned + Send + Sync>;

struct Route {
    method: Method,
    path: String,
    handler: Handler,
}

/// In-memory transport for unit tests: match on `(method, path)`, return a canned response.
///
/// Routes match the **path only**, deliberately. Pagination varies only the query string, so a
/// query-sensitive match would force every pagination test to spell out three nearly identical
/// route keys; instead, [`FakeTransport::on_fn`] hands the whole call to a closure that can
/// branch on `?page=`.
pub struct FakeTransport {
    routes: Vec<Route>,
    fallback: Option<Canned>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl fmt::Debug for FakeTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeTransport").field("routes", &self.routes.len()).finish()
    }
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTransport {
    pub fn new() -> Self {
        Self { routes: Vec::new(), fallback: None, calls: Mutex::new(Vec::new()) }
    }

    /// Answer `(method, path)` with a fixed response, every time.
    pub fn on(self, method: Method, path: &str, canned: Canned) -> Self {
        self.on_fn(method, path, move |_| canned.clone())
    }

    /// Answer `(method, path)` from a closure that sees the whole request.
    pub fn on_fn<F>(mut self, method: Method, path: &str, f: F) -> Self
    where
        F: Fn(&RecordedCall) -> Canned + Send + Sync + 'static,
    {
        self.routes.push(Route { method, path: path.to_owned(), handler: Box::new(f) });
        self
    }

    /// Answer `(method, path)` with each response in turn; the last repeats once exhausted.
    ///
    /// Repeating rather than erroring is deliberate: a pagination bug that requests one page
    /// too many should be caught by an assertion on the item count or the call count, with a
    /// message about pagination — not by an opaque "no canned response left".
    pub fn on_sequence(self, method: Method, path: &str, replies: Vec<Canned>) -> Self {
        assert!(!replies.is_empty(), "on_sequence needs at least one reply");
        let n = Mutex::new(0usize);
        self.on_fn(method, path, move |_| {
            let mut i = n.lock().expect("fake transport counter");
            let idx = (*i).min(replies.len() - 1);
            *i += 1;
            replies[idx].clone()
        })
    }

    /// Answer anything unmatched. Without this, an unmatched request is a loud error naming the
    /// registered routes.
    pub fn fallback(mut self, canned: Canned) -> Self {
        self.fallback = Some(canned);
        self
    }

    /// Everything that was requested, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("fake transport call log").clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().expect("fake transport call log").len()
    }

    /// Calls matching a `(method, path)` pair, which is what most assertions want.
    pub fn calls_to(&self, method: &Method, path: &str) -> Vec<RecordedCall> {
        self.calls().into_iter().filter(|c| c.method == method && c.path == path).collect()
    }
}

/// Does this connect failure look like TLS talking to a plain-HTTP server?
///
/// rustls reports it as `received corrupt message of type InvalidContentType`: it read the first
/// bytes of an HTTP response where a TLS record should have been. The distinction is worth
/// drawing because the usual remedy for a failed connect is to check the port, and here the port
/// is right — it is `https://` that is wrong.
fn reads_as_plaintext(cause: &str) -> bool {
    let c = cause.to_ascii_lowercase();
    c.contains("invalidcontenttype")
        || c.contains("invalid content type")
        || (c.contains("corrupt message") && !c.contains("certificate"))
}

impl Transport for FakeTransport {
    fn execute(&self, req: HttpRequest) -> BoxFuture<'_, Result<Response>> {
        let (path, query) = split_path_query(&req.url);
        let call = RecordedCall {
            method: req.method.clone(),
            url: req.url.clone(),
            path: path.clone(),
            query,
            headers: req
                .headers
                .iter()
                .map(|(n, v)| {
                    (n.as_str().to_owned(), v.to_str().unwrap_or("<non-utf8>").to_owned())
                })
                .collect(),
            body: match &req.body {
                OutBody::Bytes { data, .. } => Some(data.clone()),
                _ => None,
            },
        };
        self.calls.lock().expect("fake transport call log").push(call.clone());

        let reply = self
            .routes
            .iter()
            .find(|r| r.method == req.method && r.path == path)
            .map(|r| (r.handler)(&call))
            .or_else(|| self.fallback.clone());

        Box::pin(async move {
            let Some(reply) = reply else {
                let known: Vec<String> =
                    self.routes.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
                return Err(Error::new(ErrorKind::Usage(format!(
                    "FakeTransport has no canned response for {} {path}; registered: [{}]",
                    req.method,
                    known.join(", ")
                ))));
            };
            let mut headers = HeaderMap::new();
            for (n, v) in &reply.headers {
                headers.append(
                    http::HeaderName::from_bytes(n.as_bytes()).expect("test header name"),
                    http::HeaderValue::from_str(v).expect("test header value"),
                );
            }
            Ok(Response::from_bytes(reply.status, headers, reply.body))
        })
    }

    fn name(&self) -> &'static str {
        "FakeTransport"
    }
}

/// Split an absolute URL into (path, query). Used only by the fake, so it stays simple.
fn split_path_query(url: &str) -> (String, String) {
    let after_scheme = url.split_once("://").map_or(url, |(_, r)| r);
    let from_path = after_scheme.find('/').map_or("/", |i| &after_scheme[i..]);
    let from_path = from_path.split('#').next().unwrap_or(from_path);
    match from_path.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (from_path.to_owned(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(url: &str) -> HttpRequest {
        HttpRequest {
            method: Method::GET,
            url: url.to_owned(),
            headers: HeaderMap::new(),
            body: OutBody::Empty,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn fake_matches_on_method_and_path_ignoring_query() {
        let t =
            FakeTransport::new().on(Method::GET, "/api/v1/user", Canned::json(200, r#"{"id":1}"#));
        let r = t.execute(get("https://h/api/v1/user?page=2")).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(&r.bytes().await.unwrap()[..], br#"{"id":1}"#);
        assert_eq!(t.calls()[0].query_param("page"), Some("2"));
    }

    /// An unmatched route must say what *was* registered. The alternative — a bare 404 — sends
    /// you hunting through client code for a bug that is a typo in the test.
    #[tokio::test]
    async fn unmatched_route_names_the_registered_routes() {
        let t = FakeTransport::new().on(Method::GET, "/api/v1/user", Canned::new(200));
        let e = t.execute(get("https://h/api/v1/orgs")).await.unwrap_err();
        let msg = format!("{:?}", e.kind());
        assert!(msg.contains("/api/v1/orgs"), "{msg}");
        assert!(msg.contains("GET /api/v1/user"), "{msg}");
    }

    #[tokio::test]
    async fn sequences_advance_then_repeat_the_last() {
        let t = FakeTransport::new().on_sequence(
            Method::GET,
            "/p",
            vec![Canned::text(200, "a"), Canned::text(200, "b")],
        );
        for expect in ["a", "b", "b"] {
            let r = t.execute(get("https://h/p")).await.unwrap();
            assert_eq!(String::from_utf8(r.bytes().await.unwrap().to_vec()).unwrap(), expect);
        }
    }

    /// A stdin body cannot be replayed; the retry loop keys off exactly this, and getting it
    /// wrong sends a truncated body on attempt two.
    #[test]
    fn stdin_bodies_are_not_replayable() {
        let stream = |src| OutBody::Stream {
            mime: "application/octet-stream".into(),
            src,
            progress: Progress::default(),
        };
        assert!(OutBody::Empty.is_replayable());
        assert!(stream(Source::Path("/tmp/x".into())).is_replayable());
        assert!(stream(Source::Bytes(vec![1, 2, 3])).is_replayable());
        assert!(!stream(Source::Stdin).is_replayable());
    }

    #[test]
    fn debug_of_a_request_redacts_the_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, "token s3cret-value-here".parse().unwrap());
        let req = HttpRequest {
            method: Method::GET,
            url: "https://h/api/v1/user?token=s3cret-value-here".into(),
            headers,
            body: OutBody::Bytes {
                mime: "application/json".into(),
                data: Bytes::from_static(b"{}"),
            },
            timeout: None,
        };
        let d = format!("{req:?}");
        assert!(!d.contains("s3cret"), "{d}");
    }

    #[test]
    fn host_extraction_handles_ports_credentials_and_subpaths() {
        assert_eq!(host_of("https://git.example.org/api/v1/user"), "git.example.org");
        assert_eq!(host_of("http://localhost:3000/api/v1"), "localhost:3000");
        assert_eq!(host_of("https://a:b@git.example.org/x"), "git.example.org");
        assert_eq!(split_port("localhost:3000"), ("localhost".to_owned(), 3000));
        assert_eq!(split_port("git.example.org"), ("git.example.org".to_owned(), 0));
    }
}

//! Test scaffolding shared by every group's unit tests.
//!
//! `http::Method` is deliberately never named here. `gea` does not depend on the `http` crate,
//! and taking a dependency purely so tests can spell one type would put a public crate in
//! `Cargo.toml` for the benefit of `#[cfg(test)]` code. [`on`] therefore takes the method as a
//! string and routes it through the same `Request` constructors the client uses, which is also
//! the shape a reader of the test wants: `on(fake, "POST", path, reply)`.
//!
//! A `#[cfg(test)]` module rather than a `tests/` file, because everything worth testing is
//! crate-private: the renderers take `&Term` and the fetchers take `&Api`, and an integration
//! test could only reach them by widening their visibility for no other reason.

use std::sync::Arc;

use gitea_client::Api;
use gitea_core::http::transport::{Canned, RecordedCall};
use gitea_core::http::{Auth, Client, FakeTransport, Request, RetryPolicy};
use serde_json::Value;

use super::emit::Emit;
use crate::global::GlobalOpts;
use crate::output::Term;

/// The base every fake-transport test that does not care about the host uses.
pub const BASE: &str = "https://forge.test";

/// An [`Api`] over a fake transport, at [`BASE`].
///
/// Retries are off so an assertion on the recorded call count measures the command's behaviour
/// rather than the retry policy's, and the 404 repository probe is off so it does not show up as
/// an extra recorded call.
pub fn api(fake: Arc<FakeTransport>) -> Api {
    api_at(BASE, fake)
}

/// The base the `repo`/`pr` wave's fixtures and snapshots use.
pub const EXAMPLE: &str = "https://git.example.org";

/// [`api`], at a named base, for the groups whose snapshots contain URLs.
pub fn api_at(base: &str, fake: Arc<FakeTransport>) -> Api {
    Api::new(
        Client::builder(base, Auth::token("t"))
            .transport(fake)
            .retry(RetryPolicy::none())
            .probe_404(false)
            .build()
            .expect("test client"),
    )
}

/// A transport whose *unmatched* requests fail loudly.
///
/// A silent fallback turns "the command built the wrong path" into "the command got an empty
/// answer", which is a much longer debugging session.
pub fn transport() -> FakeTransport {
    FakeTransport::new().fallback(Canned::json(599, r#"{"message":"no route in this test"}"#))
}

/// Register a canned reply for `(method, path)`.
pub fn on(fake: FakeTransport, method: &str, path: &str, reply: Canned) -> FakeTransport {
    fake.on(carrier(method).method, path, reply)
}

/// Register a closure that sees the whole request, for pagination and body assertions.
///
/// Prefer this to a canned reply whenever the command paginates: a canned reply ignores `?page`
/// and re-serves a full page for ever, so no termination rule can fire and the walk runs to its
/// cap. Answering off the **query** rather than off a call counter is the point — a real server
/// decides from the request it was given.
pub fn on_fn<F>(fake: FakeTransport, method: &str, path: &str, f: F) -> FakeTransport
where
    F: Fn(&RecordedCall) -> Canned + Send + Sync + 'static,
{
    fake.on_fn(carrier(method).method, path, f)
}

/// A 204, which is what most of Gitea's mutating endpoints answer with.
pub fn empty() -> Canned {
    Canned::new(204)
}

/// `http::Method` by inference, for a test that builds a `FakeTransport` by hand.
pub fn method<T: std::str::FromStr>(name: &str) -> T {
    name.parse().unwrap_or_else(|_| panic!("{name} is a literal method name"))
}

/// A canned JSON page that the paginator can *terminate* on.
///
/// A bare `Canned::json` repeats for ever, and `http::paginate` deliberately refuses to treat a
/// short page as the last one (Gitea clamps `limit` silently, so that rule loses data). A
/// `Link` header with no `rel="next"` is the authoritative end-of-collection signal, and Gitea
/// does send one — so a fake that omits it is not modelling the real server.
pub fn one_page(body: &str) -> Canned {
    Canned::json(200, body)
        .with_header("link", "<https://git.example.org/api/v1/x?page=1>; rel=\"first\"")
}

/// Answer page 1 with `first` and every later page with `[]`, from the request's own `?page`.
///
/// The other way to give a paginated fake an end. See [`on_fn`] for why this reads the query
/// rather than counting calls.
pub fn paged(call: &RecordedCall, first: &str) -> Canned {
    let page: usize = call.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    Canned::json(200, if page == 1 { first } else { "[]" })
}

/// Requests matching `(method, path)`.
pub fn calls(t: &FakeTransport, method: &str, path: &str) -> Vec<RecordedCall> {
    t.calls().into_iter().filter(|c| c.method == *method && c.path == path).collect()
}

/// The JSON body of the single request to `(method, path)`.
pub fn body(t: &FakeTransport, method: &str, path: &str) -> Value {
    let calls = calls(t, method, path);
    assert_eq!(calls.len(), 1, "expected exactly one {method} {path}, got {}", calls.len());
    serde_json::from_str(&calls[0].body_str()).unwrap_or_else(|e| {
        panic!("{method} {path} body was not JSON ({e}): {:?}", calls[0].body_str())
    })
}

/// A `GlobalOpts` asking for `--json` with the named fields, which is what a snapshot of machine
/// output wants: the whole document, in the API's own names.
pub fn json_globals(fields: &str) -> GlobalOpts {
    GlobalOpts { json: Some(fields.to_owned()), ..GlobalOpts::default() }
}

/// A colourless 100-column terminal. Wide enough that layout tests exercise real column widths
/// rather than truncation, and colourless so goldens are reviewable.
pub fn term() -> Term {
    Term::tty(100)
}

/// Run `body` with an [`Emit`] over a buffer, and return what it wrote to stdout.
pub fn captured(
    globals: &GlobalOpts,
    fields: Option<Vec<String>>,
    term: &Term,
    body: impl FnOnce(&mut Emit<'_>) -> gitea_core::Result<()>,
) -> String {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut emit = Emit::new(globals, fields, term, &mut buf).expect("output flags");
        body(&mut emit).expect("command body");
    }
    String::from_utf8(buf).expect("output is UTF-8")
}

/// A throwaway [`Request`] carrying the named method, so this module never names `http::Method`.
fn carrier(name: &str) -> Request {
    let mut req = Request::get("/");
    crate::api::set_method(&mut req, name).expect("test used a valid HTTP method");
    req
}

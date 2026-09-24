//! Complete Gitea API client, generated from the OpenAPI specification.
//!
//! # Getting started
//!
//! ```no_run
//! # async fn f() -> gitea_client::Result<()> {
//! use gitea_client::{Api, gitea_core::http::Auth};
//!
//! let api = Api::new(gitea_core::http::Client::new("https://codeberg.org", Auth::token("…"))?);
//! let pr = api.repo().get_pull_request("gitea", "gitea", 1).await?;
//! println!("{}", pr.title);
//! # Ok(()) }
//! ```
//!
//! [`Api`] is a thin borrow-and-dispatch wrapper: `api.repo()` hands back a zero-cost
//! [`ops::Repo`] holding a `&Client`, and every operation in the group hangs off it. One
//! accessor per API group, generated so that a group added upstream cannot be unreachable.
//!
//! # What the generated surface promises
//!
//! **Every generated method is concrete.** No type parameters, no `impl Trait` arguments, no
//! macros with logic. A body is `&gitea_model::CreateIssueOption`; a paginated collection is
//! `ItemStream<T>`; a download is `(Mime, ByteStream)`. All polymorphism lives in hand-written
//! [`gitea_core::http`]. With 506 operations, one generic parameter would monomorphise per
//! operation *and* per call site and make this crate's compile time superlinear in how much code
//! uses it.
#![forbid(unsafe_code)]
// Spec descriptions are prose written for the Swagger UI, not rustdoc. `ir/doc.rs` escapes `[`
// and `]` before emission, but a bare URL in a sentence is not something a generator can rewrite
// without mangling the sentence, and neither is worth failing a published crate's docs build
// over. The fix, if one is ever wanted, belongs in `ir/doc.rs` rather than here.
#![allow(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]

use futures::StreamExt;
use gitea_core::http::{Client, Paging, Request};

pub use gitea_core::{Error, Result};
// Re-exported so that one dependency on `gitea-client` is enough: every method signature names
// a `gitea_model` type, and a caller who cannot spell it cannot use the method.
pub use gitea_core;
pub use gitea_model;

pub mod meta_types;

// `#[path]` rather than a `generated/mod.rs` facade: every emitter owns its own subtree of
// `src/generated/`, and `xtask codegen` deletes and rewrites that tree wholesale. A shared
// `generated/mod.rs` would have to be produced by one emitter and edited by the others, which
// is exactly the kind of cross-emitter coupling the IR contract exists to prevent.
#[path = "generated/fields/mod.rs"]
pub mod fields;
#[path = "generated/meta/mod.rs"]
pub mod meta;
#[path = "generated/ops/mod.rs"]
pub mod ops;
#[path = "generated/query/mod.rs"]
pub mod query;
#[path = "generated/spec.rs"]
pub mod spec;

// The version this crate was generated against, at the crate root because every consumer wants
// it (a `User-Agent`, a `--version` line, a bug report) and none of them should have to know
// which generated module it landed in.
pub use spec::{SPEC_SHA256, SPEC_VERSION};

/// The typed API surface: one accessor per operation group.
///
/// Deliberately thin — a `Client` and nothing else. Everything interesting (auth, retry,
/// pagination, error classification) is in the `Client`; everything typed is generated. The
/// group accessors themselves are generated too, in `ops/mod.rs`, because a hand-written list of
/// seventeen of them is a list that silently goes stale: a group added upstream would arrive with
/// no way to reach it and nothing would fail.
///
/// `Api` borrows nothing and owns one `Arc` internally, so cloning it is cheap and the group
/// structs it hands out are plain `&Client` wrappers.
#[derive(Debug, Clone)]
pub struct Api {
    client: Client,
}

impl Api {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// The underlying client, for the runtime concerns no generated method exposes: `raw` for
    /// `-i/--include`, `value` for an untyped call, `capabilities`.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn into_client(self) -> Client {
        self.client
    }
}

impl From<Client> for Api {
    fn from(client: Client) -> Self {
        Self::new(client)
    }
}

// ------------------------------------------------------------- support for the generated methods
//
// Three helpers the generated code calls. They are hand-written and live here rather than being
// emitted, because they are *behaviour* rather than a rendering of the IR — and a bug in one of
// them should be fixable by editing one function instead of by re-running the generator.

/// Collect a `text/plain` or `text/html` response into a `String`.
///
/// Lossy on purpose. `GET /repos/{o}/{r}/pulls/{i}.diff` is declared `text/plain` and can
/// perfectly well contain a binary file's bytes; failing the whole command with a UTF-8 error
/// would be worse than a replacement character in a hunk nobody was going to read.
pub(crate) async fn text(client: &Client, req: Request) -> Result<String> {
    let (_, mut body) = client.bytes(req).await?;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Prepare a request for `Client::items`, which walks the whole collection.
///
/// The page parameter is dropped: the paginator owns it, and a value left over from a query
/// struct would make the walk start at page 5 and then continue at page 2 — silently skipping and
/// then repeating items. A caller who wants one specific page wants the `_page` method.
pub(crate) fn stream_request(mut req: Request, page_param: &str) -> Request {
    req.query.retain(|(k, _)| k.as_ref() != page_param);
    req
}

/// Apply a [`Paging`] to a single-page request.
///
/// Both fields reach the wire, as the one page size that satisfies them: `per_page` is the page
/// size asked for, and `limit` — a cap on items across *all* pages — can only mean "do not send
/// me more than this", so it lowers the page size rather than being silently ignored or
/// truncating a page whose `PageInfo` would then disagree with it.
///
/// Unlike `Client::items`, this does **not** clamp against `max_response_items`: that costs a
/// `/settings/api` round trip, and it exists to disambiguate a short page — which a caller
/// reading `PageInfo` itself does not need.
pub(crate) fn page_request(mut req: Request, paging: Paging) -> Request {
    let per_page = match (paging.per_page, paging.limit) {
        (Some(p), Some(l)) => Some(p.min(u32::try_from(l).unwrap_or(u32::MAX))),
        (Some(p), None) => Some(p),
        (None, Some(l)) => Some(u32::try_from(l).unwrap_or(u32::MAX)),
        (None, None) => None,
    };
    if let Some(n) = per_page {
        // Zero would ask for an empty page, which no caller means by `--limit 0`.
        req.set_query("limit", n.max(1));
    }
    req
}

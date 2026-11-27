//! Types for the Gitea API, generated from the OpenAPI specification.
//!
//! Everything under [`generated`] is produced by `cargo xtask codegen` from the version-pinned
//! spec in `spec/`, and re-exported here so that the useful path is short:
//! [`PullRequest`], not `generated::models::pull_request::PullRequest`.
//!
//! ## What these types promise
//!
//! They are **tolerant of a server newer than this build**, which matters because the crate is
//! pinned to one Gitea release and users point it at whatever their instance runs:
//!
//! - Unknown fields are ignored — no struct uses `deny_unknown_fields`.
//! - Missing fields fall back to a zero value: every struct carries `#[serde(default)]`, so `{}`
//!   deserializes and an endpoint that stops sending a field does not fail the request.
//! - Unknown *enum values* deserialize into an `Unknown` arm and re-serialize verbatim (see
//!   [`open_enum`]), so a read-modify-write cannot corrupt a value this build does not
//!   understand.
//! - Absent timestamps are `None`, including Go's zero time `"0001-01-01T00:00:00Z"`, which
//!   Gitea sends for things like `merged_at` on an unmerged pull request.
//!
//! Tolerant is not silent: unknown enum values and unparseable values are recorded in
//! [`gitea_core::error::compat`], which `gea` drains into one grouped note at exit.
//!
//! ## What they do not promise
//!
//! `Option` is kept only where "absent" and "zero" mean different things — timestamps and nested
//! objects. Scalars, `Vec`s and maps collapse to their zero value, because
//! `String::new()` for an absent description is harmless and `Option<Vec<_>>` would put
//! `.unwrap_or_default()` at every call site.
#![forbid(unsafe_code)]
// Every doc comment in this crate is a Gitea spec description, written for the Swagger UI
// rather than for rustdoc. Lowering (`xtask/src/ir/doc.rs`) already escapes `[` and `]` so a
// mention of `[owner]` is not read as an intra-doc link; these two allows cover what it does
// not:
//
//   * `bare_urls` — `RegisterRunnerOptions.ephemeral` cites a docs URL in prose, and one
//     description in 250 models is not worth teaching the generator to rewrite sentences.
//     Wrapping bare URLs in `<>` belongs next to the bracket escaping in `ir/doc.rs`; until
//     then this is the honest place to say so.
//   * `broken_intra_doc_links` — a description may contain something that still parses as a
//     link target after escaping.
//
// Neither is worth failing a published crate's docs build over, and both are prose-level, so
// nothing about the API surface is being hidden here.
#![allow(rustdoc::bare_urls, rustdoc::broken_intra_doc_links)]

pub mod de;
pub mod generated;
pub mod open_enum;

pub use generated::*;

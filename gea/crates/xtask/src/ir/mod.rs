//! The IR — **the contract between lowering and the emitters.**
//!
//! Every decision is made in [`lower`]: name mangling, `$ref` resolution, `Presence`, doc
//! cleanup, path tokenization, cycle detection. The emitters are dumb by rule:
//!
//! - **No emitter may read `overrides.toml`.** Two emitters consulting the override table could
//!   disagree about the same name.
//! - **No emitter may do string casing.** Names come from [`names`] or not at all.
//! - **No emitter may resolve a `$ref`.** By the time a [`RustType`] exists, the reference is
//!   already a decision.
//!
//! The payoff is *structural* consistency rather than reviewed consistency: a layer-2 flag name
//! and the corresponding client function's parameter name are both read off the same [`Param`]
//! value, so they cannot drift. That property is what makes 42k lines of generated code
//! reviewable by reading the IR instead.

pub mod doc;
pub mod lower;
pub mod names;
pub mod paths;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use doc::Doc;
use names::Ident;
use paths::PathTemplate;
use types::{Mime, Presence, RustType};

#[derive(Debug, Serialize)]
pub struct Ir {
    /// Gitea version the spec came from, e.g. `1.27.2`.
    pub spec_version: String,
    /// sha256 of the canonical spec JSON. Stamped into every generated file header so that
    /// "which spec produced this line" is answerable from the tree — a timestamp or a hostname
    /// would make the output non-reproducible for no benefit.
    pub spec_sha256: String,
    pub models: Vec<Model>,
    pub open_enums: Vec<OpenEnum>,
    pub operations: Vec<Operation>,
    pub groups: Vec<Group>,
    /// Definitions whose `$ref` graph contains a cycle through them, and therefore have at
    /// least one [`Presence::OptionalBoxed`] field. Reported so a spec bump that introduces a
    /// new cycle is visible rather than merely compiling differently.
    pub cyclic_models: BTreeSet<String>,
}

/// A layer-2 command group: `gea raw <name> <command>`.
#[derive(Debug, Serialize)]
pub struct Group {
    pub name: String,
    pub module: Ident,
    pub doc: String,
    /// Indices into [`Ir::operations`], in command order.
    pub operations: Vec<usize>,
}

// ------------------------------------------------------------------------------------- models

#[derive(Debug, Serialize)]
pub struct Model {
    /// Definition name as the spec writes it, e.g. `PullRequest`.
    pub wire: String,
    pub rust: Ident,
    pub kind: ModelKind,
    pub doc: Doc,
    /// The file this type is emitted into. One type per file: 246 files of 20–120 lines, each
    /// with its own git history, beats one 9,000-line file whose blame is useless.
    pub module: Ident,
}

#[derive(Debug, Serialize)]
pub enum ModelKind {
    Struct(Vec<Field>),
    /// A definition that is a bare scalar or collection, e.g. `Duration` (`int64`) or
    /// `QuotaGroupList` (`[QuotaGroup]`). Emitted as `pub type X = …`.
    Alias(RustType),
    /// A `type: object` with no properties: `ForgeLike`, `ForgeOutbox`. There is nothing to
    /// type, so `serde_json::Value` is the honest answer.
    FreeForm,
    /// A definition that is a named string type — Gitea's Go `type StateType string`. The
    /// spec carries no variants, so they come from `overrides.toml [enum_values]`.
    OpenEnum(String),
    /// An untagged `One(T) / Many(Vec<T>)` enum, synthesized from `overrides.toml
    /// [one_or_many]` for a route whose response shape depends on the request rather than on
    /// the declared schema. Not a definition in the spec at all.
    OneOrMany(RustType),
}

#[derive(Debug, Serialize)]
pub struct Field {
    /// JSON key on the wire.
    pub wire: String,
    pub rust: Ident,
    /// Whether `#[serde(rename = …)]` is needed. Computed here so the emitter does not have to
    /// compare strings and reach a different conclusion.
    pub needs_rename: bool,
    pub ty: RustType,
    pub presence: Presence,
    pub doc: Doc,
    pub deprecated: bool,
    /// True when the spec listed this field in `required` but `overrides.toml` demoted it. Kept
    /// for `--dump-ir`, so a demotion is visible rather than inferred from its absence.
    pub required_demoted: bool,
}

/// A generated open enum: tolerant of values this build has never heard of.
#[derive(Debug, Serialize)]
pub struct OpenEnum {
    pub name: Ident,
    pub doc: Doc,
    pub variants: Vec<EnumVariant>,
    /// Where the variants came from, for `--dump-ir`: the spec, or a curated override.
    pub curated: bool,
    /// The `Model.field` or definition name this enum was derived from.
    pub origin: String,
}

#[derive(Debug, Serialize)]
pub struct EnumVariant {
    pub wire: String,
    pub rust: Ident,
}

// --------------------------------------------------------------------------------- operations

#[derive(Debug, Serialize)]
pub struct Operation {
    pub op_id: String,
    /// Layer-2 group, as typed.
    pub group: String,
    /// Layer-2 command, kebab-case.
    pub command: String,
    /// Module the client function lives in.
    pub module: Ident,
    /// Sub-module within the group, for splitting oversized groups into files. The 198-operation
    /// `repo` group would otherwise bust the 1500-line-per-file cap several times over.
    pub sub_bucket: String,
    pub fn_name: Ident,
    pub method: crate::swagger::HttpMethod,
    pub path: PathTemplate,
    /// In path order — these become positional arguments, so the order is public API.
    pub path_params: Vec<Param>,
    /// Sorted by wire name, for a stable `<OpPascal>Query` struct and stable `--help` output.
    pub query_params: Vec<Param>,
    pub form_data: Vec<Param>,
    pub body: Option<Body>,
    pub consumes: Option<Mime>,
    pub produces: Vec<Mime>,
    pub success: Success,
    pub pagination: Pagination,
    /// The token scope this operation needs, e.g. `write:repository`. Feeds the
    /// `InsufficientScope` error message, which has to name a scope the user can actually
    /// create — Gitea scopes are fixed at token creation time.
    pub scope: Option<String>,
    pub doc: Doc,
    /// `Some(note)` when the spec marks the operation deprecated.
    pub deprecated: Option<String>,
    /// Name of the `<OpPascal>Query` struct, when there are query parameters. Query parameters
    /// always collapse into one struct: several operations take more than a dozen, and a
    /// 15-argument function is not an API.
    pub query_struct: Option<Ident>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PathEncoding {
    /// Percent-encoded with the path-segment set. `/` becomes `%2F`, which is what protects us
    /// from an owner literally named `a/b`.
    Segment,
    /// `/` is preserved. For parameters that legitimately contain a path — get this wrong and
    /// `gea raw repo get-contents o r src/main.rs` 404s with no hint why.
    PathLike,
}

/// Which piece of resolved context can fill a parameter the user did not supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CtxFill {
    Owner,
    Repo,
    Branch,
}

#[derive(Debug, Serialize)]
pub struct Param {
    /// Name on the wire, used verbatim in the query string or path.
    pub wire: String,
    /// Rust parameter or struct field name.
    pub rust: Ident,
    /// Layer-2 flag, without the leading `--`.
    pub flag: String,
    pub ty: RustType,
    pub required: bool,
    pub encoding: PathEncoding,
    pub ctx_fill: Option<CtxFill>,
    /// Known values, for completion and help text. **Suggestions, not validation**: the server
    /// may accept values this spec does not list, and rejecting them locally would make the CLI
    /// less capable than `curl`.
    pub enum_values: Option<Vec<String>>,
    /// `collectionFormat: multi` — repeat the key rather than joining values. This is why
    /// `Request::query` is an ordered `Vec` and not a map.
    pub repeated: bool,
    pub default: Option<String>,
    pub doc: Doc,
}

#[derive(Debug, Serialize)]
pub struct Body {
    pub ty: RustType,
    pub required: bool,
    /// Body fields flattened to depth 1 as typed layer-2 flags. 124 of 125 request bodies are
    /// `$ref`s to named definitions, which is what makes this possible at all.
    pub flat: Vec<FlatField>,
    /// Wire names excluded from flattening — nested objects and arrays of objects. `--help`
    /// names them and points at `--body-file`, because silently dropping a field would be
    /// worse than admitting the limit.
    pub deep: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FlatField {
    pub wire: String,
    pub flag: String,
    pub ty: RustType,
    pub required: bool,
    pub enum_values: Option<Vec<String>>,
    pub doc: Doc,
}

#[derive(Debug, Serialize)]
pub struct Success {
    /// The lowest 2xx status the operation declares.
    pub status: u16,
    pub ty: RustType,
    /// Name of the shared `#/responses/…` entry, when the operation refers to one. The
    /// `FooList` / `FooListWithoutPagination` convention in these names is the *only* in-spec
    /// signal that an endpoint paginates.
    pub response_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub enum Pagination {
    None,
    /// The operation takes a `page` parameter. Note this drives *help text and method shape*
    /// only: the spec declares no `Link` header anywhere, so the runtime decides when to stop
    /// by reading the actual response headers.
    Paged {
        page_param: String,
        limit_param: Option<String>,
        /// True when the response name lacks the `WithoutPagination` suffix, i.e. the shared
        /// response is the paginated flavour.
        shared_response_paginated: bool,
    },
}

impl Ir {
    /// A stable, human-readable dump. Read by whoever is writing an emitter, and by anyone
    /// asking "what did lowering decide about this operation".
    ///
    /// JSON rather than `Debug` because it can be piped through `jq`, which is what someone
    /// staring at 506 operations actually wants to do.
    pub fn dump(&self) -> String {
        let mut s = serde_json::to_string_pretty(self)
            .unwrap_or_else(|e| format!("{{\"error\": \"IR is not serializable: {e}\"}}"));
        s.push('\n');
        s
    }

    pub fn operation(&self, op_id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.op_id == op_id)
    }

    /// `(module, fn_name)` and `(group, command)` counts, for the `--check-names` summary.
    pub fn name_counts(&self) -> (usize, usize) {
        let fns: BTreeSet<(&str, &str)> =
            self.operations.iter().map(|o| (o.module.as_str(), o.fn_name.as_str())).collect();
        let cmds: BTreeSet<(&str, &str)> =
            self.operations.iter().map(|o| (o.group.as_str(), o.command.as_str())).collect();
        (fns.len(), cmds.len())
    }

    /// Operation count per group, for the group summary in `--check-names`.
    pub fn group_sizes(&self) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for op in &self.operations {
            *out.entry(op.group.as_str()).or_default() += 1;
        }
        out
    }
}

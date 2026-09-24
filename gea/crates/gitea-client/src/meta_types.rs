//! The metadata vocabulary that drives layer 2 — every one of the 506 operations as a
//! command — plus `--json` field discovery.
//!
//! # Why this is data and not types
//!
//! The obvious way to expose 506 operations as CLI commands is 506 `#[derive(clap::Args)]`
//! structs. That does not work here, for one disqualifying reason: clap's derive path builds
//! the *entire* command tree eagerly inside `Parser::parse()`. With ~3,000 flags across 506
//! subcommands, that is thousands of `Arg` constructions, with their allocations, on **every
//! invocation — including `gea --version`**. Five to twenty milliseconds spent building a
//! parser the user will never reach, in a tool whose whole pre-network budget should be under
//! twenty. The derive approach also costs minutes of compile time and megabytes of binary.
//!
//! So the generator emits these `const` tables instead. They are data, not generics: they
//! compile in seconds and land in `.rodata`. `gea-raw` then peeks at `argv`, and builds
//! `clap::Command` objects at runtime for **only the subtree the user actually named** — ten
//! group stubs for `gea raw --help`, one full command for `gea raw repo create-pull-request`.
//!
//! Everything here is `&'static` for that reason. Nothing in this module allocates.

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum In {
    Path,
    Query,
    /// A `multipart/form-data` field. Only four parameters in the whole API use this.
    FormData,
}

/// How a path parameter is percent-encoded.
///
/// This distinction is not cosmetic. Parameters like `filepath` and `ref` legitimately contain
/// `/` — `GET /repos/{owner}/{repo}/contents/{filepath}` is called with `src/main.rs`. Encoding
/// that slash produces `src%2Fmain.rs` and a 404 for every nested file in every repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathEncoding {
    /// Percent-encode with the path-segment set, including `/`. The default.
    Segment,
    /// Preserve `/`: the value is a path, not a single segment.
    PathLike,
}

/// A parameter that can be filled from resolved repository context instead of being typed.
///
/// This is what lets `gea raw repo list-pull-requests` work inside a clone with no arguments
/// at all, the same way `-R/--repo` is optional for the porcelain commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxFill {
    Owner,
    Repo,
}

/// The value shape of a parameter or body field, for clap parsing and `--json` typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueTy {
    Bool,
    Int,
    Float,
    Str,
    DateTime,
    /// A repeated flag collecting into a list.
    List,
    /// Nested JSON that cannot be expressed as a flag; supply via `--body-file`.
    Json,
    /// A file path whose contents are uploaded.
    File,
}

/// What an operation returns, which decides how output is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Produces {
    Json,
    /// `application/ld+json`, from the ActivityPub endpoints.
    LdJson,
    Text,
    Html,
    /// `application/zip`, `application/octet-stream`, `application/gzip`. Streamed to a file;
    /// refused to a terminal without `--force`.
    Bytes,
    /// A 204-style response with no body.
    Empty,
}

/// A path, query, or form parameter.
#[derive(Debug, Clone, Copy)]
pub struct ParamMeta {
    /// The name the API uses. Also the name used in `--json` output, since we do no case
    /// translation anywhere.
    pub wire: &'static str,
    /// The long flag, kebab-cased from `wire`.
    pub flag: &'static str,
    pub location: In,
    pub ty: ValueTy,
    pub required: bool,
    /// True for array-valued query params, which clap should accept repeatedly.
    pub repeatable: bool,
    pub encoding: PathEncoding,
    pub ctx_fill: Option<CtxFill>,
    /// Values the spec listed. Used to *suggest* completions, never to reject input — a
    /// server may accept values our pinned spec does not know about.
    pub enum_values: &'static [&'static str],
    /// First sentence of the spec description, for clap's `about`.
    pub help: &'static str,
}

/// One request-body field, flattened to depth 1 and exposed as a flag.
#[derive(Debug, Clone, Copy)]
pub struct BodyField {
    /// JSON pointer into the request body, e.g. `/title`. Flags are merged into a
    /// `--body-file` base object by pointer, which is what makes the two composable.
    pub pointer: &'static str,
    pub flag: &'static str,
    pub ty: ValueTy,
    pub required: bool,
    pub enum_values: &'static [&'static str],
    pub help: &'static str,
}

/// The request body of an operation.
///
/// 124 of the 125 request bodies in the API are `$ref`s to named definitions with typed
/// properties, which is precisely why per-field flags are possible at all.
#[derive(Debug, Clone, Copy)]
pub struct BodyMeta {
    /// The model type name, for help text and `--dry-run` output.
    pub type_name: &'static str,
    pub required: bool,
    pub content_type: &'static str,
    /// Depth-1 scalar and list fields, available as flags.
    pub fields: &'static [BodyField],
    /// Names of fields too deeply nested (or arrays of objects) to express as flags. Their
    /// presence makes `--help` say so and point at `--body-file`.
    pub deep: &'static [&'static str],
}

/// Whether and how an operation paginates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pagination {
    None,
    /// Takes `page` and `limit` query parameters.
    ///
    /// Note this flag only decides whether `--paginate` and `--limit` appear in help. The
    /// runtime keys actual page-following off the response's `Link` header, because the
    /// specification declares no response headers at all and therefore cannot be trusted on
    /// this point.
    Paged,
}

/// One API operation.
#[derive(Debug, Clone, Copy)]
pub struct OpMeta {
    /// The spec's `operationId`, verbatim. The stable identity of this operation.
    pub op_id: &'static str,
    /// Layer-2 group, e.g. `repo`. Locked in `spec/name-lock.toml`.
    pub group: &'static str,
    /// Layer-2 leaf command, e.g. `create-pull-request`. Locked in `spec/name-lock.toml`.
    pub command: &'static str,
    pub method: &'static str,
    /// The path template, with `{param}` placeholders.
    pub path: &'static str,
    /// First sentence of the spec summary.
    pub summary: &'static str,
    /// Full description, for `--help`'s long form.
    pub description: &'static str,
    pub params: &'static [ParamMeta],
    pub body: Option<&'static BodyMeta>,
    pub pagination: Pagination,
    pub produces: Produces,
    /// The token scope this operation needs, e.g. `write:issue`. Derived from the operation's
    /// tag and HTTP method. Lets a 403 name a scope that actually exists in Gitea's
    /// `read:`/`write:` vocabulary rather than guessing at GitHub's.
    pub scope: Option<&'static str>,
    /// Set when the spec marks the operation deprecated, carrying the note.
    pub deprecated: Option<&'static str>,
}

impl OpMeta {
    pub fn path_params(&self) -> impl Iterator<Item = &'static ParamMeta> {
        self.params.iter().filter(|p| p.location == In::Path)
    }

    pub fn query_params(&self) -> impl Iterator<Item = &'static ParamMeta> {
        self.params.iter().filter(|p| p.location == In::Query)
    }

    pub fn form_params(&self) -> impl Iterator<Item = &'static ParamMeta> {
        self.params.iter().filter(|p| p.location == In::FormData)
    }

    /// `true` when this operation uploads a file, i.e. needs a multipart request.
    pub fn is_upload(&self) -> bool {
        self.params.iter().any(|p| p.ty == ValueTy::File)
    }
}

/// A layer-2 command group.
#[derive(Debug, Clone, Copy)]
pub struct GroupMeta {
    pub name: &'static str,
    pub about: &'static str,
    /// Indices into the `OPS` table. Contiguous, because `OPS` is sorted by
    /// `(group, command)` — so building one group's command tree is a slice, not a scan.
    pub first: usize,
    pub len: usize,
}

/// A field available to `--json`.
///
/// Only top-level fields are offered. Nested access is `--jq`'s job, the same division `gh`
/// makes, which keeps the field list short enough to read.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// snake_case, verbatim from the API. There is no case translation anywhere in this tool.
    pub name: &'static str,
    pub kind: FieldKind,
    pub doc: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    Bool,
    Int,
    Float,
    Str,
    DateTime,
    Enum(&'static [&'static str]),
    Object(&'static [FieldSpec]),
    Array(&'static FieldKind),
    Map(&'static FieldKind),
    Json,
}

impl FieldKind {
    /// The label shown beside a field name when `--json` lists available fields on a TTY.
    pub fn label(&self) -> &'static str {
        match self {
            FieldKind::Bool => "bool",
            FieldKind::Int => "int",
            FieldKind::Float => "float",
            FieldKind::Str => "string",
            FieldKind::DateTime => "datetime",
            FieldKind::Enum(_) => "enum",
            FieldKind::Object(_) => "object",
            FieldKind::Array(_) => "array",
            FieldKind::Map(_) => "map",
            FieldKind::Json => "json",
        }
    }
}

/// Lookups over the generated tables.
///
/// These take the tables as arguments rather than reading globals so that they are testable
/// without the generated code, which lets `gea-raw` be developed and unit-tested before the
/// emitters exist.
pub mod lookup {
    use super::{GroupMeta, OpMeta};

    /// Find an operation by group and command. `ops` must be sorted by `(group, command)`.
    pub fn op<'a>(ops: &'a [OpMeta], group: &str, command: &str) -> Option<&'a OpMeta> {
        ops.binary_search_by(|o| (o.group, o.command).cmp(&(group, command))).ok().map(|i| &ops[i])
    }

    /// Find an operation by its spec `operationId`. Linear, because `OPS` is sorted by
    /// `(group, command)` rather than by id; only used on diagnostic paths.
    pub fn op_by_id<'a>(ops: &'a [OpMeta], op_id: &str) -> Option<&'a OpMeta> {
        ops.iter().find(|o| o.op_id == op_id)
    }

    pub fn group<'a>(groups: &'a [GroupMeta], name: &str) -> Option<&'a GroupMeta> {
        groups.binary_search_by(|g| g.name.cmp(name)).ok().map(|i| &groups[i])
    }

    /// The operations belonging to a group, as a slice. Contiguous because `OPS` is sorted by
    /// `(group, command)`.
    pub fn ops_in<'a>(ops: &'a [OpMeta], g: &GroupMeta) -> &'a [OpMeta] {
        &ops[g.first..g.first + g.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &[ParamMeta] = &[ParamMeta {
        wire: "owner",
        flag: "owner",
        location: In::Path,
        ty: ValueTy::Str,
        required: true,
        repeatable: false,
        encoding: PathEncoding::Segment,
        ctx_fill: Some(CtxFill::Owner),
        enum_values: &[],
        help: "owner of the repo",
    }];

    fn op(group: &'static str, command: &'static str) -> OpMeta {
        OpMeta {
            op_id: "x",
            group,
            command,
            method: "GET",
            path: "/x",
            summary: "",
            description: "",
            params: P,
            body: None,
            pagination: Pagination::None,
            produces: Produces::Json,
            scope: None,
            deprecated: None,
        }
    }

    #[test]
    fn binary_search_finds_ops_in_sorted_order() {
        // The generator must emit OPS sorted by (group, command); lookup::op relies on it.
        let ops = [op("issue", "list"), op("repo", "create"), op("repo", "get")];
        assert_eq!(lookup::op(&ops, "repo", "get").map(|o| o.command), Some("get"));
        assert!(lookup::op(&ops, "repo", "nope").is_none());
        assert!(lookup::op(&ops, "zzz", "get").is_none());
    }

    #[test]
    fn a_group_is_a_contiguous_slice() {
        // This is why building one group's clap tree is O(group), not O(506).
        let ops = [op("issue", "list"), op("repo", "create"), op("repo", "get")];
        let g = GroupMeta { name: "repo", about: "", first: 1, len: 2 };
        let slice = lookup::ops_in(&ops, &g);
        assert_eq!(slice.len(), 2);
        assert!(slice.iter().all(|o| o.group == "repo"));
    }

    #[test]
    fn path_params_are_filtered_by_location() {
        let o = op("repo", "get");
        assert_eq!(o.path_params().count(), 1);
        assert_eq!(o.query_params().count(), 0);
    }
}

//! The `meta` emitter: layer 2's `&'static` command tables.
//!
//! Output is [`gitea_client::meta`](../../../gitea-client/src/generated/meta/mod.rs):
//! a flat `OPS: &[OpMeta]` covering all 482 operations, a `GROUPS: &[GroupMeta]` index into
//! it, and one `ops_*.rs` file per group holding the `const OpMeta` initializers.
//!
//! # Why data instead of 482 derived clap structs
//!
//! See the module docs on `gitea_client::meta_types`. The short version: clap's derive path
//! builds the *entire* command tree inside `Parser::parse()`, so 482 `Args` structs would cost
//! thousands of `Arg` allocations on every invocation including `gea --version`. These tables
//! are plain data — they land in `.rodata` and cost nothing until `gea-raw` reads the one
//! subtree the user named.
//!
//! # The two invariants `gea-raw` cannot function without
//!
//! 1. **`OPS` is sorted by `(group, command)`.** `lookup::op` binary-searches it. An unsorted
//!    table does not error; it silently fails to find roughly half the commands.
//! 2. **`GroupMeta::{first, len}` is a correct, contiguous slice of `OPS`.** `lookup::ops_in`
//!    hands that slice straight to the clap builder, so a wrong index puts another group's
//!    operations under `gea raw repo`.
//!
//! Both are asserted by the generated `meta/invariants.rs` test module against the committed
//! table, not merely arranged for here — the assertion is worth more than the arrangement,
//! because it survives a future refactor of this file.
//!
//! # A note on the const names
//!
//! The per-operation const names (`OP_CREATE_PULL_REQUEST`) are uppercased from the `Ident`
//! that `ir::names` already produced. That is not the string casing the IR contract forbids:
//! the forbidden kind derives a *public* name from a spec string, where two emitters could
//! reach different conclusions. These symbols are `pub(super)`, referenced only from the
//! `mod.rs` this same function writes, and appear in no public API and in no `name-lock.toml`.

use std::collections::BTreeMap;

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

use super::{GeneratedFile, MAX_LINES, render};
use crate::Result;
use crate::ir::types::{Mime, RustType};
use crate::ir::{CtxFill, Ir, Operation, Pagination, Param, PathEncoding};

/// Line budget a single `ops_*.rs` file aims for, below [`MAX_LINES`] on purpose.
///
/// The headroom matters: splitting is automatic, so a group that sits at 1,499 lines today
/// would fan out into a dozen letter-keyed files the moment upstream adds one operation, and
/// that diff is unreadable for a one-operation change. Budgeting at 80% of the cap means a
/// group has to grow by a quarter before its file layout moves.
const BUDGET: usize = MAX_LINES * 4 / 5;

pub fn emit(ir: &Ir) -> Result<Vec<GeneratedFile>> {
    let enums = enum_table(ir);

    // `lower` sorts operations by `(group, command)`, which is exactly the order `OPS` needs.
    // Re-check rather than trust: if lowering ever stops sorting, every binary search in
    // `gea-raw` starts missing commands, and nothing else in the build would notice.
    for w in ir.operations.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if (a.group.as_str(), a.command.as_str()) >= (b.group.as_str(), b.command.as_str()) {
            bail!(
                "ir.operations is not sorted by (group, command): {:?} then {:?}. \
                 `lookup::op` binary-searches OPS, so an unsorted table silently fails to find \
                 commands rather than erroring.",
                (&a.group, &a.command),
                (&b.group, &b.command),
            );
        }
    }

    let assignment = assign_files(ir, &enums)?;

    let mut files = Vec::new();
    for (key, ops) in &assignment.files {
        let tokens = ops_file(ir, key, ops, &enums)?;
        files.push(GeneratedFile::new(
            path(&format!("{key}.rs")),
            render(tokens, &ir.spec_version, &ir.spec_sha256)?,
        ));
    }

    files.push(GeneratedFile::new(
        path("mod.rs"),
        render(mod_file(ir, &assignment)?, &ir.spec_version, &ir.spec_sha256)?,
    ));
    files.push(GeneratedFile::new(
        path("invariants.rs"),
        render(invariants_file(), &ir.spec_version, &ir.spec_sha256)?,
    ));

    Ok(files)
}

fn path(leaf: &str) -> String {
    format!("crates/gitea-client/src/generated/meta/{leaf}")
}

// --------------------------------------------------------------------------- file assignment

/// Which file each operation lands in, plus the group index into `OPS`.
struct Assignment {
    /// File stem (`ops_repo_pulls`) → operation indices into `Ir::operations`, in `OPS` order.
    files: BTreeMap<String, Vec<usize>>,
    /// File stem an operation was assigned, indexed the same as `Ir::operations`.
    owner: Vec<String>,
    groups: Vec<GroupEntry>,
}

/// One `GroupMeta` row, resolved before any tokens are written because `mod.rs` needs the
/// indices and the file list at the same time.
#[derive(Debug, PartialEq, Eq)]
struct GroupEntry {
    name: String,
    about: String,
    first: usize,
    len: usize,
}

/// Resolves the file layout, splitting a group whose file would bust [`BUDGET`].
///
/// The primary key is the IR's own `(group, sub_bucket)`, so `repo`'s 177 operations land in
/// the eleven buckets `ir::lower::sub_bucket` derives from the API's own URL structure — a
/// reader looking for `create-pull-request` looks under `ops_repo_pulls*`, which tracks the API
/// rather than an arbitrary split.
///
/// Six of those buckets (`issue`, `org`, `user`, `admin`, `repo_misc`, `repo_pulls`) still
/// exceed the budget, so they get a second split on the command's first letter. That key was
/// chosen over a greedy line-count packing because it is *stable*: a new `list-whatever`
/// command lands in `ops_user_l.rs` and moves nothing else, whereas a packed layout reshuffles
/// every file after the insertion point and buries the one real change in the noise.
fn assign_files(ir: &Ir, enums: &EnumTable) -> Result<Assignment> {
    let mut by_primary: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, op) in ir.operations.iter().enumerate() {
        by_primary.entry(primary_key(op)).or_default().push(i);
    }

    let mut files: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (key, ops) in by_primary {
        if measure(ir, &key, &ops, enums)? <= BUDGET {
            files.insert(key, ops);
            continue;
        }
        // Second pass: split this group by the command's first letter.
        let mut by_letter: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for &i in &ops {
            by_letter.entry(letter_key(&key, &ir.operations[i])?).or_default().push(i);
        }
        for (leaf, ops) in by_letter {
            let lines = measure(ir, &leaf, &ops, enums)?;
            if lines > MAX_LINES {
                bail!(
                    "{leaf}.rs would be {lines} lines, over the {MAX_LINES}-line review cap, \
                     even after splitting {key} by the command's first letter. Add a \
                     `sub_bucket` rule for this group in crates/xtask/src/ir/lower.rs rather \
                     than raising the cap: the point of committing generated code is that its \
                     diff is reviewable."
                );
            }
            files.insert(leaf, ops);
        }
    }

    let mut owner = vec![String::new(); ir.operations.len()];
    for (key, ops) in &files {
        for &i in ops {
            owner[i] = key.clone();
        }
    }

    Ok(Assignment { groups: groups(ir), files, owner })
}

fn primary_key(op: &Operation) -> String {
    if op.sub_bucket == "mod" {
        format!("ops_{}", op.module.as_str())
    } else {
        format!("ops_{}_{}", op.module.as_str(), op.sub_bucket)
    }
}

fn letter_key(primary: &str, op: &Operation) -> Result<String> {
    let Some(c) = op.command.chars().next().filter(char::is_ascii_alphabetic) else {
        bail!(
            "command {:?} does not start with an ASCII letter, so it cannot be keyed to a \
             letter file. Pin the operation's name in overrides.toml.",
            op.command
        );
    };
    Ok(format!("{primary}_{}", c.to_ascii_lowercase()))
}

/// The rendered line count of a candidate file. Rendering is the only honest measurement:
/// `prettyplease` decides where the line breaks go, not this emitter.
fn measure(ir: &Ir, key: &str, ops: &[usize], enums: &EnumTable) -> Result<usize> {
    let tokens = ops_file(ir, key, ops, enums)?;
    Ok(render(tokens, &ir.spec_version, &ir.spec_sha256)?.lines().count())
}

/// `GroupMeta` rows. `first`/`len` index `OPS`, which is `ir.operations` verbatim, and a group
/// is therefore contiguous because the table is sorted by `(group, command)`.
fn groups(ir: &Ir) -> Vec<GroupEntry> {
    let mut runs: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (i, op) in ir.operations.iter().enumerate() {
        let e = runs.entry(op.group.as_str()).or_insert((i, 0));
        e.1 += 1;
    }
    // BTreeMap iteration gives the by-name order `lookup::group`'s binary search needs.
    runs.into_iter()
        .map(|(name, (first, len))| GroupEntry {
            about: ir
                .groups
                .iter()
                .find(|g| g.name == name)
                .map(|g| g.doc.clone())
                .unwrap_or_default(),
            name: name.to_owned(),
            first,
            len,
        })
        .collect()
}

// ------------------------------------------------------------------------------- enum values

/// Known values for each generated open enum, keyed by type name.
type EnumTable = BTreeMap<String, Vec<String>>;

fn enum_table(ir: &Ir) -> EnumTable {
    ir.open_enums
        .iter()
        .map(|e| (e.name.as_str().to_owned(), e.variants.iter().map(|v| v.wire.clone()).collect()))
        .collect()
}

/// The values to suggest for a parameter or body field.
///
/// A parameter that declares `enum` inline carries its own values. A `$ref` to one of Gitea's
/// bare `type: string` definitions (`ReviewStateType`, `CommitStatusState`) does not — the spec
/// has no variants for those at all — so they come from the generated open enum, which is the
/// same curated list the models use. Without this, three body flags would suggest nothing.
fn values(enums: &EnumTable, declared: Option<&Vec<String>>, ty: &RustType) -> Vec<String> {
    if let Some(v) = declared {
        return v.clone();
    }
    match ty {
        RustType::OpenEnum(name) => enums.get(name).cloned().unwrap_or_default(),
        RustType::Vec(inner) | RustType::Map(inner) => values(enums, None, inner),
        _ => Vec::new(),
    }
}

// ------------------------------------------------------------------------------- type mapping

/// `RustType` → `ValueTy`, the shape layer 2 parses a flag into.
///
/// Three mappings deserve their comment:
///
/// * An open enum is a `Str`. `ParamMeta::enum_values` carries the known values separately, and
///   layer 2 only *suggests* them — the server may accept a value our pinned spec does not list,
///   and rejecting it locally would make the CLI less capable than `curl`.
/// * `Vec<T>` collapses to `List`, losing the element type. `ValueTy` has no parameterised list
///   variant; the eleven affected query parameters are all arrays of strings or ids, where
///   clap's own parse of the repeated value is enough.
/// * `BTreeMap<String, T>` becomes `Json`. `ValueTy` has no map variant, and the eleven affected
///   body fields (`config` on every webhook, `units_map`, `DispatchWorkflow.inputs`) are
///   free-form key/value bags. `Json` says "hand me an object", which `--body-file` and a
///   JSON-valued flag both satisfy; pretending they were `Str` would silently send a string
///   where the API wants an object.
fn value_ty(ty: &RustType) -> Result<TokenStream> {
    Ok(match ty {
        RustType::Bool => quote! { ValueTy::Bool },
        RustType::I32 | RustType::I64 | RustType::U64 | RustType::Newtype(_) => {
            quote! { ValueTy::Int }
        }
        RustType::F64 => quote! { ValueTy::Float },
        RustType::String | RustType::OpenEnum(_) => quote! { ValueTy::Str },
        RustType::Timestamp => quote! { ValueTy::DateTime },
        RustType::Vec(_) => quote! { ValueTy::List },
        RustType::Map(_) | RustType::Json => quote! { ValueTy::Json },
        RustType::File => quote! { ValueTy::File },
        // These are response-only shapes. Reaching here means lowering started producing them
        // for an input, and guessing would put a wrong flag type in front of users.
        RustType::Model(_) | RustType::Unit | RustType::Bytes | RustType::Text => bail!(
            "{ty:?} has no ValueTy: it is a response shape, not something a flag can carry. \
             Either lowering changed or meta_types::ValueTy needs a new variant."
        ),
    })
}

/// `Mime` + `Success` → `Produces`, which decides how the runtime handles the response body.
///
/// The success *type* leads and `produces` only disambiguates, because lowering already applied
/// the subtle precedence: the document-level default `produces` is
/// `["application/json", "text/html"]`, so 35 ordinary JSON operations that declare no
/// `produces` of their own inherit `text/html`. Reading `produces[0]` here would classify
/// `WikiPage` and `WatchInfo` as HTML and hand the user an opaque string.
fn produces(op: &Operation) -> Result<TokenStream> {
    Ok(match &op.success.ty {
        RustType::Unit => quote! { Produces::Empty },
        RustType::Bytes => quote! { Produces::Bytes },
        RustType::Text if op.produces.contains(&Mime::TextPlain) => quote! { Produces::Text },
        RustType::Text if op.produces.contains(&Mime::TextHtml) => quote! { Produces::Html },
        RustType::Json if op.produces.contains(&Mime::LdJson) => quote! { Produces::LdJson },
        _ if op.produces.contains(&Mime::Json) => quote! { Produces::Json },
        _ if op.produces.contains(&Mime::LdJson) => quote! { Produces::LdJson },
        _ => bail!(
            "cannot map produces {:?} with success type {:?} onto meta_types::Produces. A new \
             media type upstream needs a `Produces` variant and a runtime path, not a silent \
             fallback to JSON.",
            op.produces,
            op.success.ty,
        ),
    })
}

// ------------------------------------------------------------------------------- ops_*.rs

fn ops_file(ir: &Ir, key: &str, ops: &[usize], enums: &EnumTable) -> Result<TokenStream> {
    let doc = format!(
        " `OpMeta` initializers for `{key}`. Referenced by `OPS` in this module's parent.",
    );
    let items =
        ops.iter().map(|&i| op_const(&ir.operations[i], enums)).collect::<Result<Vec<_>>>()?;

    Ok(quote! {
        #![doc = #doc]

        use crate::meta_types::*;

        #(#items)*
    })
}

fn op_const(op: &Operation, enums: &EnumTable) -> Result<TokenStream> {
    let name = op_ident(op);
    let op_id = op.op_id.as_str();
    let group = op.group.as_str();
    let command = op.command.as_str();
    let method = op.method.as_str();
    let raw_path = op.path.raw.as_str();
    let summary = op.doc.short.as_str();
    // Joined with a space rather than a newline: `Doc::long` is one whitespace-collapsed
    // paragraph that lowering wrapped at 96 columns for rustdoc's benefit, and clap wraps
    // `long_about` to the real terminal width itself. Keeping the hard breaks would produce
    // ragged help on every width but 96.
    let description = op.doc.long.join(" ");

    let params = param_list(op, enums)?;
    let body = match &op.body {
        Some(b) => {
            let type_name = body_type_name(&b.ty);
            let required = b.required;
            let content_type = op.consumes.as_ref().map_or("application/json", Mime::as_str);
            let fields = b
                .flat
                .iter()
                .map(|f| {
                    let pointer = json_pointer(&f.wire);
                    let flag = f.flag.as_str();
                    let ty = value_ty(&f.ty)?;
                    let required = f.required;
                    let vals = values(enums, f.enum_values.as_ref(), &f.ty);
                    let help = f.doc.short.as_str();
                    Ok(quote! {
                        BodyField {
                            pointer: #pointer,
                            flag: #flag,
                            ty: #ty,
                            required: #required,
                            enum_values: &[#(#vals),*],
                            help: #help,
                        }
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let deep = b.deep.iter().map(String::as_str);
            quote! {
                Some(&BodyMeta {
                    type_name: #type_name,
                    required: #required,
                    content_type: #content_type,
                    fields: &[#(#fields),*],
                    deep: &[#(#deep),*],
                })
            }
        }
        None => quote! { None },
    };

    let pagination = match op.pagination {
        Pagination::None => quote! { Pagination::None },
        Pagination::Paged { .. } => quote! { Pagination::Paged },
    };
    let produces = produces(op)?;
    let scope = opt_str(op.scope.as_deref());
    let deprecated = opt_str(op.deprecated.as_deref());

    Ok(quote! {
        pub(super) const #name: OpMeta = OpMeta {
            op_id: #op_id,
            group: #group,
            command: #command,
            method: #method,
            path: #raw_path,
            summary: #summary,
            description: #description,
            params: &[#(#params),*],
            body: #body,
            pagination: #pagination,
            produces: #produces,
            scope: #scope,
            deprecated: #deprecated,
        };
    })
}

/// Path parameters first, in path order, because layer 2 also accepts them positionally and
/// the position is the path's, not the spec's parameter array's.
fn param_list(op: &Operation, enums: &EnumTable) -> Result<Vec<TokenStream>> {
    let all = op
        .path_params
        .iter()
        .map(|p| (quote! { In::Path }, p))
        .chain(op.query_params.iter().map(|p| (quote! { In::Query }, p)))
        .chain(op.form_data.iter().map(|p| (quote! { In::FormData }, p)));

    all.map(|(location, p)| param(p, location, enums)).collect()
}

fn param(p: &Param, location: TokenStream, enums: &EnumTable) -> Result<TokenStream> {
    let wire = p.wire.as_str();
    let flag = p.flag.as_str();
    let ty = value_ty(&p.ty)?;
    let required = p.required;
    // An array-valued parameter is repeatable whether or not the spec bothered to say
    // `collectionFormat: multi`: clap has to accept `--label a --label b` either way, and
    // `Request::query` is an ordered `Vec` precisely so repeated keys survive.
    let repeatable = p.repeated || matches!(p.ty, RustType::Vec(_));
    let encoding = match p.encoding {
        PathEncoding::Segment => quote! { PathEncoding::Segment },
        PathEncoding::PathLike => quote! { PathEncoding::PathLike },
    };
    let ctx_fill = match p.ctx_fill {
        Some(CtxFill::Owner) => quote! { Some(CtxFill::Owner) },
        Some(CtxFill::Repo) => quote! { Some(CtxFill::Repo) },
        // `meta_types::CtxFill` has no `Branch`. Five `repo` operations take a `branch` path
        // parameter that the IR marks context-fillable; they stay required here rather than
        // being silently filled from the checkout. Reported upstream of this file — changing
        // `meta_types` is not this emitter's call while `gea-raw` is being written against it.
        Some(CtxFill::Branch) | None => quote! { None },
    };
    let vals = values(enums, p.enum_values.as_ref(), &p.ty);
    let help = p.doc.short.as_str();

    Ok(quote! {
        ParamMeta {
            wire: #wire,
            flag: #flag,
            location: #location,
            ty: #ty,
            required: #required,
            repeatable: #repeatable,
            encoding: #encoding,
            ctx_fill: #ctx_fill,
            enum_values: &[#(#vals),*],
            help: #help,
        }
    })
}

/// The name `--help` and `--dry-run` show for a request body.
///
/// 123 of 125 bodies are `$ref`s to named definitions and get their real name. The other two
/// lose it in lowering, which resolves a free-form `$ref` to `RustType::Json` and an inline
/// schema to its scalar type, discarding the definition name in both cases. `object` and
/// `string` are honest about what the endpoint wants even if they are less specific than
/// `ForgeLike`.
fn body_type_name(ty: &RustType) -> &str {
    match ty {
        RustType::Model(name) => name,
        RustType::Json => "object",
        RustType::String => "string",
        RustType::Vec(_) => "array",
        _ => "value",
    }
}

/// RFC 6901 escaping. No Gitea field name contains `~` or `/` today, but a pointer that
/// silently means a different location than the field it came from is the kind of bug that
/// surfaces as "the flag did nothing".
fn json_pointer(wire: &str) -> String {
    format!("/{}", wire.replace('~', "~0").replace('/', "~1"))
}

fn opt_str(s: Option<&str>) -> TokenStream {
    match s {
        Some(s) => quote! { Some(#s) },
        None => quote! { None },
    }
}

fn op_ident(op: &Operation) -> Ident {
    // `fn_name` is unique per module, and a module is never split across groups, so this is
    // unique within its file. See the module docs on why uppercasing here is not the string
    // casing the IR contract forbids.
    Ident::new(&format!("OP_{}", op.fn_name.as_str().to_ascii_uppercase()), Span::call_site())
}

// ---------------------------------------------------------------------------------- mod.rs

fn mod_file(ir: &Ir, a: &Assignment) -> Result<TokenStream> {
    let mods = a.files.keys().map(|k| {
        let m = Ident::new(k, Span::call_site());
        quote! { mod #m; }
    });

    let entries = ir.operations.iter().enumerate().map(|(i, op)| {
        let m = Ident::new(&a.owner[i], Span::call_site());
        let c = op_ident(op);
        quote! { #m::#c }
    });

    let groups = a.groups.iter().map(|g| {
        let name = g.name.as_str();
        let about = g.about.as_str();
        // Unsuffixed, because `first: 420usize` is noise in a table a human is meant to read.
        let first = proc_macro2::Literal::usize_unsuffixed(g.first);
        let len = proc_macro2::Literal::usize_unsuffixed(g.len);
        quote! {
            GroupMeta { name: #name, about: #about, first: #first, len: #len }
        }
    });

    let count = ir.operations.len();
    let ops_doc = format!(
        " Every operation in the Gitea API — all {count} of them — sorted by \
         `(group, command)`."
    );

    Ok(quote! {
        #![doc = " Generated layer-2 command metadata: the whole API as `&'static` data."]
        #![doc = ""]
        #![doc = " `OPS` is the table `gea raw` dispatches from. Two properties of it are"]
        #![doc = " load-bearing and asserted by this module's `invariants` tests:"]
        #![doc = ""]
        #![doc = " 1. It is sorted by `(group, command)`, which is what makes `lookup::op` a"]
        #![doc = "    binary search."]
        #![doc = " 2. Each `GroupMeta` in `GROUPS` names a contiguous run of it, which is what"]
        #![doc = "    makes building one group's clap tree a slice rather than a scan of 482."]
        #![doc = ""]
        #![doc = " `GROUPS` is sorted by `name` for the same reason."]

        #(#mods)*

        #[cfg(test)]
        mod invariants;

        pub use crate::meta_types::{
            BodyField, BodyMeta, CtxFill, GroupMeta, In, OpMeta, Pagination, ParamMeta,
            PathEncoding, Produces, ValueTy, lookup,
        };

        #[doc = #ops_doc]
        pub static OPS: &[OpMeta] = &[#(#entries),*];

        #[doc = " The layer-2 command groups, sorted by name, each indexing a run of `OPS`."]
        pub static GROUPS: &[GroupMeta] = &[#(#groups),*];
    })
}

// ----------------------------------------------------------------------------- invariants.rs

/// The generated test module.
///
/// It is generated rather than hand-written for one reason: it must live inside the crate that
/// owns `OPS`, and everything inside `crates/gitea-client/src/generated/` is deleted and
/// rewritten by `xtask codegen`. A hand-written file there would vanish on the next run.
///
/// The coverage test loads the vendored spec and compares operation sets, rather than asserting
/// a hardcoded 482. A hardcoded count emitted *from the IR* would be self-fulfilling: the
/// generator would be checking its own arithmetic. Reading the spec makes the test an
/// independent witness, and it is the mechanical proof of this project's headline claim.
fn invariants_file() -> TokenStream {
    quote! {
        #![doc = " Invariants of the generated tables, asserted against the committed data."]

        use std::collections::{BTreeMap, BTreeSet};

        use super::{GROUPS, OPS};
        use crate::meta_types::{In, lookup};

        /// Every `operationId` in the vendored spec.
        ///
        /// The spec is located by scanning `spec/` for `gitea-*.json` rather than by a
        /// filename baked in here, so a version bump does not silently turn this test into a
        /// no-op — a missing file panics instead.
        fn spec_operation_ids() -> BTreeSet<String> {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec");
            let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot read the vendored spec directory {dir}: {e}"))
                .map(|e| e.expect("directory entry").path())
                .filter(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    name.starts_with("gitea-") && name.ends_with(".json")
                })
                .collect();
            found.sort();
            assert_eq!(
                found.len(),
                1,
                "expected exactly one vendored spec under {dir}, found {found:?}"
            );

            let text = std::fs::read_to_string(&found[0]).expect("the vendored spec is committed");
            let spec: serde_json::Value =
                serde_json::from_str(&text).expect("the vendored spec is valid JSON");
            let paths = spec["paths"].as_object().expect("the spec has a `paths` object");

            let mut ids = BTreeSet::new();
            let mut count = 0usize;
            for item in paths.values() {
                let Some(item) = item.as_object() else { continue };
                // Keys are HTTP methods plus a possible path-level `parameters` array, whose
                // value is not an object and therefore carries no `operationId`.
                for op in item.values() {
                    if let Some(id) = op.get("operationId").and_then(|v| v.as_str()) {
                        count += 1;
                        ids.insert(id.to_owned());
                    }
                }
            }
            assert_eq!(count, ids.len(), "the spec has duplicate operationIds");
            assert!(
                ids.len() > 400,
                "only {} operationIds found; the spec scan is broken, not the table",
                ids.len()
            );
            ids
        }

        /// The mechanical proof of "every endpoint is reachable". Coverage is a test failure
        /// here rather than a judgement call, and it fails the moment `update-spec` pulls in
        /// an endpoint the generator did not emit.
        #[test]
        fn ops_is_a_bijection_with_the_specs_operation_ids() {
            let spec = spec_operation_ids();
            let table: BTreeSet<String> = OPS.iter().map(|o| o.op_id.to_owned()).collect();

            assert_eq!(
                table.len(),
                OPS.len(),
                "OPS contains a duplicate op_id, so some operation is unreachable"
            );

            let missing: Vec<&String> = spec.difference(&table).collect();
            let extra: Vec<&String> = table.difference(&spec).collect();
            assert!(missing.is_empty(), "in the spec but not in OPS: {missing:?}");
            assert!(extra.is_empty(), "in OPS but not in the spec: {extra:?}");
            assert_eq!(OPS.len(), spec.len());
        }

        /// `lookup::op` is a binary search. An unsorted table does not error — it silently
        /// fails to find roughly half the commands, which reads as "that operation does not
        /// exist" to the user.
        #[test]
        fn ops_are_sorted_by_group_then_command() {
            for w in OPS.windows(2) {
                assert!(
                    (w[0].group, w[0].command) < (w[1].group, w[1].command),
                    "OPS is not sorted: {:?} then {:?}",
                    (w[0].group, w[0].command),
                    (w[1].group, w[1].command)
                );
            }
        }

        #[test]
        fn every_op_is_findable_by_its_own_group_and_command() {
            for op in OPS {
                let found = lookup::op(OPS, op.group, op.command);
                assert_eq!(
                    found.map(|o| o.op_id),
                    Some(op.op_id),
                    "lookup::op could not find `gea raw {} {}`",
                    op.group,
                    op.command
                );
            }
        }

        /// `lookup::group` binary-searches `GROUPS`.
        #[test]
        fn groups_are_sorted_by_name() {
            for w in GROUPS.windows(2) {
                assert!(w[0].name < w[1].name, "GROUPS is not sorted by name");
            }
            for g in GROUPS {
                assert_eq!(lookup::group(GROUPS, g.name).map(|x| x.name), Some(g.name));
            }
        }

        /// A wrong `first`/`len` puts another group's operations under `gea raw <group>`, with
        /// no error anywhere — the clap tree is simply built from the wrong slice.
        #[test]
        fn group_indices_slice_ops_exactly_and_cover_all_of_it() {
            let mut covered = 0;
            for g in GROUPS {
                assert!(
                    g.first + g.len <= OPS.len(),
                    "{} runs off the end of OPS",
                    g.name
                );
                let slice = lookup::ops_in(OPS, g);
                assert_eq!(slice.len(), g.len);
                assert!(!slice.is_empty(), "{} is an empty group", g.name);
                for op in slice {
                    assert_eq!(
                        op.group, g.name,
                        "{} claims {}, which belongs to {}",
                        g.name, op.op_id, op.group
                    );
                }
                covered += g.len;
            }
            assert_eq!(
                covered,
                OPS.len(),
                "GROUPS does not partition OPS; some operations are unreachable"
            );

            // ...and the reverse direction: no operation names a group that does not exist.
            let names: BTreeSet<&str> = GROUPS.iter().map(|g| g.name).collect();
            for op in OPS {
                assert!(
                    names.contains(op.group),
                    "{} is in group {:?}, which has no GroupMeta",
                    op.op_id,
                    op.group
                );
            }
        }

        /// A `{param}` with no `ParamMeta` produces a request URL with a literal `{...}` still
        /// in it, and the server answers 404 — one of the most baffling failures possible,
        /// because the command line looked fine. The reverse (a path parameter that appears
        /// nowhere in the template) means a value the user typed is silently dropped.
        #[test]
        fn path_placeholders_and_path_params_agree() {
            for op in OPS {
                let mut placeholders = BTreeSet::new();
                let mut rest = op.path;
                while let Some(open) = rest.find('{') {
                    rest = &rest[open + 1..];
                    let close = rest
                        .find('}')
                        .unwrap_or_else(|| panic!("{}: unclosed '{{' in {:?}", op.op_id, op.path));
                    placeholders.insert(&rest[..close]);
                    rest = &rest[close + 1..];
                }
                let declared: BTreeSet<&str> = op
                    .params
                    .iter()
                    .filter(|p| p.location == In::Path)
                    .map(|p| p.wire)
                    .collect();
                assert_eq!(
                    placeholders, declared,
                    "{}: path {:?} and its In::Path parameters disagree",
                    op.op_id, op.path
                );
            }
        }

        /// Every path parameter is required, and no two parameters of one operation share a
        /// flag: clap panics at runtime on a duplicate long flag, and it would panic only for
        /// whichever operation the user happened to invoke.
        #[test]
        fn flags_are_unique_within_an_operation() {
            for op in OPS {
                let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
                for p in op.params {
                    if let Some(prev) = seen.insert(p.flag, p.wire) {
                        panic!(
                            "{}: --{} is claimed by both {:?} and {:?}",
                            op.op_id, p.flag, prev, p.wire
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::types::Mime;
    use std::sync::OnceLock;

    fn ir() -> &'static Ir {
        static IR: OnceLock<Ir> = OnceLock::new();
        IR.get_or_init(|| {
            let root = crate::workspace_root();
            let loaded = crate::spec::load(&root).expect("spec/ is committed");
            let ov = crate::overrides::Overrides::load().expect("overrides.toml is committed");
            crate::ir::lower::lower(&loaded, &ov).expect("lowering the vendored spec succeeds")
        })
    }

    #[test]
    fn every_operation_lands_in_exactly_one_file() {
        let ir = ir();
        let a = assign_files(ir, &enum_table(ir)).unwrap();
        let total: usize = a.files.values().map(Vec::len).sum();
        assert_eq!(total, ir.operations.len());
        assert!(a.owner.iter().all(|o| !o.is_empty()));
    }

    #[test]
    fn no_emitted_file_busts_the_review_cap() {
        // `emit_all` also checks this, but failing here names the meta emitter rather than
        // leaving the reader to work out which of four emitters produced the offender.
        for f in emit(ir()).unwrap() {
            assert!(
                f.line_count() <= MAX_LINES,
                "{} is {} lines",
                f.path.display(),
                f.line_count()
            );
        }
    }

    #[test]
    fn emission_is_deterministic() {
        // The whole reason `clippy.toml` bans HashMap in this crate. A generator whose output
        // moves between runs makes `codegen --check` flap and destroys the property that a
        // generated diff is the API changelog.
        assert_eq!(emit(ir()).unwrap(), emit(ir()).unwrap());
    }

    #[test]
    fn groups_are_contiguous_runs_in_name_order() {
        let ir = ir();
        let gs = groups(ir);
        for w in gs.windows(2) {
            assert!(w[0].name < w[1].name, "groups must be sorted by name");
        }
        assert_eq!(gs.iter().map(|g| g.len).sum::<usize>(), ir.operations.len());
        for g in &gs {
            for op in &ir.operations[g.first..g.first + g.len] {
                assert_eq!(op.group, g.name);
            }
        }
    }

    #[test]
    fn repo_operations_keep_the_irs_own_buckets() {
        // A reader looking for `create-pull-request` should find it under `ops_repo_pulls*`,
        // not in whichever slice a line-count packing happened to produce. `repo_pulls` is one
        // of the buckets large enough to need the second, letter-keyed split as well.
        let ir = ir();
        let a = assign_files(ir, &enum_table(ir)).unwrap();
        let owner = |op_id: &str| {
            let i = ir
                .operations
                .iter()
                .position(|o| o.op_id == op_id)
                .unwrap_or_else(|| panic!("{op_id} is missing"));
            a.owner[i].clone()
        };
        assert_eq!(owner("repoCreatePullRequest"), "ops_repo_pulls_c");
        assert_eq!(owner("repoGetContents"), "ops_repo_contents");
        assert_eq!(owner("GetTree"), "ops_git");
        // A small group stays in one unsplit file, so the letter split is not applied blindly.
        assert_eq!(owner("ActionsDispatchWorkflow"), "ops_workflow");
    }

    #[test]
    fn open_enum_values_reach_body_flags_that_the_spec_left_bare() {
        // `repoCreatePullReview.event` is a `$ref` to `ReviewStateType`, a bare Go
        // `type string` with no `enum` in the spec at all. Without the open-enum fallback the
        // flag would suggest nothing, and `--event` is not guessable.
        let ir = ir();
        let enums = enum_table(ir);
        let op = ir.operation("repoCreatePullReview").unwrap();
        let field = op.body.as_ref().unwrap().flat.iter().find(|f| f.wire == "event").unwrap();
        let v = values(&enums, field.enum_values.as_ref(), &field.ty);
        assert!(v.contains(&"APPROVED".to_owned()), "{v:?}");
    }

    #[test]
    fn html_defaulting_operations_are_still_json() {
        // 40 operations declare no `produces` of their own and inherit the document-level one.
        // Gitea's is `["application/json"]` (Forgejo's added `text/html`), so an inherited
        // default must classify as JSON: calling `WikiPage` or `WatchInfo` HTML would hand the
        // user an opaque string instead of an object.
        let ir = ir();
        for op_id in ["repoCreateWikiPage", "userCurrentCheckSubscription"] {
            let op = ir.operation(op_id).unwrap();
            assert!(op.produces.contains(&Mime::Json), "{op_id}");
            assert_eq!(
                produces(op).unwrap().to_string(),
                quote! { Produces::Json }.to_string(),
                "{op_id}"
            );
        }
        // ...while a genuinely HTML endpoint is not swept along with them.
        let markdown = ir.operation("renderMarkdown").unwrap();
        assert_eq!(produces(markdown).unwrap().to_string(), quote! { Produces::Html }.to_string());
        // ...and a plain-text one is Text, not Html.
        let key = ir.operation("repoSigningKey").unwrap();
        assert_eq!(produces(key).unwrap().to_string(), quote! { Produces::Text }.to_string());
    }

    #[test]
    fn produces_covers_every_operation_in_the_spec() {
        for op in &ir().operations {
            produces(op).unwrap_or_else(|e| panic!("{}: {e}", op.op_id));
        }
    }

    #[test]
    fn value_ty_covers_every_parameter_and_body_field_in_the_spec() {
        for op in &ir().operations {
            for p in op.path_params.iter().chain(&op.query_params).chain(&op.form_data) {
                value_ty(&p.ty).unwrap_or_else(|e| panic!("{} {}: {e}", op.op_id, p.wire));
            }
            for f in op.body.iter().flat_map(|b| &b.flat) {
                value_ty(&f.ty).unwrap_or_else(|e| panic!("{} {}: {e}", op.op_id, f.wire));
            }
        }
    }

    #[test]
    fn a_response_shape_has_no_value_ty() {
        // Silently mapping `Bytes` to `Str` would put a flag in front of users that cannot
        // work; the emitter must stop instead.
        assert!(value_ty(&RustType::Bytes).is_err());
        assert!(value_ty(&RustType::Unit).is_err());
    }

    #[test]
    fn json_pointers_are_rfc6901_escaped() {
        assert_eq!(json_pointer("title"), "/title");
        assert_eq!(json_pointer("a/b"), "/a~1b");
        assert_eq!(json_pointer("a~b"), "/a~0b");
    }
}

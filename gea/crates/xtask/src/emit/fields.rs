//! The `fields` emitter: the `--json` field tables.
//!
//! Output is [`gitea_client::fields`](../../../gitea-client/src/generated/fields/mod.rs):
//! one `FIELDS_<MODEL>: &[FieldSpec]` per model that can come back from an operation, plus
//! `OP_FIELDS`, a sorted `op_id → fields` index that `gea` binary-searches to answer bare
//! `--json` without auth and without a network round trip.
//!
//! # Wire names, verbatim
//!
//! `FieldSpec::name` is the JSON key exactly as Gitea writes it. There is no case translation
//! anywhere in this project — `docs/output.md` argues it at length, and the short version is
//! that `gh`'s rule is "field names are the API's field names", which for a snake_case REST API
//! yields snake_case. Copying `gh`'s camelCase *output* would break `--jq` portability between
//! `gea api` and `gea pr list` and would need a bijective mapping that `html_url` does not
//! have.
//!
//! # `Vec<T>` resolves to `T`'s fields
//!
//! `--json number,title` on a list endpoint selects those keys from each element, matching `gh`.
//! So an operation returning `Vec<Issue>` gets `FIELDS_ISSUE`, identical to one returning
//! `Issue`.
//!
//! # Breaking the `$ref` cycles
//!
//! `FieldKind::Object(&'static [FieldSpec])` cannot express a cycle. `Repository.parent` is a
//! `Repository` and `GPGKey.subkeys` is a `Vec<GPGKey>`, so the naive translation writes
//! `static FIELDS_REPOSITORY: &[FieldSpec] = &[… Object(FIELDS_REPOSITORY) …]` — a static whose
//! own initializer reads it. That is not a stack overflow at runtime, it is a `cycle detected
//! when simplifying constant` error at compile time, because the `&'static` value it describes
//! cannot exist.
//!
//! **The choice: an edge that participates in a cycle becomes `FieldKind::Json`.** So
//! `Repository.parent` is `json` and `GPGKey.subkeys` is `[json]`. Reasons, in order:
//!
//! * It is honest. `Json` in this vocabulary means "opaque nested value, reach into it with
//!   `--jq`", which is exactly what a self-referential parent repository is. The alternative
//!   candidates all lie: an empty `Object(&[])` claims the field has no fields, and dropping
//!   the field claims it does not exist.
//! * It loses nothing a user can act on. Only top-level fields are selectable by `--json`; the
//!   nested `FieldKind` exists solely to label a field in the discovery listing as something to
//!   reach into rather than a scalar to put in a column, and `json` says that.
//! * It is local. Only the closing edge is rewritten, so `Repository`'s other 60 fields and
//!   every other model keep their full nesting.
//!
//! A cycle edge is detected as "the target can reach the owner", which breaks both edges of a
//! future mutual cycle rather than picking one arbitrarily. Today there are exactly two, both
//! self-loops, and the generated `fields/invariants.rs` asserts each one by name.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

use super::{GeneratedFile, MAX_LINES, render};
use crate::Result;
use crate::ir::types::RustType;
use crate::ir::{Ir, Model, ModelKind, Success};

/// See the note on the meta emitter's budget: splitting is automatic, so leaving headroom keeps
/// a one-field spec change from reshuffling every file in the directory.
const BUDGET: usize = MAX_LINES * 4 / 5;

pub fn emit(ir: &Ir) -> Result<Vec<GeneratedFile>> {
    let cx = Context::build(ir)?;

    let mut files = Vec::new();
    for (bucket, models) in &cx.buckets {
        let tokens = bucket_file(&cx, bucket, models)?;
        let text = render(tokens, &ir.spec_version, &ir.spec_sha256)?;
        let lines = text.lines().count();
        if lines > MAX_LINES {
            bail!(
                "{bucket}.rs would be {lines} lines, over the {MAX_LINES}-line review cap. The \
                 field tables bucket by the first letter of the model's module name; a single \
                 letter this large needs a finer rule in crates/xtask/src/emit/fields.rs."
            );
        }
        files.push(GeneratedFile::new(path(&format!("{bucket}.rs")), text));
    }

    files.push(GeneratedFile::new(
        path("mod.rs"),
        render(mod_file(&cx)?, &ir.spec_version, &ir.spec_sha256)?,
    ));
    files.push(GeneratedFile::new(
        path("invariants.rs"),
        render(invariants_file(&cx), &ir.spec_version, &ir.spec_sha256)?,
    ));

    Ok(files)
}

fn path(leaf: &str) -> String {
    format!("crates/gitea-client/src/generated/fields/{leaf}")
}

// -------------------------------------------------------------------------------- the context

/// Everything the token writers need, resolved once so that emission is a pure walk.
struct Context<'a> {
    /// Struct models a `FIELDS_*` table is emitted for: reachable from some operation's success
    /// type, transitively through nested references.
    models: BTreeMap<&'a str, &'a Model>,
    /// Bucket file stem → the model names it holds.
    buckets: BTreeMap<String, Vec<&'a str>>,
    /// Model name → the bucket file its table lives in.
    bucket_of: BTreeMap<&'a str, String>,
    /// `op_id` → the model whose fields `--json` offers, sorted by `op_id`.
    op_models: BTreeMap<&'a str, &'a str>,
    /// Open enum name → known values.
    enums: BTreeMap<&'a str, Vec<&'a str>>,
    /// `(owner, target)` reference edges that close a cycle and are therefore emitted as
    /// `FieldKind::Json`.
    broken: BTreeSet<(&'a str, &'a str)>,
    /// `(owner, field wire name)` for each field a broken edge passes through. The generated
    /// test asserts these by name, so the cycle-breaking rule is visible in a test failure
    /// rather than only in this file's doc comment.
    broken_fields: BTreeSet<(&'a str, &'a str)>,
}

impl<'a> Context<'a> {
    fn build(ir: &'a Ir) -> Result<Context<'a>> {
        let structs: BTreeMap<&str, &Model> = ir
            .models
            .iter()
            .filter(|m| matches!(m.kind, ModelKind::Struct(_)))
            .map(|m| (m.wire.as_str(), m))
            .collect();

        // A synthesized `One(T) / Many(Vec<T>)` response offers the fields of `T`: whether the
        // path named a file or a directory, `--json name,path,type` means the same thing, and
        // the enum itself has no fields to offer. Resolving the alias here rather than in
        // `success_model` keeps that lookup a pure function of the `Success`.
        let one_or_many: BTreeMap<&str, &str> = ir
            .models
            .iter()
            .filter_map(|m| match &m.kind {
                ModelKind::OneOrMany(RustType::Model(inner)) => {
                    Some((m.wire.as_str(), inner.as_str()))
                }
                _ => None,
            })
            .collect();

        // Roots: the model behind each operation's success type.
        let mut op_models: BTreeMap<&str, &str> = BTreeMap::new();
        for op in &ir.operations {
            if let Some(name) = success_model(&op.success) {
                let name = one_or_many.get(name).copied().unwrap_or(name);
                let Some(m) = structs.get(name) else {
                    bail!(
                        "{} returns {name:?}, which is not a struct model, so there are no \
                         `--json` fields to offer for it.",
                        op.op_id
                    );
                };
                op_models.insert(op.op_id.as_str(), m.wire.as_str());
            }
        }

        // Transitive closure over nested references. `--json` only offers top-level fields, but
        // the nested `FieldKind` labels them, so every model that can appear inside one needs a
        // table of its own.
        let graph = ref_graph(&structs);
        let mut models: BTreeMap<&str, &Model> = BTreeMap::new();
        let mut queue: Vec<&str> = op_models.values().copied().collect();
        while let Some(name) = queue.pop() {
            let Some(m) = structs.get(name) else { continue };
            if models.insert(name, m).is_some() {
                continue;
            }
            queue.extend(graph.get(name).into_iter().flatten().copied());
        }

        let broken = broken_edges(&graph, &models);
        let mut broken_fields = BTreeSet::new();
        for (owner, m) in &models {
            let ModelKind::Struct(fields) = &m.kind else {
                continue;
            };
            for f in fields {
                let mut targets = BTreeSet::new();
                collect_models(&f.ty, &mut targets);
                if targets.iter().any(|t| broken.contains(&(*owner, *t))) {
                    broken_fields.insert((*owner, f.wire.as_str()));
                }
            }
        }

        let mut buckets: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        let mut bucket_of: BTreeMap<&str, String> = BTreeMap::new();
        let mut by_static: BTreeMap<String, &str> = BTreeMap::new();
        for (name, m) in &models {
            // Two models whose mangled module names collide would produce two `FIELDS_X`
            // statics with the same name; the models emitter has the same constraint, but a
            // duplicate-definition error 5,000 generated lines away is a poor way to find out.
            let stat = static_name(m);
            if let Some(prev) = by_static.insert(stat.clone(), name) {
                bail!(
                    "models {prev:?} and {name:?} both mangle to `{stat}`. One of them needs a \
                     distinct name; there is no way to emit both tables."
                );
            }
            let bucket = bucket_name(m)?;
            buckets.entry(bucket.clone()).or_default().push(name);
            bucket_of.insert(name, bucket);
        }

        let enums = ir
            .open_enums
            .iter()
            .map(|e| (e.name.as_str(), e.variants.iter().map(|v| v.wire.as_str()).collect()))
            .collect();

        Ok(Context { models, buckets, bucket_of, op_models, enums, broken, broken_fields })
    }

    /// The path a `FieldKind::Object` uses to name another model's table.
    ///
    /// Always fully qualified through the parent module even for a model in the same file, so
    /// the reference does not depend on which bucket either side landed in.
    fn table_path(&self, name: &str) -> Result<TokenStream> {
        let Some(bucket) = self.bucket_of.get(name) else {
            bail!("no field table was emitted for {name:?}");
        };
        let m = Ident::new(bucket, Span::call_site());
        let s = Ident::new(&static_name(self.models[name]), Span::call_site());
        Ok(quote! { super::#m::#s })
    }
}

/// The model whose fields `--json` should offer for a response.
///
/// `Vec<T>` unwraps to `T`: `--json` on a list selects fields of each element, which is what
/// `gh` does and what anyone piping to `--jq '.[].title'` expects. Anything else — a bare
/// scalar, a map, `()`, a byte stream — has no field list.
fn success_model(s: &Success) -> Option<&str> {
    fn inner(ty: &RustType) -> Option<&str> {
        match ty {
            RustType::Model(name) => Some(name),
            RustType::Vec(t) => inner(t),
            _ => None,
        }
    }
    inner(&s.ty)
}

/// Model → the models its fields can contain, following through `Vec` and `BTreeMap`.
///
/// Unlike the *size* graph in `ir::lower`, collections are edges here: `Vec<GPGKey>` inside
/// `GPGKey` is harmless for a Rust type's size but is still a cycle for a `&'static` field
/// table, because `Array(&Object(FIELDS_GPGKEY))` inside `FIELDS_GPGKEY` reads the static it is
/// defining.
fn ref_graph<'a>(structs: &BTreeMap<&'a str, &'a Model>) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    structs
        .iter()
        .map(|(name, m)| {
            let mut edges = BTreeSet::new();
            if let ModelKind::Struct(fields) = &m.kind {
                for f in fields {
                    collect_models(&f.ty, &mut edges);
                }
            }
            (*name, edges)
        })
        .collect()
}

/// Every model a type mentions, looking through `Vec` and `BTreeMap`.
fn collect_models<'a>(ty: &'a RustType, out: &mut BTreeSet<&'a str>) {
    match ty {
        RustType::Model(name) => {
            out.insert(name.as_str());
        }
        RustType::Vec(t) | RustType::Map(t) => collect_models(t, out),
        _ => {}
    }
}

/// Edges `(owner, target)` that participate in a cycle, i.e. where `target` can get back to
/// `owner`. A self-loop qualifies, since the edge is itself the whole cycle.
///
/// Breaking every such edge rather than a chosen spanning-tree back edge keeps the result
/// independent of traversal order — a generator whose output depends on which node a DFS
/// happened to start from produces diff churn for no reason.
fn broken_edges<'a>(
    graph: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    models: &BTreeMap<&'a str, &'a Model>,
) -> BTreeSet<(&'a str, &'a str)> {
    let mut out = BTreeSet::new();
    for owner in models.keys() {
        for target in graph.get(owner).into_iter().flatten() {
            if can_reach(graph, target, owner) {
                out.insert((*owner, *target));
            }
        }
    }
    out
}

/// Whether `to` is reachable from `from` in **zero** or more edges.
///
/// Zero, deliberately: this is only ever asked about the far end of an existing edge
/// `owner -> target`, so `can_reach(target, owner)` with `target == owner` is asking whether a
/// self-loop closes a cycle, and it does. Requiring at least one edge would let
/// `Repository.parent` through and produce a static whose initializer reads itself.
fn can_reach(graph: &BTreeMap<&str, BTreeSet<&str>>, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut queue = vec![from];
    while let Some(n) = queue.pop() {
        if n == to {
            return true;
        }
        if !seen.insert(n) {
            continue;
        }
        queue.extend(graph.get(n).into_iter().flatten().copied());
    }
    false
}

/// `PullRequest` → `FIELDS_PULL_REQUEST`.
///
/// Uppercased from the `module` ident that `ir::names` already produced, not re-derived from the
/// wire name. See the note in the meta emitter: this is a private-to-generated symbol, and the
/// name is the one the plan specifies.
fn static_name(m: &Model) -> String {
    format!("FIELDS_{}", m.module.as_str().to_ascii_uppercase())
}

/// Bucket file for a model: the first letter of its module name.
///
/// A per-letter split beats a line-count packing for the same reason it does in the meta
/// emitter — a new model lands in one file and moves nothing else — and beats one file per
/// model because a `FieldSpec` table is a dozen lines, not a hundred.
fn bucket_name(m: &Model) -> Result<String> {
    let Some(c) = m.module.as_str().chars().next().filter(char::is_ascii_alphabetic) else {
        bail!(
            "model {:?} has module name {:?}, which does not start with an ASCII letter",
            m.wire,
            m.module.as_str()
        );
    };
    Ok(format!("f_{}", c.to_ascii_lowercase()))
}

// ---------------------------------------------------------------------------- the field tables

fn bucket_file(cx: &Context<'_>, bucket: &str, models: &[&str]) -> Result<TokenStream> {
    let letter = bucket.trim_start_matches("f_").to_ascii_uppercase();
    let doc = format!(" `--json` field tables for models whose name starts with `{letter}`.");
    let tables = models.iter().map(|name| table(cx, name)).collect::<Result<Vec<_>>>()?;

    Ok(quote! {
        #![doc = #doc]

        use crate::meta_types::{FieldKind, FieldSpec};

        #(#tables)*
    })
}

fn table(cx: &Context<'_>, name: &str) -> Result<TokenStream> {
    let m = cx.models[name];
    let ModelKind::Struct(fields) = &m.kind else {
        bail!("{name:?} is not a struct model");
    };

    // Sorted by wire name so the bare-`--json` listing is alphabetical. The IR's fields already
    // arrive in this order (they come from a BTreeMap), but the listing's order is user-visible,
    // so it is arranged here rather than assumed.
    let mut sorted: Vec<_> = fields.iter().collect();
    sorted.sort_by(|a, b| a.wire.cmp(&b.wire));

    let specs = sorted
        .iter()
        .map(|f| {
            let wire = f.wire.as_str();
            let kind = field_kind(cx, name, &f.ty)?;
            let doc = f.doc.short.as_str();
            Ok(quote! {
                FieldSpec { name: #wire, kind: #kind, doc: #doc }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let stat = Ident::new(&static_name(m), Span::call_site());
    let doc = format!(" `--json` fields of `{name}`, sorted by name.");
    Ok(quote! {
        #[doc = #doc]
        pub static #stat: &[FieldSpec] = &[#(#specs),*];
    })
}

fn field_kind(cx: &Context<'_>, owner: &str, ty: &RustType) -> Result<TokenStream> {
    Ok(match ty {
        RustType::Bool => quote! { FieldKind::Bool },
        RustType::I32 | RustType::I64 | RustType::U64 | RustType::Newtype(_) => {
            quote! { FieldKind::Int }
        }
        RustType::F64 => quote! { FieldKind::Float },
        RustType::String => quote! { FieldKind::Str },
        RustType::Timestamp => quote! { FieldKind::DateTime },
        RustType::OpenEnum(name) => {
            let vals = cx.enums.get(name.as_str()).cloned().unwrap_or_default();
            quote! { FieldKind::Enum(&[#(#vals),*]) }
        }
        RustType::Model(name) if cx.broken.contains(&(owner, name.as_str())) => {
            quote! { FieldKind::Json }
        }
        RustType::Model(name) => {
            let path = cx.table_path(name)?;
            quote! { FieldKind::Object(#path) }
        }
        RustType::Vec(inner) => {
            let k = field_kind(cx, owner, inner)?;
            quote! { FieldKind::Array(&#k) }
        }
        RustType::Map(inner) => {
            let k = field_kind(cx, owner, inner)?;
            quote! { FieldKind::Map(&#k) }
        }
        RustType::Json => quote! { FieldKind::Json },
        RustType::Unit | RustType::Bytes | RustType::Text | RustType::File => bail!(
            "{ty:?} is a transport shape, not a model field type, so it has no FieldKind. \
             Either lowering changed or meta_types::FieldKind needs a new variant."
        ),
    })
}

// ---------------------------------------------------------------------------------- mod.rs

fn mod_file(cx: &Context<'_>) -> Result<TokenStream> {
    let mods = cx.buckets.keys().map(|b| {
        let m = Ident::new(b, Span::call_site());
        quote! { mod #m; }
    });
    let reexports = cx.buckets.keys().map(|b| {
        let m = Ident::new(b, Span::call_site());
        quote! { pub use #m::*; }
    });

    // `BTreeMap<&str, _>` iteration is byte-wise `str` order, which is the order
    // `slice::binary_search_by` on `&str` uses. The 16 PascalCase operationIds therefore sort
    // before the camelCase ones; that is not a mistake, it is the same comparison the lookup
    // performs.
    let entries = cx
        .op_models
        .iter()
        .map(|(op_id, model)| {
            let bucket = Ident::new(&cx.bucket_of[*model], Span::call_site());
            let stat = Ident::new(&static_name(cx.models[*model]), Span::call_site());
            quote! { (#op_id, #bucket::#stat) }
        })
        .collect::<Vec<_>>();

    let n_models = cx.models.len();
    let n_ops = cx.op_models.len();
    let models_doc = format!(" {n_models} field tables cover every model an operation can return.");
    let ops_doc = format!(
        " The {n_ops} operations that return a typed object, sorted by `op_id` for binary search."
    );

    Ok(quote! {
        #![doc = " Generated `--json` field tables."]
        #![doc = ""]
        #![doc = " Field names are the API's own snake_case keys, verbatim: there is no case"]
        #![doc = " translation anywhere in this project. See `docs/output.md`."]
        #![doc = ""]
        #![doc = " Only top-level fields are selectable with `--json`; the nested `FieldKind` is"]
        #![doc = " there to tell the reader that a field is an object to reach into with `--jq`"]
        #![doc = " rather than a scalar to put in a column. That is the same division `gh` makes."]
        #![doc = ""]
        #![doc = " An operation whose response has no typed object — a 204, a byte stream, plain"]
        #![doc = " text, a bare array of strings — has no entry in `OP_FIELDS`, because there is"]
        #![doc = " nothing to select."]

        #(#mods)*

        #[cfg(test)]
        mod invariants;

        pub use crate::meta_types::{FieldKind, FieldSpec};

        #(#reexports)*

        #[doc = #ops_doc]
        #[doc = ""]
        #[doc = #models_doc]
        pub static OP_FIELDS: &[(&str, &[FieldSpec])] = &[#(#entries),*];
    })
}

// ----------------------------------------------------------------------------- invariants.rs

fn invariants_file(cx: &Context<'_>) -> TokenStream {
    // One assertion per field a cycle passes through, named. Without these the cycle-breaking
    // rule is invisible: the tables simply compile, and the next person to "improve"
    // `Repository.parent` into a `FieldKind::Object` gets a `cycle detected when simplifying
    // constant` error pointing at generated code they did not write.
    let cycle_asserts = cx.broken_fields.iter().map(|(owner, field)| {
        let stat = Ident::new(&static_name(cx.models[*owner]), Span::call_site());
        let owner_lit = *owner;
        let field_lit = *field;
        quote! {
            {
                let f = super::#stat
                    .iter()
                    .find(|f| f.name == #field_lit)
                    .unwrap_or_else(|| panic!("{}.{} is missing", #owner_lit, #field_lit));
                assert!(
                    is_opaque(&f.kind),
                    "{}.{} closes a $ref cycle, so it must be json; found {}",
                    #owner_lit,
                    #field_lit,
                    f.kind.label()
                );
            }
        }
    });

    quote! {
        #![doc = " Invariants of the generated field tables."]

        use std::collections::BTreeSet;

        use super::OP_FIELDS;
        use crate::meta_types::{FieldKind, FieldSpec};

        /// `Json`, or a collection of it: what a broken cycle edge looks like.
        fn is_opaque(kind: &FieldKind) -> bool {
            match kind {
                FieldKind::Json => true,
                FieldKind::Array(inner) | FieldKind::Map(inner) => is_opaque(inner),
                _ => false,
            }
        }

        /// `gea` binary-searches `OP_FIELDS`. Unsorted, bare `--json` reports "no such
        /// operation" for roughly half the API while the operation itself works fine.
        #[test]
        fn op_fields_is_sorted_by_op_id() {
            for w in OP_FIELDS.windows(2) {
                assert!(w[0].0 < w[1].0, "OP_FIELDS is not sorted: {:?}", (w[0].0, w[1].0));
            }
            for (op_id, _) in OP_FIELDS {
                assert!(
                    OP_FIELDS.binary_search_by(|(k, _)| (*k).cmp(op_id)).is_ok(),
                    "{op_id} is not findable by binary search"
                );
            }
        }

        /// Bare `--json` prints this list, and an unsorted list is unreadable at sixty fields.
        #[test]
        fn every_field_table_is_sorted_by_name() {
            for (op_id, fields) in OP_FIELDS {
                for w in fields.windows(2) {
                    assert!(
                        w[0].name < w[1].name,
                        "{op_id}: field table is not sorted: {:?}",
                        (w[0].name, w[1].name)
                    );
                }
            }
        }

        /// A field name is a JSON key, so an empty one means the table cannot be used to
        /// project anything.
        #[test]
        fn field_names_are_non_empty_and_unique_per_table() {
            for (op_id, fields) in OP_FIELDS {
                let mut seen = BTreeSet::new();
                for f in *fields {
                    assert!(!f.name.is_empty(), "{op_id} has an unnamed field");
                    assert!(seen.insert(f.name), "{op_id}: duplicate field {:?}", f.name);
                }
            }
        }

        /// Every `op_id` here must be a real operation, or bare `--json` and the command tree
        /// disagree about what exists.
        #[test]
        fn op_fields_keys_are_all_real_operations() {
            let ops: BTreeSet<&str> = crate::meta::OPS.iter().map(|o| o.op_id).collect();
            for (op_id, _) in OP_FIELDS {
                assert!(ops.contains(op_id), "{op_id} is in OP_FIELDS but not in OPS");
            }
        }

        /// `--json` on a list selects fields of each element, matching `gh`. So the table for an
        /// operation returning `Vec<Issue>` is the same one used for a single `Issue`.
        #[test]
        fn a_list_endpoint_offers_its_element_type_s_fields() {
            let one = fields_for("issueGetIssue").expect("issueGetIssue returns an Issue");
            let many = fields_for("issueListIssues").expect("issueListIssues returns Vec<Issue>");
            assert!(
                std::ptr::eq(one.as_ptr(), many.as_ptr()),
                "a list endpoint must point at the same table as its element endpoint"
            );
            assert!(one.iter().any(|f| f.name == "number"));
        }

        fn fields_for(op_id: &str) -> Option<&'static [FieldSpec]> {
            OP_FIELDS
                .binary_search_by(|(k, _)| (*k).cmp(op_id))
                .ok()
                .map(|i| OP_FIELDS[i].1)
        }

        /// Fields that close a `$ref` cycle must be `FieldKind::Json`.
        ///
        /// `Repository.parent` is a `Repository` and `GPGKey.subkeys` is a `Vec<GPGKey>`. Making
        /// either a `FieldKind::Object` pointing back at its own table is not a runtime
        /// recursion — it is a `cycle detected when simplifying constant` compile error, because
        /// the `&'static` value it describes cannot exist. `Json` is the honest label for it:
        /// "opaque nested value, reach into it with `--jq`". This test exists so the rule is
        /// visible, rather than being rediscovered by whoever tries to "improve" it.
        #[test]
        fn reference_cycles_are_broken_with_opaque_json() {
            #(#cycle_asserts)*
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn no_emitted_file_busts_the_review_cap() {
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
        assert_eq!(emit(ir()).unwrap(), emit(ir()).unwrap());
    }

    #[test]
    fn the_only_broken_edges_are_the_two_reference_cycles() {
        // `Repository.parent: Repository` and `GPGKey.subkeys: Vec<GPGKey>`. The second is
        // deliberately here even though `ir::lower` does *not* box it: a `Vec` is enough
        // indirection for a Rust type's size, but not for a `&'static` field table, where
        // `Array(&Object(FIELDS_GPGKEY))` inside `FIELDS_GPGKEY` is still a self-reference and
        // therefore a compile error.
        let cx = Context::build(ir()).unwrap();
        assert_eq!(
            cx.broken,
            [("GPGKey", "GPGKey"), ("Repository", "Repository")].into_iter().collect()
        );
    }

    #[test]
    fn a_list_success_type_resolves_to_its_element_model() {
        let ir = ir();
        let cx = Context::build(ir).unwrap();
        assert_eq!(cx.op_models.get("issueListIssues"), Some(&"Issue"));
        assert_eq!(cx.op_models.get("issueGetIssue"), Some(&"Issue"));
    }

    #[test]
    fn responses_with_nothing_to_select_get_no_entry() {
        // A 204, a plain-text body and a byte stream have no fields; an empty table would be a
        // lie, and `gea` reports "this command has no --json fields" from the absence.
        let cx = Context::build(ir()).unwrap();
        assert!(!cx.op_models.contains_key("issueDelete"));
        assert!(!cx.op_models.contains_key("repoGetRawFile"));
    }

    #[test]
    fn every_nested_object_reference_has_a_table() {
        // The closure pass is what makes this true; without it a `FieldKind::Object` would name
        // a static that was never emitted.
        let ir = ir();
        let cx = Context::build(ir).unwrap();
        for name in cx.models.keys() {
            let ModelKind::Struct(fields) = &cx.models[name].kind else { unreachable!() };
            for f in fields {
                // Resolving the kind is what would fail; do it for every field of every table.
                field_kind(&cx, name, &f.ty).unwrap_or_else(|e| panic!("{name}.{}: {e}", f.wire));
            }
        }
    }

    #[test]
    fn broken_edges_catches_self_loops_and_mutual_cycles_but_not_shared_references() {
        // `A -> A` is `Repository.parent`. `B <-> C` is the mutual cycle the spec does not have
        // today; both directions are broken rather than one picked arbitrarily, so the output
        // does not depend on traversal order. `D -> A` is a plain shared reference (`User`
        // appearing in a dozen models) and must keep its full nesting.
        let g: BTreeMap<&str, BTreeSet<&str>> = [
            ("A", ["A"].into_iter().collect()),
            ("B", ["C"].into_iter().collect()),
            ("C", ["B"].into_iter().collect()),
            ("D", ["A"].into_iter().collect()),
        ]
        .into_iter()
        .collect();
        let models: BTreeMap<&str, &Model> = BTreeMap::new();
        // `broken_edges` iterates `models`, so drive it through the graph keys directly.
        let mut out = BTreeSet::new();
        for owner in g.keys() {
            for target in &g[owner] {
                if can_reach(&g, target, owner) {
                    out.insert((*owner, *target));
                }
            }
        }
        assert_eq!(out, [("A", "A"), ("B", "C"), ("C", "B")].into_iter().collect());
        assert!(broken_edges(&g, &models).is_empty(), "no models, no edges");
    }

    #[test]
    fn can_reach_terminates_on_a_cycle_it_is_not_looking_for() {
        let g: BTreeMap<&str, BTreeSet<&str>> =
            [("A", ["B"].into_iter().collect()), ("B", ["A"].into_iter().collect())]
                .into_iter()
                .collect();
        assert!(!can_reach(&g, "A", "C"));
    }
}

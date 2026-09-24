//! The `client` emitter: [`Ir::operations`] → `crates/gitea-client/src/generated/{ops,query}/**`.
//!
//! # Every generated `fn` is concrete. No generics, anywhere, ever.
//!
//! This is the rule the whole emitter is organised around, and it is a compile-time budget
//! decision rather than a style preference. There are 482 operations. A single generic parameter
//! — `body: &impl Serialize`, or a `T: DeserializeOwned` return — monomorphises per operation
//! *and* per call site, so the cost of 42k generated lines stops being linear in the line count
//! and starts being linear in how much code uses it. `gea` alone would instantiate the same
//! machinery hundreds of times.
//!
//! So every polymorphic thing lives in hand-written `gitea-core`: `Client::json` is generic,
//! `create_pull_request` is not. Concretely, that means:
//!
//! - Request bodies are `&gitea_model::CreatePullRequestOption`, never `impl Serialize`.
//! - Responses are named types; the generic `Client::json::<T>` call is the only turbofish.
//! - A streamed body is `ByteStream` (a boxed trait object) and a paginated one is `ItemStream<T>`
//!   — both concrete, so they can appear in a published signature without an `impl Trait` that
//!   leaks an unnameable type.
//! - `<Op>Query` builders take `&str`, not `impl Into<String>`, for exactly the same reason.
//!
//! # Method shape
//!
//! ```text
//! impl Repo<'_> {
//!     pub async fn create_pull_request(
//!         &self, owner: &str, repo: &str, body: &gitea_model::CreatePullRequestOption,
//!     ) -> Result<gitea_model::PullRequest> { … }
//! }
//! ```
//!
//! Path parameters are positional, **in path order** — that order is public API, so it comes from
//! [`crate::ir::paths::PathTemplate`] and not from the spec's parameter array. Query parameters
//! always collapse into one generated `<OpPascal>Query` struct, even when there is only one of
//! them: several operations take more than a dozen, and an arity cliff where some operations take
//! their filters as arguments and others take a struct would be worse than either rule alone.
//!
//! Paginated operations that return a bare JSON array get **two** methods: `list_x` returning an
//! `ItemStream`, and `list_x_page` returning one page plus its `PageInfo`, for callers who need
//! `x-total-count` or want to drive pagination themselves.
//!
//! # What this emitter is not allowed to decide
//!
//! Names come from [`crate::ir::names`], types from [`crate::ir::types`], path tokenization from
//! [`crate::ir::paths`], and doc text from [`crate::ir::doc`]. This module contains no string
//! casing, no `$ref` resolution, and no `overrides.toml`. Where a decision *looks* like it is
//! made here — which `Client` exit an operation uses, which `Accept` it sends — it is a pure
//! function of `Operation::success` and `Operation::produces`, and it fails loudly rather than
//! falling back, because a silent fallback to JSON on a `zip` endpoint hands the user a
//! `Result<Value>` that can never succeed.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::quote;

use super::{GeneratedFile, MAX_LINES, render};
use crate::Result;
use crate::ir::doc::Doc;
use crate::ir::names::{Ident as IrIdent, ident_pascal};
use crate::ir::paths::Chunk;
use crate::ir::types::{Mime, RustType};
use crate::ir::{Group, Ir, Operation, Pagination, Param, PathEncoding};
use crate::swagger::HttpMethod;

const OPS_ROOT: &str = "crates/gitea-client/src/generated/ops";
const QUERY_ROOT: &str = "crates/gitea-client/src/generated/query";
/// The one-constant module naming the spec this tree was generated from.
const SPEC_FILE: &str = "crates/gitea-client/src/generated/spec.rs";

/// Line budget one file aims for, deliberately under [`MAX_LINES`].
///
/// The headroom is what keeps the *layout* stable. Splitting is automatic, so a file sitting at
/// 1,499 lines today would fan out into a dozen letter-keyed files the moment upstream adds one
/// operation — and that diff buries the one real change. At 80% of the cap a group has to grow by
/// a quarter before its files move. Same constant and same reasoning as [`super::meta`].
const BUDGET: usize = MAX_LINES * 4 / 5;

/// Argument names the generated signatures claim for themselves. A path parameter spelled the
/// same way would shadow one of them, and the generated body would silently use the wrong value.
const RESERVED_ARGS: [&str; 4] = ["body", "query", "paging", "progress"];

pub fn emit(ir: &Ir) -> Result<Vec<GeneratedFile>> {
    let ctx = Ctx::new(ir)?;
    let mut files = Vec::new();

    let ops_layout = ctx.ops_layout()?;
    for (group, leaves) in &ops_layout {
        let g = ctx.group(group)?;
        let stems: Vec<&str> = leaves.keys().filter(|s| *s != "mod").map(String::as_str).collect();
        let inline: &[usize] = leaves.get("mod").map_or(&[], Vec::as_slice);

        files.push(ctx.file(
            format!("{OPS_ROOT}/{}/mod.rs", g.module.as_str()),
            ctx.group_mod(g, &stems, inline)?,
        )?);
        for stem in &stems {
            files.push(ctx.file(
                format!("{OPS_ROOT}/{}/{stem}.rs", g.module.as_str()),
                ctx.leaf_file(g, stem, &leaves[*stem])?,
            )?);
        }
    }
    files.push(ctx.file(format!("{OPS_ROOT}/mod.rs"), ctx.ops_mod(&ops_layout)?)?);

    let query_layout = ctx.query_layout()?;
    for (stem, ops) in &query_layout {
        files.push(ctx.file(format!("{QUERY_ROOT}/{stem}.rs"), ctx.query_file(stem, ops)?)?);
    }
    files.push(ctx.file(format!("{QUERY_ROOT}/mod.rs"), ctx.query_mod(&query_layout)?)?);
    files.push(ctx.file(SPEC_FILE.to_owned(), ctx.spec_module())?);

    Ok(files)
}

struct Ctx<'a> {
    ir: &'a Ir,
    /// Definition name → the Rust type name `gitea-model` exports it under.
    models: BTreeMap<&'a str, &'a IrIdent>,
    /// Open-enum type names, so a response referring to one can be spelled.
    enums: BTreeSet<&'a str>,
}

impl<'a> Ctx<'a> {
    fn new(ir: &'a Ir) -> Result<Self> {
        let ctx = Ctx {
            ir,
            models: ir.models.iter().map(|m| (m.wire.as_str(), &m.rust)).collect(),
            enums: ir.open_enums.iter().map(|e| e.name.as_str()).collect(),
        };
        for op in &ir.operations {
            for p in &op.path_params {
                if RESERVED_ARGS.contains(&p.rust.as_str()) {
                    bail!(
                        "{}: path parameter {:?} mangles to `{}`, which is the name the generated \
                         signature gives one of its own arguments ({}). Rename it in \
                         crates/xtask/src/overrides.toml; otherwise the generated body would read \
                         the wrong value.",
                        op.op_id,
                        p.wire,
                        p.rust,
                        RESERVED_ARGS.join(", "),
                    );
                }
            }
        }
        Ok(ctx)
    }

    fn file(&self, path: String, tokens: TokenStream) -> Result<GeneratedFile> {
        Ok(GeneratedFile::new(path, render(tokens, &self.ir.spec_version, &self.ir.spec_sha256)?))
    }

    fn group(&self, name: &str) -> Result<&'a Group> {
        self.ir
            .groups
            .iter()
            .find(|g| g.name == name)
            .ok_or_else(|| format!("group {name:?} has operations but no `Group` entry").into())
    }

    // ---------------------------------------------------------------------------- file layout

    /// Which file each operation's methods land in: group name → file stem → operation indices.
    ///
    /// The stem `"mod"` means the group's own `mod.rs`, which is where a small group's methods
    /// live so that `ops/topic/mod.rs` is not a one-line file next to a one-method one.
    ///
    /// The primary key is the IR's own `sub_bucket`, derived from the API's URL structure, so a
    /// reader looking for `create_pull_request` finds it in `ops/repo/pulls.rs`. Buckets that
    /// still bust [`BUDGET`] — `user` and `org` are single buckets of 80-odd operations — split
    /// again on the command's first letter. That key beats a greedy line-count packing because it
    /// is *stable*: a new `list-whatever` lands in `l.rs` and moves nothing else.
    /// The module carrying [`Ir::spec_version`] as a constant.
    ///
    /// Generated rather than hand-written for the reason every constant that restates a fact
    /// should be: the value comes from the same `Ir` that stamps every banner in this tree, so
    /// it cannot drift from `spec/lock.toml` the way a hand-maintained `const` would.
    ///
    /// It lives here and not in `gitea-core` because core is deliberately spec-agnostic, and
    /// because the only drift-proof spelling available there — `include_str!` of
    /// `spec/lock.toml` — reads a file outside the package root, which `cargo package` drops.
    /// That would break publishing; `gea` gets away with the same trick only because it is
    /// `publish = false`.
    fn spec_module(&self) -> TokenStream {
        let version = &self.ir.spec_version;
        let sha = &self.ir.spec_sha256;
        quote! {
            #![doc = " The Gitea API description this crate was generated from."]

            #[doc = " The Gitea release the vendored API description came from, e.g. `1.27.2`."]
            #[doc = ""]
            #[doc = " This crate tracks exactly one Gitea version. Reporting it — in a"]
            #[doc = " `User-Agent`, in `--version`, in a bug report — is the difference between"]
            #[doc = " \"the server rejected this\" and \"the server is newer than this client\"."]
            pub const SPEC_VERSION: &str = #version;

            #[doc = " sha256 of the canonical specification JSON, as recorded in `spec/lock.toml`."]
            #[doc = ""]
            #[doc = " The same hash every generated file's banner carries, so \"which description"]
            #[doc = " produced this build\" is answerable from a running binary and not only from"]
            #[doc = " the source tree."]
            pub const SPEC_SHA256: &str = #sha;
        }
    }

    fn ops_layout(&self) -> Result<BTreeMap<String, BTreeMap<String, Vec<usize>>>> {
        let mut out = BTreeMap::new();
        for g in &self.ir.groups {
            let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for &i in &g.operations {
                buckets.entry(self.ir.operations[i].sub_bucket.clone()).or_default().push(i);
            }

            // Named buckets first: their split is independent of everything else, and `mod.rs`
            // needs the resulting stem list before it can be measured.
            let mut leaves: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            let inline = buckets.remove("mod").unwrap_or_default();
            for (bucket, ops) in buckets {
                let lines = self.measure(self.leaf_file(g, &bucket, &ops)?)?;
                if lines <= BUDGET {
                    leaves.insert(bucket, ops);
                    continue;
                }
                for (stem, ops) in self.by_letter(&ops, |c| format!("{bucket}_{c}"))? {
                    self.reject_oversize(g, &stem, self.leaf_file(g, &stem, &ops)?)?;
                    leaves.insert(stem, ops);
                }
            }

            if !inline.is_empty() {
                let stems: Vec<&str> = leaves.keys().map(String::as_str).collect();
                let lines = self.measure(self.group_mod(g, &stems, &inline)?)?;
                if lines <= BUDGET {
                    leaves.insert("mod".to_owned(), inline);
                } else {
                    for (stem, ops) in self.by_letter(&inline, |c| c.to_string())? {
                        self.reject_oversize(g, &stem, self.leaf_file(g, &stem, &ops)?)?;
                        leaves.insert(stem, ops);
                    }
                }
            }

            out.insert(g.name.clone(), leaves);
        }
        Ok(out)
    }

    /// One file per group for the `<Op>Query` structs, letter-split when a group's structs do not
    /// fit. Query structs are chunkier than methods — a dozen fields, a builder each — so `repo`
    /// splits here even though its methods are already bucketed.
    fn query_layout(&self) -> Result<BTreeMap<String, Vec<usize>>> {
        let mut out = BTreeMap::new();
        for g in &self.ir.groups {
            let ops: Vec<usize> = g
                .operations
                .iter()
                .copied()
                .filter(|&i| self.ir.operations[i].query_struct.is_some())
                .collect();
            if ops.is_empty() {
                continue;
            }
            let stem = g.module.as_str().to_owned();
            if self.measure(self.query_file(&stem, &ops)?)? <= BUDGET {
                out.insert(stem, ops);
                continue;
            }
            for (stem, ops) in self.by_letter(&ops, |c| format!("{}_{c}", g.module.as_str()))? {
                self.reject_oversize(g, &stem, self.query_file(&stem, &ops)?)?;
                out.insert(stem, ops);
            }
        }
        Ok(out)
    }

    /// Group operations by the first letter of their command, which is where a reader would look
    /// for them and which does not move when a neighbour is added.
    fn by_letter(
        &self,
        ops: &[usize],
        stem: impl Fn(char) -> String,
    ) -> Result<BTreeMap<String, Vec<usize>>> {
        let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for &i in ops {
            let op = &self.ir.operations[i];
            let Some(c) = op.command.chars().next().filter(char::is_ascii_alphabetic) else {
                bail!(
                    "command {:?} does not start with an ASCII letter, so it cannot be keyed to a \
                     letter file. Pin the operation's name in crates/xtask/src/overrides.toml.",
                    op.command
                );
            };
            out.entry(stem(c.to_ascii_lowercase())).or_default().push(i);
        }
        Ok(out)
    }

    /// Rendered line count. Rendering is the only honest measurement: `prettyplease` decides
    /// where the breaks go, not this emitter.
    fn measure(&self, tokens: TokenStream) -> Result<usize> {
        Ok(render(tokens, &self.ir.spec_version, &self.ir.spec_sha256)?.lines().count())
    }

    fn reject_oversize(&self, g: &Group, stem: &str, tokens: TokenStream) -> Result<()> {
        let lines = self.measure(tokens)?;
        if lines > MAX_LINES {
            bail!(
                "{stem}.rs would be {lines} lines, over the {MAX_LINES}-line review cap, even \
                 after splitting the {:?} group by the command's first letter. Add a `sub_bucket` \
                 rule for it in crates/xtask/src/ir/lower.rs rather than raising the cap: the \
                 point of committing generated code is that its diff is reviewable.",
                g.name
            );
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------ ops/mod.rs

    /// The group accessors, `impl`emented onto the hand-written `gitea_client::Api`.
    ///
    /// Generated rather than hand-written next to `Api` itself, because a hand-written list of 17
    /// accessors is a list that silently goes stale: a spec bump that adds a group would add a
    /// module here and no way to reach it, and nothing would fail. `Api` stays hand-written
    /// because *it* has no generated content — one field and one constructor.
    fn ops_mod(
        &self,
        layout: &BTreeMap<String, BTreeMap<String, Vec<usize>>>,
    ) -> Result<TokenStream> {
        let mut mods = TokenStream::new();
        let mut reexports = TokenStream::new();
        let mut accessors = TokenStream::new();

        for group in layout.keys() {
            let g = self.group(group)?;
            let module = g.module.to_ident();
            let ty = group_ty(g)?;
            let accessor = g.module.to_ident();
            let doc = format!(" {}.", g.doc.trim_end_matches('.'));

            // The module is private and the type is re-exported: `gitea_client::ops::Repo` is
            // the path worth documenting, and a published crate should not expose a module tree
            // whose shape is a line-count artefact.
            mods.extend(quote! { mod #module; });
            reexports.extend(quote! {
                #[doc = #doc]
                pub use self::#module::#ty;
            });
            accessors.extend(quote! {
                #[doc = #doc]
                pub fn #accessor(&self) -> #ty<'_> {
                    #ty::new(self.client())
                }
            });
        }

        Ok(quote! {
            #![doc = " One typed method per API operation, grouped the way `gea` groups commands."]
            #![doc = ""]
            #![doc = " Every method here is concrete: no type parameters, no `impl Trait` arguments."]
            #![doc = " All polymorphism lives in hand-written `gitea_core::http`. See the emitter"]
            #![doc = " docs in `crates/xtask/src/emit/client.rs` for why that is load-bearing."]

            #mods

            #reexports

            impl crate::Api {
                #accessors
            }
        })
    }

    // ------------------------------------------------------------------- ops/<group>/{mod,*}.rs

    /// `ops/<group>/mod.rs`: the group struct, its submodules, and — for a group small enough to
    /// need no split — its methods.
    fn group_mod(&self, g: &Group, stems: &[&str], inline: &[usize]) -> Result<TokenStream> {
        let ty = group_ty(g)?;
        let mods: Vec<TokenStream> = stems
            .iter()
            .map(|s| {
                let m = IrIdent::new(*s).to_ident();
                // Private: these files hold nothing but `impl` blocks, and the methods surface on
                // the group struct regardless of which module the block sits in.
                quote! { mod #m; }
            })
            .collect();

        let mut needs = Needs::default();
        let methods = self.methods(inline, &mut needs)?;
        let imports = needs.imports(true);

        let module_doc = format!(" `{}` — {}.", g.name, g.doc.trim_end_matches('.'));
        let struct_doc = format!(" {}.", g.doc.trim_end_matches('.'));
        let accessor_doc = format!(
            " Obtain one from `Api::{}`, or wrap a bare `Client` with `{}::new`.",
            g.module, ty,
        );

        Ok(quote! {
            #![doc = #module_doc]

            #imports

            #(#mods)*

            #[doc = #struct_doc]
            #[doc = ""]
            #[doc = #accessor_doc]
            #[derive(Debug, Clone, Copy)]
            pub struct #ty<'a> {
                // `pub(crate)` rather than private: the methods live in sibling modules, and a
                // getter would be one more thing for 506 generated bodies to call.
                pub(crate) client: &'a Client,
            }

            impl<'a> #ty<'a> {
                #[doc = " Borrow a client as this operation group. Free — no allocation, no clone."]
                pub fn new(client: &'a Client) -> Self {
                    Self { client }
                }

                #methods
            }
        })
    }

    /// `ops/<group>/<stem>.rs`: one `impl` block of methods, nothing else.
    fn leaf_file(&self, g: &Group, stem: &str, ops: &[usize]) -> Result<TokenStream> {
        let ty = group_ty(g)?;
        let mut needs = Needs::default();
        let methods = self.methods(ops, &mut needs)?;
        let imports = needs.imports(false);
        let doc = format!(" `{}` operations: `{stem}`.", g.name);

        Ok(quote! {
            #![doc = #doc]

            #imports

            impl super::#ty<'_> {
                #methods
            }
        })
    }

    fn methods(&self, ops: &[usize], needs: &mut Needs) -> Result<TokenStream> {
        let mut out = TokenStream::new();
        for &i in ops {
            let op = &self.ir.operations[i];
            out.extend(self.method(op, needs).map_err(|e| format!("{}: {e}", op.op_id))?);
        }
        Ok(out)
    }

    // --------------------------------------------------------------------------- one operation

    fn method(&self, op: &Operation, needs: &mut Needs) -> Result<TokenStream> {
        let exit = self.exit(op)?;
        let name = op.fn_name.to_ident();
        let args = self.args(op, needs)?;
        let (pre, req) = self.request(op, needs)?;
        let deprecated = deprecated_attr(op);
        needs.result = true;

        let mut out = TokenStream::new();

        if let Exit::Items { item } = &exit {
            needs.http.insert("ItemStream");
            needs.http.insert("PageInfo");
            needs.http.insert("Paging");
            let page_name = page_fn_name(op)?;
            let page_param = match &op.pagination {
                Pagination::Paged { page_param, .. } => page_param.as_str(),
                Pagination::None => unreachable!("Exit::Items is only built for a paged operation"),
            };
            let stream_doc = self.doc(
                op,
                &[
                    " Every item, as a stream that follows the collection to its end.",
                    " The runtime resolves the page size from `/settings/api` and terminates on the",
                    " `Link` header rather than on a short page, because Gitea silently clamps",
                    " `limit` to `max_response_items`.",
                    " Any page number set on the query struct is ignored here — the walk starts at",
                    " the beginning. Use the `_page` method for one specific page, and",
                    " `futures::StreamExt::take` to cap how many items you consume.",
                ],
            );
            let page_doc = self.doc(
                op,
                &[
                    " One page, plus what its headers said — `x-total-count` and `Link`.",
                    " For callers driving pagination themselves; the page number comes from the",
                    " query struct, and `Paging` narrows the page size.",
                ],
            );
            let stream_arity = arity_allow(args.len());
            let page_arity = arity_allow(args.len() + 1);
            out.extend(quote! {
                #stream_doc
                #deprecated
                #stream_arity
                pub fn #name(#(#args),*) -> ItemStream<#item> {
                    #pre
                    let req = crate::stream_request(#req, #page_param);
                    self.client.items(req)
                }

                #page_doc
                #deprecated
                #page_arity
                pub async fn #page_name(#(#args),*, paging: Paging) -> Result<(Vec<#item>, PageInfo)> {
                    #pre
                    let req = crate::page_request(#req, paging);
                    self.client.page(req).await
                }
            });
            return Ok(out);
        }

        let doc = self.doc(op, &[]);
        let (ret, call) = match &exit {
            Exit::Empty => (quote! { () }, quote! { self.client.empty(req).await }),
            Exit::Text => (quote! { String }, quote! { crate::text(self.client, req).await }),
            Exit::Bytes => {
                needs.http.insert("ByteStream");
                needs.http.insert("Mime");
                (quote! { (Mime, ByteStream) }, quote! { self.client.bytes(req).await })
            }
            Exit::Value => {
                (quote! { ::serde_json::Value }, quote! { self.client.value(req).await })
            }
            Exit::Json(ty) => (ty.clone(), quote! { self.client.json(req).await }),
            Exit::JsonList(item) => (
                quote! { Vec<#item> },
                quote! {
                    Ok(self.client.json::<Option<Vec<#item>>>(req).await?.unwrap_or_default())
                },
            ),
            Exit::Items { .. } => unreachable!("handled above"),
        };

        let arity = arity_allow(args.len());
        out.extend(quote! {
            #doc
            #deprecated
            #arity
            pub async fn #name(#(#args),*) -> Result<#ret> {
                #pre
                let req = #req;
                #call
            }
        });
        Ok(out)
    }

    /// The method's doc comment: the spec's own prose, then the endpoint, then the scope.
    ///
    /// The endpoint line earns its place in a generated SDK — it is the one fact that lets a
    /// reader match a method against Gitea's own API documentation — and the scope line answers
    /// the question every 403 raises, at the place where it can be acted on.
    fn doc(&self, op: &Operation, extra: &[&str]) -> TokenStream {
        let mut ts = doc_lines(&op.doc);
        if !op.doc.rustdoc.is_empty() && !extra.is_empty() {
            ts.extend(quote! { #[doc = ""] });
        }
        for line in extra {
            ts.extend(quote! { #[doc = #line] });
        }
        if !op.doc.rustdoc.is_empty() || !extra.is_empty() {
            ts.extend(quote! { #[doc = ""] });
        }
        let endpoint = match &op.scope {
            Some(scope) => {
                format!(
                    " `{} {}` — needs the `{scope}` token scope.",
                    op.method.as_str(),
                    op.path.raw
                )
            }
            None => format!(" `{} {}`.", op.method.as_str(), op.path.raw),
        };
        ts.extend(quote! { #[doc = #endpoint] });
        ts
    }

    /// The argument list, in the one order every generated method uses: `&self`, path parameters
    /// in path order, then the payload (a body or the form-data parts), then the query struct,
    /// then `Progress` for an upload.
    fn args(&self, op: &Operation, needs: &mut Needs) -> Result<Vec<TokenStream>> {
        let mut args = vec![quote! { &self }];

        for p in &op.path_params {
            let ident = p.rust.to_ident();
            let ty = self.arg_ty(&p.ty).ok_or_else(|| {
                format!(
                    "path parameter {:?} has type {:?}, which has no argument form",
                    p.wire, p.ty
                )
            })?;
            args.push(quote! { #ident: #ty });
        }

        if let Some(body) = &op.body {
            let ty = self.body_arg_ty(&body.ty)?;
            args.push(quote! { body: #ty });
        }

        for p in &op.form_data {
            let ident = p.rust.to_ident();
            if p.ty == RustType::File {
                needs.http.insert("Part");
                let ty = if p.required {
                    quote! { Part }
                } else {
                    quote! { Option<Part> }
                };
                args.push(quote! { #ident: #ty });
                continue;
            }
            let ty = self.arg_ty(&p.ty).ok_or_else(|| {
                format!("form field {:?} has type {:?}, which has no argument form", p.wire, p.ty)
            })?;
            let ty = if p.required {
                ty
            } else {
                quote! { Option<#ty> }
            };
            args.push(quote! { #ident: #ty });
        }

        if let Some(q) = &op.query_struct {
            let ty = q.to_ident();
            args.push(quote! { query: &crate::query::#ty });
        }

        if !op.form_data.is_empty() {
            needs.http.insert("Progress");
            args.push(quote! { progress: Progress });
        }

        Ok(args)
    }

    /// Statements the body needs before the request expression, and the request expression.
    fn request(&self, op: &Operation, needs: &mut Needs) -> Result<(TokenStream, TokenStream)> {
        needs.http.insert("Request");
        let ctor = match op.method {
            HttpMethod::Get => quote! { get },
            HttpMethod::Post => quote! { post },
            HttpMethod::Put => quote! { put },
            HttpMethod::Patch => quote! { patch },
            HttpMethod::Delete => quote! { delete },
            // `Request` has no constructor for these, and inventing one here would mean the
            // generated crate depending on `http::Method` directly.
            HttpMethod::Head | HttpMethod::Options => bail!(
                "method {} has no `Request` constructor. Add one to gitea-core::http::Request \
                 before the spec starts using it.",
                op.method.as_str()
            ),
        };

        let path = self.path_expr(op, needs)?;
        let mut req = quote! { Request::#ctor(#path) };

        // The scope the operation needs, carried on the request so a 403 can name it. This is
        // the authoritative value — `tags[0]` plus the method, corrected in `overrides.toml` —
        // and the runtime's `infer_scope` fallback is a reconstruction that disagrees with it on
        // every pull-request route. A `&'static str`: no generics, nothing to monomorphise.
        if let Some(scope) = op.scope.as_deref() {
            req = quote! { #req.scope(#scope) };
        }

        if let Some(accept) = self.accept(op)? {
            needs.http.insert("Accept");
            req = quote! { #req.accept(#accept) };
        }

        let mut pre = TokenStream::new();
        if let Some(body) = &op.body {
            match (&body.ty, &op.consumes) {
                // 124 of 125 bodies are `$ref`s to named definitions, so this is the path almost
                // every write operation takes.
                (RustType::Model(_) | RustType::Json, Some(Mime::Json) | None) => {
                    req = quote! { #req.json_body(body)? };
                }
                // `renderMarkdownRaw` posts the markdown itself as `text/plain`.
                (RustType::String, Some(mime)) => {
                    needs.http.insert("Body");
                    needs.http.insert("Source");
                    let mime = mime.as_str();
                    req = quote! {
                        #req.body(Body::Octets {
                            mime: #mime.to_owned(),
                            src: Source::Bytes(body.as_bytes().to_vec()),
                        })
                    };
                }
                (ty, consumes) => bail!(
                    "a {ty:?} body with `consumes: {consumes:?}` has no encoding here. Teach the \
                     client emitter how to send it rather than letting it default to JSON."
                ),
            }
        }

        if !op.form_data.is_empty() {
            if op.consumes != Some(Mime::MultipartFormData) {
                bail!(
                    "has formData parameters but `consumes: {:?}`. Only multipart is streamed; \
                     anything else would have to be urlencoded, which would buffer an upload.",
                    op.consumes
                );
            }
            needs.http.insert("Body");
            needs.http.insert("Part");
            // Required fields go straight into the initialiser and optional ones are pushed
            // after. Splitting them that way is not cosmetic: `Vec::new()` followed by one
            // unconditional `push` trips `clippy::vec_init_then_push`, and CI builds the
            // generated tree with `-D warnings`.
            let mut init = Vec::new();
            let mut pushes = TokenStream::new();
            for p in &op.form_data {
                let ident = p.rust.to_ident();
                let wire = p.wire.as_str();
                let build = if p.ty == RustType::File {
                    // The form field name comes from the specification, never from the caller's
                    // `Part`: Gitea matches on it, and a mismatch is a 422 that names no field.
                    quote! { Part { name: #wire.to_owned(), ..#ident } }
                } else {
                    quote! { Part::bytes(#wire, #ident.as_bytes().to_vec()) }
                };
                if p.required {
                    init.push(build);
                } else {
                    pushes.extend(quote! { if let Some(#ident) = #ident { parts.push(#build); } });
                }
            }
            let mutable = (!pushes.is_empty()).then(|| quote! { mut });
            // `Vec::from([…])` rather than `vec![…]` because `prettyplease` does not format
            // inside a macro invocation — it prints the token stream verbatim, spaces before
            // colons and all — and a committed generated file should not have one line that looks
            // like nobody ran a formatter over it.
            pre.extend(quote! {
                let #mutable parts: Vec<Part> = Vec::from([#(#init),*]);
                #pushes
            });
            req = quote! { #req.body(Body::Multipart(parts)).progress(progress) };
        }

        if op.query_struct.is_some() {
            req = quote! { query.apply(#req) };
        }
        Ok((pre, req))
    }

    /// The path expression: a `format!` over [`Chunk`]s, with per-parameter encoding.
    ///
    /// Walking the chunks rather than substituting `{name}` at runtime is what makes
    /// `/repos/{owner}/{repo}/pulls/{index}.{diffType}` work: the `.` is a literal chunk between
    /// two parameters, so `diffType` is never encoded into a filename and `index` is never
    /// encoded together with it.
    fn path_expr(&self, op: &Operation, needs: &mut Needs) -> Result<TokenStream> {
        let mut fmt = String::new();
        let mut args: Vec<TokenStream> = Vec::new();

        for chunk in &op.path.chunks {
            match chunk {
                Chunk::Lit(s) => {
                    // A brace in a literal would either be a `format!` placeholder we did not
                    // intend or an unsubstituted `{param}` reaching the wire, where the server
                    // answers 404 and the command line looked perfectly fine.
                    if s.contains('{') || s.contains('}') {
                        bail!("path literal {s:?} contains a brace; the template is malformed");
                    }
                    fmt.push_str(s);
                }
                Chunk::Param(id) => {
                    let p = op.path_params.iter().find(|p| p.rust == *id).ok_or_else(|| {
                        format!(
                            "path {:?} uses {{{id}}} but the operation declares no such parameter",
                            op.path.raw
                        )
                    })?;
                    fmt.push_str("{}");
                    args.push(self.path_arg(p, needs)?);
                }
            }
        }

        if args.is_empty() {
            return Ok(quote! { #fmt });
        }
        Ok(quote! { format!(#fmt, #(#args),*) })
    }

    fn path_arg(&self, p: &Param, needs: &mut Needs) -> Result<TokenStream> {
        let ident = p.rust.to_ident();
        Ok(match (&p.ty, p.encoding) {
            (RustType::String, PathEncoding::Segment) => {
                needs.http.insert("encode");
                quote! { encode::seg(#ident) }
            }
            (RustType::String, PathEncoding::PathLike) => {
                needs.http.insert("encode");
                quote! { encode::path_like(#ident) }
            }
            // An integer's `Display` is digits and an optional `-`, all of which RFC 3986 calls
            // unreserved, so encoding it is provably a no-op. Interpolating directly keeps the
            // generated line readable and saves an allocation per call.
            (RustType::I32 | RustType::I64 | RustType::U64, _) => quote! { #ident },
            (ty, _) => bail!(
                "path parameter {:?} has type {ty:?}; only strings and integers can be encoded \
                 into a path.",
                p.wire
            ),
        })
    }

    /// Which `Client` exit an operation uses, from its success type and `produces`.
    fn exit(&self, op: &Operation) -> Result<Exit> {
        Ok(match &op.success.ty {
            RustType::Unit => Exit::Empty,
            RustType::Text => Exit::Text,
            RustType::Bytes => Exit::Bytes,
            RustType::Json => Exit::Value,
            // A paginated operation whose body is a bare JSON array is the only shape
            // `ItemStream` can walk. Ten paged operations answer with a wrapper object instead
            // (`GitTreeResponse`, `SearchResults`); they get one ordinary method and paginate
            // through the `page` field of their query struct, because pretending a wrapper is a
            // stream would decode-fail on the first page.
            RustType::Vec(inner) if !matches!(op.pagination, Pagination::None) => {
                Exit::Items { item: self.ty(inner)? }
            }
            // Unpaginated, and still an array: `null` is the wire form of the empty answer.
            RustType::Vec(inner) => Exit::JsonList(self.ty(inner)?),
            ty => Exit::Json(self.ty(ty)?),
        })
    }

    /// The `Accept` header, or `None` when the default (`application/json`) is right.
    fn accept(&self, op: &Operation) -> Result<Option<TokenStream>> {
        Ok(match &op.success.ty {
            RustType::Text if op.produces.contains(&Mime::TextPlain) => {
                Some(quote! { Accept::Text })
            }
            RustType::Text if op.produces.contains(&Mime::TextHtml) => {
                Some(quote! { Accept::Html })
            }
            RustType::Text => bail!(
                "produces {:?} for a text response; there is no `Accept` for it.",
                op.produces
            ),
            // `*/*`, not `application/octet-stream`: we hand the caller whatever media type
            // arrived, so asking for something narrower could only earn a 406 from a strict
            // proxy for no benefit. Three of these endpoints answer zip or gzip.
            RustType::Bytes => Some(quote! { Accept::Any }),
            RustType::Json if op.produces.contains(&Mime::LdJson) => {
                let mime = Mime::LdJson.as_str();
                Some(quote! { Accept::Other(::std::borrow::Cow::Borrowed(#mime)) })
            }
            _ => None,
        })
    }

    // -------------------------------------------------------------------------- query/<stem>.rs

    fn query_file(&self, stem: &str, ops: &[usize]) -> Result<TokenStream> {
        let mut needs = Needs::default();
        let mut items = TokenStream::new();
        for &i in ops {
            let op = &self.ir.operations[i];
            items.extend(
                self.query_struct(op, &mut needs).map_err(|e| format!("{}: {e}", op.op_id))?,
            );
        }
        let imports = needs.imports(false);
        let doc = format!(" Query-parameter structs for `{stem}`.");
        Ok(quote! {
            #![doc = #doc]

            #imports

            #items
        })
    }

    /// One `<OpPascal>Query`: public fields, `with_*` builders, `to_pairs`, `apply`.
    ///
    /// Every parameter is optional in the struct even when the API requires it, because the struct
    /// derives `Default` and a `Default` that cannot be constructed is not a `Default`. A required
    /// parameter says so in its doc instead; omitting it is a 422 from the server, which is a
    /// better failure than a locally-invented one.
    fn query_struct(&self, op: &Operation, needs: &mut Needs) -> Result<TokenStream> {
        let Some(name) = &op.query_struct else {
            bail!("has no query struct but was assigned to a query file");
        };
        let name = name.to_ident();

        let mut fields = TokenStream::new();
        let mut builders = TokenStream::new();
        let mut pairs = TokenStream::new();

        for p in &op.query_params {
            let ident = p.rust.to_ident();
            let wire = p.wire.as_str();
            let doc = self.param_doc(p);
            let inner = self.query_ty(&p.ty)?;

            // A list parameter is a `Vec`, never an `Option<Vec>`: empty already means absent,
            // and `Option<Vec<_>>` would put `.unwrap_or_default()` at every call site for
            // nothing. Same rule the models emitter follows.
            if let RustType::Vec(elem) = &p.ty {
                let push = self.pair_push(wire, elem)?;
                let setter = builder_name(&p.rust)?;
                fields.extend(quote! { #doc pub #ident: #inner, });
                builders.extend(quote! {
                    #doc
                    pub fn #setter(mut self, #ident: #inner) -> Self {
                        self.#ident = #ident;
                        self
                    }
                });
                pairs.extend(quote! { for v in &self.#ident { #push } });
                continue;
            }

            let push = self.pair_push(wire, &p.ty)?;
            let setter = builder_name(&p.rust)?;
            let arg_ty = self.arg_ty(&p.ty).ok_or_else(|| {
                format!("query parameter {wire:?} has type {:?}, which has no argument form", p.ty)
            })?;
            let assign = match &p.ty {
                RustType::String => quote! { Some(#ident.to_owned()) },
                _ => quote! { Some(#ident) },
            };
            fields.extend(quote! { #doc pub #ident: Option<#inner>, });
            builders.extend(quote! {
                #doc
                pub fn #setter(mut self, #ident: #arg_ty) -> Self {
                    self.#ident = #assign;
                    self
                }
            });
            pairs.extend(quote! { if let Some(v) = &self.#ident { #push } });
        }

        needs.http.insert("Request");
        let struct_doc = format!(" Query parameters for `{}`.", op.op_id);
        let endpoint = format!(" `{} {}`.", op.method.as_str(), op.path.raw);

        Ok(quote! {
            #[doc = #struct_doc]
            #[doc = ""]
            #[doc = #endpoint]
            #[doc = ""]
            #[doc = " Every field is optional and `Default` sends nothing at all, so adding a"]
            #[doc = " parameter upstream cannot change what an existing call sends."]
            #[derive(Debug, Clone, Default, PartialEq)]
            pub struct #name {
                #fields
            }

            impl #name {
                #builders

                #[doc = " The wire pairs, in a stable order: sorted by parameter name, with a"]
                #[doc = " repeated key per element of a list. Stable because a request URL that"]
                #[doc = " moves between runs makes every snapshot test flap."]
                pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
                    let mut out: Vec<(&'static str, String)> = Vec::new();
                    #pairs
                    out
                }

                #[doc = " Append every set parameter to `req`, skipping the unset ones."]
                pub fn apply(&self, mut req: Request) -> Request {
                    for (k, v) in self.to_pairs() {
                        req = req.query(k, v);
                    }
                    req
                }
            }
        })
    }

    /// `query/mod.rs`: private per-group modules, one explicit `pub use` per struct.
    ///
    /// Explicit rather than a glob, for the reason the models emitter gives: a glob makes "what
    /// does this crate expose" unanswerable from the source, and makes a spec bump's new type
    /// invisible in review.
    fn query_mod(&self, layout: &BTreeMap<String, Vec<usize>>) -> Result<TokenStream> {
        let mut mods = TokenStream::new();
        let mut uses = TokenStream::new();
        for (stem, ops) in layout {
            let module = IrIdent::new(stem.as_str()).to_ident();
            mods.extend(quote! { mod #module; });
            for &i in ops {
                let op = &self.ir.operations[i];
                let Some(name) = &op.query_struct else {
                    bail!("{}: assigned to a query file with no query struct", op.op_id);
                };
                let ty = name.to_ident();
                let doc = format!(" Query parameters for `{}`.", op.op_id);
                uses.extend(quote! {
                    #[doc = #doc]
                    pub use self::#module::#ty;
                });
            }
        }
        Ok(quote! {
            #![doc = " One `<Operation>Query` struct per operation that takes query parameters."]
            #![doc = ""]
            #![doc = " Query parameters collapse into a struct rather than becoming function"]
            #![doc = " arguments: `repoSearch` alone takes seventeen, and a rule that switched"]
            #![doc = " between arguments and a struct depending on how many there are would make"]
            #![doc = " every signature a surprise. Each struct is `Default` plus `with_*`"]
            #![doc = " builders, and `Default` sends no parameters at all."]

            #mods

            #uses
        })
    }

    fn param_doc(&self, p: &Param) -> TokenStream {
        let mut ts = doc_lines(&p.doc);
        let mut notes: Vec<String> = Vec::new();
        if p.required {
            notes.push(" Required by the API: the request fails without it.".to_owned());
        }
        if let Some(d) = &p.default {
            notes.push(format!(" The server defaults this to `{d}` when it is absent."));
        }
        if let Some(values) = &p.enum_values {
            // Suggestions, not validation: the server may accept values this pinned spec does
            // not list, and a type that refused them would make the SDK less capable than curl.
            notes.push(format!(
                " Known values: {}. Not enforced — a newer server may accept others.",
                values.iter().map(|v| format!("`{v}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !notes.is_empty() && !p.doc.rustdoc.is_empty() {
            ts.extend(quote! { #[doc = ""] });
        }
        for note in notes {
            ts.extend(quote! { #[doc = #note] });
        }
        ts
    }

    /// The statement that pushes one `(key, value)` pair, given the *element* type.
    fn pair_push(&self, wire: &str, ty: &RustType) -> Result<TokenStream> {
        Ok(match ty {
            // `v` is a `&String` here; cloning beats `to_string`, which allocates the same and
            // reads as a conversion that is not happening.
            RustType::String => quote! { out.push((#wire, v.clone())); },
            RustType::Bool
            | RustType::I32
            | RustType::I64
            | RustType::U64
            | RustType::F64
            // `Timestamp`'s `Display` is RFC 3339, which is what the API parses.
            | RustType::Timestamp => quote! { out.push((#wire, v.to_string())); },
            ty => bail!("a {ty:?} query parameter has no wire form"),
        })
    }

    /// The type of a `<Op>Query` field, inside its `Option` or `Vec`.
    ///
    /// Owned, unlike [`Ctx::arg_ty`]: a struct the caller keeps and mutates through builders
    /// cannot hold a `&str` without infecting all 132 of them — and every method signature that
    /// mentions one — with a lifetime parameter, for no benefit at all on a struct that exists to
    /// be built and immediately passed.
    fn query_ty(&self, ty: &RustType) -> Result<TokenStream> {
        Ok(match ty {
            RustType::String => quote! { String },
            RustType::Vec(inner) => {
                let inner = self.query_ty(inner)?;
                quote! { Vec<#inner> }
            }
            _ => self.arg_ty(ty).ok_or_else(|| format!("{ty:?} cannot be a query parameter"))?,
        })
    }

    /// How a scalar arrives as a function argument: borrowed for strings, by value for the rest.
    fn arg_ty(&self, ty: &RustType) -> Option<TokenStream> {
        Some(match ty {
            RustType::Bool => quote! { bool },
            RustType::I32 => quote! { i32 },
            RustType::I64 => quote! { i64 },
            RustType::U64 => quote! { u64 },
            RustType::F64 => quote! { f64 },
            RustType::String => quote! { &str },
            RustType::Timestamp => quote! { gitea_core::types::Timestamp },
            // `Copy`, so by value — and deliberately not `Deref`, so a caller cannot pass an
            // `IssueId` where an `IssueIndex` belongs.
            RustType::Newtype(name) => {
                let ident = IrIdent::new(name.as_str()).to_ident();
                quote! { gitea_core::types::ids::#ident }
            }
            _ => return None,
        })
    }

    fn body_arg_ty(&self, ty: &RustType) -> Result<TokenStream> {
        Ok(match ty {
            RustType::String => quote! { &str },
            RustType::Json => quote! { &::serde_json::Value },
            // A reference, never a value: the caller keeps ownership, and `&T` is what makes the
            // signature non-generic — an `impl Serialize` here is the single change that would
            // make this crate's compile time superlinear.
            RustType::Model(_) => {
                let ty = self.ty(ty)?;
                quote! { &#ty }
            }
            ty => bail!("{ty:?} is not a request-body type"),
        })
    }

    /// A response type, spelled from the crate root so that no file needs an import list.
    fn ty(&self, ty: &RustType) -> Result<TokenStream> {
        Ok(match ty {
            RustType::Bool => quote! { bool },
            RustType::I32 => quote! { i32 },
            RustType::I64 => quote! { i64 },
            RustType::U64 => quote! { u64 },
            RustType::F64 => quote! { f64 },
            RustType::String => quote! { String },
            RustType::Timestamp => quote! { gitea_core::types::Timestamp },
            RustType::Json => quote! { ::serde_json::Value },
            RustType::Vec(inner) => {
                let inner = self.ty(inner)?;
                quote! { Vec<#inner> }
            }
            RustType::Map(inner) => {
                let inner = self.ty(inner)?;
                quote! { ::std::collections::BTreeMap<String, #inner> }
            }
            RustType::Model(wire) => {
                let Some(rust) = self.models.get(wire.as_str()) else {
                    bail!(
                        "refers to definition {wire:?}, which is not in `Ir::models`. Lowering \
                         resolved a $ref it did not register."
                    );
                };
                let ident = rust.to_ident();
                quote! { gitea_model::#ident }
            }
            RustType::OpenEnum(name) => {
                if !self.enums.contains(name.as_str()) {
                    bail!("refers to open enum {name:?}, which is not in `Ir::open_enums`.");
                }
                let ident = IrIdent::new(name.as_str()).to_ident();
                quote! { gitea_model::#ident }
            }
            // A curated ID newtype. The friction is the point: `IssueIndex` and `IssueId` are the
            // per-repo counter and the global row id, and the API is inconsistent about which a
            // route wants. Passing the wrong one silently operates on a different real issue.
            RustType::Newtype(name) => {
                let ident = IrIdent::new(name.as_str()).to_ident();
                quote! { gitea_core::types::ids::#ident }
            }
            // Response shapes with their own `Client` exit; reaching here means `exit` grew a
            // hole rather than that this type needs spelling.
            RustType::Unit | RustType::Text | RustType::Bytes | RustType::File => {
                bail!("{ty:?} has no spelled-out Rust type; it is handled by a `Client` exit")
            }
        })
    }
}

/// Which `Client` exit a method compiles down to.
enum Exit {
    /// `Client::empty` — the 144 operations that answer `204`.
    Empty,
    /// `crate::text` — `text/plain` and `text/html` bodies.
    Text,
    /// `Client::bytes` — `zip`, `octet-stream`, `gzip`, streamed.
    Bytes,
    /// `Client::value` — `application/ld+json`, whose shape the spec does not describe.
    Value,
    /// `Client::json` into a named type.
    Json(TokenStream),
    /// `Client::json` into a bare `Vec<T>`, for the unpaginated array responses.
    ///
    /// Separate from [`Exit::Json`] only because of `null`: Go marshals a nil slice as `null`,
    /// so an endpoint with nothing to report answers `null` rather than `[]`, and a plain
    /// `Vec<T>` rejects that with "invalid type: null, expected a sequence". `repo get-assignees`
    /// on a repository with no assignable users is exactly that case. This is the same defect
    /// `crate::de::null_as_empty_vec` fixes on a *field*, one level up at the response body.
    JsonList(TokenStream),
    /// `Client::items` plus `Client::page`.
    Items { item: TokenStream },
}

/// The group's struct name. PascalCase of the group, via [`ident_pascal`] — the emitter does no
/// casing of its own, so a rename in `overrides.toml` moves this too.
fn group_ty(g: &Group) -> Result<proc_macro2::Ident> {
    let ident = ident_pascal(g.name.as_str());
    if ident.is_raw() {
        bail!("group {:?} mangles to the keyword `{ident}`, which cannot name a struct", g.name);
    }
    Ok(ident.to_ident())
}

/// `<fn_name>_page`, the single-page half of a paginated pair.
fn page_fn_name(op: &Operation) -> Result<proc_macro2::Ident> {
    Ok(IrIdent::new(format!("{}_page", op.fn_name.as_str())).to_ident())
}

/// `with_<field>`, the builder for one query parameter.
fn builder_name(field: &IrIdent) -> Result<proc_macro2::Ident> {
    Ok(IrIdent::new(format!("with_{}", field.as_str())).to_ident())
}

/// `clippy::too_many_arguments` costs one `#[allow]` on exactly one method in the whole spec:
/// `repoCreateReleaseAttachment`, which takes three path parameters, a file, an alternative URL,
/// a query struct and a progress sink.
///
/// The lint is right in general and wrong here. Every argument is one the API itself demands, and
/// the alternatives are worse: collapsing them into a generated options struct would hide the
/// operation's shape and give the caller no compiler help about which fields matter, and dropping
/// the progress sink would mean a multi-gigabyte release upload could not draw a progress bar.
fn arity_allow(args: usize) -> TokenStream {
    // clippy's threshold counts the receiver, and so does `args` — `&self` is its first element.
    const CLIPPY_THRESHOLD: usize = 7;
    if args <= CLIPPY_THRESHOLD {
        return TokenStream::new();
    }
    quote! {
        #[allow(
            clippy::too_many_arguments,
            reason = "every argument is one this API operation requires; a struct would hide its shape"
        )]
    }
}

fn deprecated_attr(op: &Operation) -> TokenStream {
    match &op.deprecated {
        Some(note) => quote! { #[deprecated(note = #note)] },
        None => TokenStream::new(),
    }
}

/// Emits a [`Doc`] as `#[doc = "…"]`, one per wrapped line.
///
/// The rustdoc form, with `[` and `]` already escaped by `ir::doc`: rustdoc reads `[owner]` as an
/// intra-doc link, and this crate is published with `RUSTDOCFLAGS=-D warnings`. `#[doc = "…"]`
/// with a `quote`-escaped literal is also immune to a description containing `*/`.
fn doc_lines(doc: &Doc) -> TokenStream {
    let mut ts = TokenStream::new();
    for line in &doc.rustdoc {
        // `prettyplease` renders `#[doc = "x"]` as `///x`; the leading space is what makes it
        // read as prose. rustdoc strips it.
        let line = format!(" {line}");
        ts.extend(quote! { #[doc = #line] });
    }
    ts
}

/// What one generated file has to import. Collected while rendering, because an unused `use` is a
/// warning and CI builds with `-D warnings`.
#[derive(Default)]
struct Needs {
    /// Names from `gitea_core::http`.
    http: BTreeSet<&'static str>,
    result: bool,
}

impl Needs {
    /// `is_group_mod` adds `Client`, which the group struct's field needs whether or not the file
    /// also carries methods.
    fn imports(&self, is_group_mod: bool) -> TokenStream {
        let mut names: BTreeSet<&str> = self.http.iter().copied().collect();
        if is_group_mod {
            names.insert("Client");
        }
        let mut ts = TokenStream::new();
        if self.result {
            ts.extend(quote! { use gitea_core::Result; });
        }
        if !names.is_empty() {
            let idents: Vec<proc_macro2::Ident> = names
                .iter()
                .map(|n| proc_macro2::Ident::new(n, proc_macro2::Span::call_site()))
                .collect();
            // rustfmt unwraps a single-item brace list, and `cargo fmt --check` covers this tree.
            ts.extend(if idents.len() == 1 {
                let only = &idents[0];
                quote! { use gitea_core::http::#only; }
            } else {
                quote! { use gitea_core::http::{#(#idents),*}; }
            });
        }
        ts
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

    fn files() -> &'static Vec<GeneratedFile> {
        static FILES: OnceLock<Vec<GeneratedFile>> = OnceLock::new();
        FILES.get_or_init(|| emit(ir()).expect("the client emitter covers the vendored spec"))
    }

    /// Source of the file whose path ends with `suffix`.
    fn source(suffix: &str) -> &'static str {
        files()
            .iter()
            .find(|f| f.path.to_string_lossy().ends_with(suffix))
            .map(|f| f.contents.as_str())
            .unwrap_or_else(|| panic!("no emitted file ends with {suffix:?}"))
    }

    /// One method's source text, from the start of its signature line to its closing brace, for
    /// assertions about a single signature.
    fn method_src(suffix: &str, name: &str) -> String {
        let src = source(suffix);
        let at =
            src.find(&format!("fn {name}(")).unwrap_or_else(|| panic!("{suffix} has no fn {name}"));
        // Back up to the start of the line so that `pub async fn` is inside the slice; without
        // this every assertion about asyncness silently passes on a truncated signature.
        let start = src[..at].rfind('\n').map_or(0, |i| i + 1);
        let rest = &src[start..];
        let end = rest.find("\n    }").map_or(rest.len(), |i| i + 6);
        rest[..end].to_owned()
    }

    fn ops_files() -> impl Iterator<Item = &'static GeneratedFile> {
        files().iter().filter(|f| f.path.to_string_lossy().contains("/generated/ops/"))
    }

    /// **The coverage assertion.** Exactly one `pub async fn` per spec operation, across the whole
    /// emitted `ops/` tree.
    ///
    /// It works out this cleanly because of how the two method shapes fall: every operation gets
    /// one `async` method — a paginated one's is its `_page` half — and the streaming half is the
    /// only non-async method a *method* file contains. So the `pub async fn` count is the
    /// operation count, with no filtering and nothing to keep in step by hand.
    #[test]
    fn there_is_exactly_one_async_method_per_operation() {
        let async_fns: usize =
            ops_files().map(|f| f.contents.matches("pub async fn ").count()).sum();
        assert_eq!(async_fns, ir().operations.len(), "one async method per operation");
        assert_eq!(async_fns, 482, "the spec has 482 operations");
    }

    /// The streaming halves, the group constructors, and the `Api` accessors are the only other
    /// `pub fn`s in `ops/`. Counting them separately means an accidentally-emitted helper shows up
    /// as a failure here rather than quietly inflating the coverage count above.
    #[test]
    fn the_non_async_methods_are_exactly_the_streams_and_the_plumbing() {
        // `"pub fn "` is not a substring of `"pub async fn "`, so this counts only the non-async.
        let plain: usize = ops_files().map(|f| f.contents.matches("pub fn ").count()).sum();
        let paged_arrays = ir()
            .operations
            .iter()
            .filter(|op| {
                !matches!(op.pagination, Pagination::None)
                    && matches!(op.success.ty, RustType::Vec(_))
            })
            .count();
        let groups = ir().groups.len();
        assert_eq!(paged_arrays, 83, "paginated array-returning operations");
        // one `new` and one `Api` accessor per group, plus one stream per paginated array.
        assert_eq!(plain, paged_arrays + 2 * groups);
    }

    #[test]
    fn every_paginated_array_operation_gets_both_halves() {
        for op in &ir().operations {
            if matches!(op.pagination, Pagination::None)
                || !matches!(op.success.ty, RustType::Vec(_))
            {
                continue;
            }
            let src = files()
                .iter()
                .filter(|f| f.path.to_string_lossy().contains("/generated/ops/"))
                .map(|f| f.contents.as_str())
                .find(|c| c.contains(&format!("pub fn {}(", op.fn_name)))
                .unwrap_or_else(|| panic!("{}: no stream method", op.op_id));
            assert!(
                src.contains(&format!("pub async fn {}_page(", op.fn_name)),
                "{}: the `_page` half must live beside the stream",
                op.op_id
            );
        }
    }

    #[test]
    fn every_unpaginated_array_response_tolerates_a_null_body() {
        // Go marshals a nil slice as `null`, so an endpoint with nothing to report answers
        // `null` rather than `[]` — and `Vec<T>` rejects that with "invalid type: null,
        // expected a sequence". `repo get-assignees` on a repository with no assignable users
        // is the ordinary case.
        //
        // This is the response-body twin of `crate::de::null_as_empty_vec`, which handles the
        // same Go behaviour on a *field*. Both must exist; the field rule never sees a body.
        let unpaginated_arrays: Vec<&Operation> = ir()
            .operations
            .iter()
            .filter(|op| {
                matches!(op.pagination, Pagination::None)
                    && matches!(op.success.ty, RustType::Vec(_))
            })
            .collect();
        assert!(unpaginated_arrays.len() > 30, "expected ~37, got {}", unpaginated_arrays.len());

        for op in &unpaginated_arrays {
            let src = files()
                .iter()
                .filter(|f| f.path.to_string_lossy().contains("/generated/ops/"))
                .map(|f| f.contents.as_str())
                .find(|c| c.contains(&format!("pub async fn {}(", op.fn_name)))
                .unwrap_or_else(|| panic!("{}: no method", op.op_id));
            assert!(
                src.contains("json::<Option<Vec<"),
                "{}: an unpaginated array response must decode through `Option<Vec<_>>`, or a \
                 nil slice from Go fails the whole call",
                op.op_id
            );
        }

        // And it must not leak onto anything else: a paginated array goes through `items`,
        // which walks pages, and a named type is not an array at all.
        let src = method_src("ops/repo/pulls.rs", "get_pull_request");
        assert!(!src.contains("Option<Vec<"), "{src}");
    }

    /// The first of the two paths in the spec that break a naive renderer. `{index}.{diffType}`
    /// must become two encoded parameters with a literal `.` between them — not one encoded
    /// segment, and not `1%2Ediff`.
    #[test]
    fn repos_owner_repo_pulls_index_dot_difftype_renders_a_literal_dot() {
        let src = method_src("ops/repo/pulls.rs", "download_pull_diff_or_patch");
        assert!(src.contains(r#""/repos/{}/{}/pulls/{}.{}""#), "{src}");
        assert!(src.contains("encode::seg(owner)"), "{src}");
        // The index is an integer, so it interpolates bare; `diff_type` is a string and is
        // encoded as its own segment rather than being folded into the filename.
        assert!(src.contains("index, encode::seg(diff_type)"), "{src}");
    }

    /// The second dotted path. Both are named so a refactor cannot quietly fix one and break the
    /// other.
    #[test]
    fn repos_owner_repo_git_commits_sha_dot_difftype_renders_a_literal_dot() {
        let src = method_src("ops/repo/git.rs", "download_commit_diff_or_patch");
        assert!(src.contains(r#""/repos/{}/{}/git/commits/{}.{}""#), "{src}");
        assert!(src.contains("encode::seg(sha), encode::seg(diff_type)"), "{src}");
    }

    /// `gea raw repo get-contents o r src/main.rs` 404s if `filepath` is encoded as a segment,
    /// because it asks for a file literally named `src/main.rs` in the root.
    #[test]
    fn a_path_like_parameter_keeps_its_slashes() {
        let src = method_src("ops/repo/contents.rs", "get_contents");
        assert!(src.contains("encode::path_like(filepath)"), "{src}");
        assert!(!src.contains("encode::seg(filepath)"), "{src}");
    }

    /// The mirror-image bug: an owner literally named `a/b` must not be able to change which
    /// route matches, so every single-segment string parameter goes through `encode::seg`.
    #[test]
    fn single_segment_parameters_are_segment_encoded() {
        for op in &ir().operations {
            for p in &op.path_params {
                if p.ty != RustType::String || p.encoding != PathEncoding::Segment {
                    continue;
                }
                let want = format!("encode::seg({})", p.rust);
                let found = ops_files().any(|f| f.contents.contains(&want));
                assert!(found, "{}: {:?} is not segment-encoded anywhere", op.op_id, p.wire);
            }
        }
    }

    /// A leftover `{owner}` in a rendered path produces a 404 from a command line that looked
    /// perfectly correct — the single most baffling failure this emitter could ship. Every brace
    /// in every emitted path literal must be a bare `{}` placeholder.
    #[test]
    fn no_emitted_path_can_contain_an_unsubstituted_placeholder() {
        for f in ops_files() {
            for (n, line) in f.contents.lines().enumerate() {
                let mut rest = line;
                while let Some(i) = rest.find('{') {
                    rest = &rest[i + 1..];
                    assert!(
                        rest.starts_with('}') || rest.starts_with('{') || !in_a_str_literal(line),
                        "{}:{}: `{{` inside a string literal is an unsubstituted placeholder: {line}",
                        f.path.display(),
                        n + 1,
                    );
                }
            }
        }
    }

    /// Cheap heuristic for the assertion above: does the line contain a string literal at all.
    fn in_a_str_literal(line: &str) -> bool {
        line.contains('"')
    }

    #[test]
    fn a_default_query_struct_sends_nothing() {
        // Asserted on the emitted source because the struct itself lives in another crate: every
        // pair push is guarded, so `Default::default()` has nothing to push.
        let src = source("query/repo_l.rs");
        let start = src.find("pub struct RepoListPullRequestsQuery").unwrap();
        let body = &src[start..];
        let to_pairs = &body[body.find("pub fn to_pairs").unwrap()..];
        let end = to_pairs.find("pub fn apply").unwrap();
        let to_pairs = &to_pairs[..end];
        for line in to_pairs.lines().filter(|l| l.contains("out.push")) {
            assert!(
                line.trim_start().starts_with("out.push"),
                "an unguarded push would make Default send a parameter: {line}"
            );
        }
        assert!(to_pairs.contains("if let Some(v) = &self.base"), "{to_pairs}");
        // A list parameter is a `Vec`, so its guard is the loop itself.
        assert!(to_pairs.contains("for v in &self.labels"), "{to_pairs}");
    }

    /// `to_pairs` order is the IR's order, which lowering sorts by wire name. A request URL that
    /// moves between runs makes every snapshot test flap.
    #[test]
    fn to_pairs_order_follows_the_sorted_parameter_list() {
        let op = ir().operation("repoListPullRequests").unwrap();
        let src = source("query/repo_l.rs");
        let start = src.find("pub fn to_pairs").unwrap();
        let body = &src[start..];
        let mut at = 0usize;
        for p in &op.query_params {
            let needle = format!("\"{}\"", p.wire);
            let found = body[at..]
                .find(&needle)
                .unwrap_or_else(|| panic!("{} is out of order in to_pairs", p.wire));
            at += found + needle.len();
        }
        for w in op.query_params.windows(2) {
            assert!(w[0].wire < w[1].wire, "lowering must sort query parameters by wire name");
        }
    }

    #[test]
    fn a_paginated_wrapper_response_gets_one_ordinary_method() {
        // `GetTree` is paginated but answers `{"tree": [...], "page": 1}`, which `ItemStream`
        // cannot walk. A stream method for it would decode-fail on the first page.
        let src = source("ops/git/mod.rs");
        assert!(src.contains("pub async fn tree("), "{src}");
        assert!(!src.contains("pub async fn tree_page("), "{src}");
        assert!(src.contains("gitea_model::GitTreeResponse"), "{src}");
    }

    #[test]
    fn the_three_uploads_stream_through_multipart() {
        for (file, name) in [
            ("ops/repo/releases.rs", "create_release_attachment"),
            ("ops/issue/c.rs", "create_issue_attachment"),
            ("ops/issue/c.rs", "create_issue_comment_attachment"),
        ] {
            let src = method_src(file, name);
            assert!(src.contains("Body::Multipart(parts)"), "{name}: {src}");
            // The form field name is the spec's, not the caller's: Gitea matches on it.
            assert!(src.contains(r#"name: "attachment".to_owned()"#), "{name}: {src}");
            assert!(src.contains(".progress(progress)"), "{name}: {src}");
        }
    }

    #[test]
    fn binary_and_text_responses_use_their_own_exits() {
        let archive = method_src("ops/repo/misc.rs", "get_archive");
        assert!(archive.contains("-> Result<(Mime, ByteStream)>"), "{archive}");
        assert!(archive.contains("Accept::Any"), "{archive}");
        assert!(archive.contains("self.client.bytes("), "{archive}");

        // Declared untyped by the spec and typed as bytes by `overrides.toml [response_type]`.
        let logs = method_src("ops/job/mod.rs", "logs");
        assert!(logs.contains("-> Result<(Mime, ByteStream)>"), "{logs}");
        assert!(logs.contains("self.client.bytes("), "{logs}");

        let key = method_src("ops/misc/mod.rs", "get_signing_key");
        assert!(key.contains("-> Result<String>"), "{key}");
        assert!(key.contains("Accept::Text"), "{key}");
        // No Gitea operation produces `application/ld+json` (Forgejo's ActivityPub routes did),
        // so the `Value` exit has no vendored example to pin here.
    }

    #[test]
    fn a_deprecated_operation_says_so_in_the_type_system() {
        let src = method_src("ops/user/t.rs", "tracked_times");
        assert!(src.contains("pub async fn tracked_times("), "{src}");
        assert!(source("ops/user/t.rs").contains("#[deprecated("), "no deprecation attribute");
    }

    #[test]
    fn no_emitted_file_busts_the_review_cap() {
        // `emit_all` checks this too, but failing here names the client emitter instead of
        // leaving a reader to work out which of four produced the offender.
        for f in files() {
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
        // The reason `clippy.toml` bans `HashMap` in this crate: a generator whose output moves
        // between runs makes `codegen --check` flap and destroys the property that a generated
        // diff is the API changelog.
        assert_eq!(emit(ir()).unwrap(), emit(ir()).unwrap());
    }

    #[test]
    fn every_operation_lands_in_exactly_one_file() {
        let ir = ir();
        let layout = Ctx::new(ir).unwrap().ops_layout().unwrap();
        let mut seen = vec![0usize; ir.operations.len()];
        for leaves in layout.values() {
            for ops in leaves.values() {
                for &i in ops {
                    seen[i] += 1;
                }
            }
        }
        assert!(seen.iter().all(|&n| n == 1), "every operation exactly once");
    }

    #[test]
    fn repo_operations_keep_the_irs_own_buckets() {
        // A reader looking for `create_pull_request` should find it in `ops/repo/pulls.rs`, not
        // in whichever slice a line-count split happened to produce.
        assert!(source("ops/repo/pulls.rs").contains("pub async fn create_pull_request("));
    }

    #[test]
    fn generated_code_contains_no_generic_functions() {
        // The rule this whole emitter exists to keep. A `<T` or an `impl Trait` argument in a
        // generated signature is the change that makes 42k lines superlinear to compile.
        for f in files() {
            for line in
                f.contents.lines().filter(|l| l.contains("pub fn ") || l.contains("pub async fn "))
            {
                assert!(!line.contains("impl "), "{}: generic argument: {line}", f.path.display());
                let after = line.split_once("fn ").unwrap().1;
                let name = after.split(['(', '<']).next().unwrap();
                assert!(
                    after.starts_with(&format!("{name}(")),
                    "{}: type parameters on a generated fn: {line}",
                    f.path.display()
                );
            }
        }
    }
}

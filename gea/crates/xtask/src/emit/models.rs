//! The `models` emitter: [`Ir::models`] → `crates/gitea-model/src/generated/**`.
//!
//! ## One type per file
//!
//! 244 files of 20–120 lines rather than one 9,000-line module. The point is `git log
//! crates/gitea-model/src/generated/models/pull_request.rs`: when a spec bump adds a field to
//! `PullRequest`, the diff is three lines in a file whose whole history is about `PullRequest`.
//! In a single module the same change is three lines lost inside a file whose blame is useless
//! and whose diff review is a scroll.
//!
//! ## What this emitter is *not* allowed to decide
//!
//! Every typing decision was made in [`crate::ir::lower`]: which Rust type a field has, whether
//! it is `T` / `T + default` / `Option<T>` / `Option<Box<T>>`, whether it needs a serde rename,
//! and what its doc comment says. This module renders those decisions and nothing more. It does
//! no string casing, resolves no `$ref`, and never reads `overrides.toml` — if something it
//! needs is missing from the IR it fails with a message saying so, rather than reconstructing
//! the decision here where it could disagree with the other three emitters.
//!
//! ## The serde contract, and why each half of it matters
//!
//! Every struct derives `Default` and carries container-level `#[serde(default)]`, and **no
//! struct has `deny_unknown_fields`**. Those two facts are the whole forward-compatibility
//! story for models:
//!
//! - `#[serde(default)]` means `{}` deserializes, and a server that *stops* sending a field we
//!   marked `Required` degrades to a zero value instead of failing the command. Only 39 of 246
//!   definitions declare `required` at all, so the spec's `required` is advisory here.
//! - No `deny_unknown_fields` means a newer Gitea adding a field is invisible to us rather
//!   than fatal.
//!
//! On top of that, five deserializers exist purely to absorb Go's marshalling habits:
//! `opt_timestamp` for the zero time (see [`crate::ir::types`]), `crate::de::lenient_u64` for
//! `format: uint64`, and `crate::de::null_as_default` / `null_as_empty_vec` / `null_as_empty_map`
//! because `json.Marshal` renders a nil *pointer*, a nil slice and a nil map all as `null`.
//! `{"assignees": null}` is the *normal* wire form of an unassigned issue, and
//! `{"merge_commit_sha": null}` the normal wire form of an open pull request; a derived
//! `Vec<User>` rejects the first and a derived `String` the second. Gitea uses `*string`,
//! `*int64` and `*bool` for optional scalars throughout and the specification records none of
//! it, so the tolerant path goes on **every** plain field rather than on the ones we have
//! watched fail.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::quote;

use super::{GeneratedFile, render};
use crate::Result;
use crate::ir::doc::Doc;
use crate::ir::types::{Presence, RustType};
use crate::ir::{Field, Ir, Model, ModelKind, OpenEnum};

/// The tree this emitter owns, workspace-relative.
const ROOT: &str = "crates/gitea-model/src/generated";

/// Names this emitter imports into model files. A generated type spelled the same way would
/// shadow the import and produce an error pointing at a `use` line nobody wrote, so
/// [`Ctx::new`] rejects the collision by name instead.
const RESERVED_IMPORTS: [&str; 4] = ["BTreeMap", "Deserialize", "Serialize", "Timestamp"];

pub fn emit(ir: &Ir) -> Result<Vec<GeneratedFile>> {
    let ctx = Ctx::new(ir)?;
    let mut files = Vec::new();

    for m in &ir.models {
        // A bare `type Foo string` definition *is* its open enum; emitting a second copy here
        // would be two types with one name.
        if matches!(m.kind, ModelKind::OpenEnum(_)) {
            continue;
        }
        let tokens = ctx.model_file(m)?;
        files.push(ctx.file(format!("{ROOT}/models/{}.rs", m.module.as_str()), tokens)?);
    }

    files.push(ctx.file(format!("{ROOT}/models/mod.rs"), ctx.models_mod())?);
    files.push(GeneratedFile::new(format!("{ROOT}/enums.rs"), ctx.enums_file()?));
    files.push(ctx.file(format!("{ROOT}/mod.rs"), ctx.root_mod())?);

    Ok(files)
}

struct Ctx<'a> {
    ir: &'a Ir,
    /// Definition name → model, for resolving [`RustType::Model`].
    by_wire: BTreeMap<&'a str, &'a Model>,
    /// Enum name → open enum, for resolving [`RustType::OpenEnum`].
    enums: BTreeMap<&'a str, &'a OpenEnum>,
}

impl<'a> Ctx<'a> {
    fn new(ir: &'a Ir) -> Result<Self> {
        let by_wire: BTreeMap<&str, &Model> =
            ir.models.iter().map(|m| (m.wire.as_str(), m)).collect();
        let enums: BTreeMap<&str, &OpenEnum> =
            ir.open_enums.iter().map(|e| (e.name.as_str(), e)).collect();

        for name in by_wire.values().map(|m| m.rust.as_str()).chain(enums.keys().copied()) {
            if RESERVED_IMPORTS.contains(&name) {
                bail!(
                    "a generated type is named {name:?}, which collides with an import every \
                     model file carries. Rename it via `overrides.toml`, or the generated `use` \
                     line will silently shadow {name}."
                );
            }
        }

        Ok(Ctx { ir, by_wire, enums })
    }

    fn file(&self, path: String, tokens: TokenStream) -> Result<GeneratedFile> {
        let contents = render(tokens, &self.ir.spec_version, &self.ir.spec_sha256)?;
        Ok(GeneratedFile::new(path, contents))
    }

    // -------------------------------------------------------------------- one type, one file

    fn model_file(&self, m: &Model) -> Result<TokenStream> {
        let mut needs = Needs::default();
        let item = match &m.kind {
            ModelKind::Struct(fields) => self.struct_item(m, fields, &mut needs)?,
            ModelKind::Alias(ty) => {
                let name = m.rust.to_ident();
                let ty = self.ty(ty, m.rust.as_str(), &mut needs)?;
                let doc = doc_attrs(&m.doc);
                quote! {
                    #doc
                    pub type #name = #ty;
                }
            }
            // `type: object` with no properties (`ForgeLike`, `ForgeOutbox`). There is nothing
            // to type, and inventing a shape would be a lie that breaks on the first real
            // response.
            ModelKind::FreeForm => {
                let name = m.rust.to_ident();
                let doc = doc_attrs(&m.doc);
                quote! {
                    #doc
                    ///
                    /// The specification declares this as an object with no properties, so there
                    /// is nothing to type. The value is passed through verbatim.
                    pub type #name = ::serde_json::Value;
                }
            }
            ModelKind::OneOrMany(inner) => self.one_or_many_item(m, inner, &mut needs)?,
            ModelKind::OpenEnum(_) => bail!("open enums are emitted into enums.rs, not per-file"),
        };

        let module_doc = match &m.kind {
            // Not a definition in the spec, so saying it is would be a lie in the one place a
            // reader goes to find out where a generated type came from.
            ModelKind::OneOrMany(_) => {
                format!(
                    " `{}`, synthesized by the generator. See the type's documentation.",
                    m.wire
                )
            }
            _ => format!(" `{}`, as declared in the Gitea API specification.", m.wire),
        };
        let imports =
            needs.imports(matches!(m.kind, ModelKind::Struct(_) | ModelKind::OneOrMany(_)));
        Ok(quote! {
            #![doc = #module_doc]

            #imports

            #item
        })
    }

    /// A shape-dispatched `One(T) / Many(Vec<T>)` enum, from `overrides.toml [one_or_many]`.
    ///
    /// ## Why `Deserialize` is written out rather than `#[serde(untagged)]`
    ///
    /// `untagged` tries the variants in declaration order and takes the first that *parses*,
    /// discarding the errors from the ones that did not. That is safe only when the variants
    /// are genuinely disjoint — and here they are not, because every generated model carries
    /// container-level `#[serde(default)]`. A struct all of whose fields default **matches any
    /// input serde can hand it**, including an empty object. So `One` is not "the object arm";
    /// it is a catch-all that swallows whatever `Many` rejected and yields an all-default value.
    ///
    /// That is exactly how `gea workflow list` came to print nothing, with exit 0, for a
    /// repository that has `ci.yml`: the real directory array failed `Many` on four `null`
    /// fields, matched `One` instead, produced an entry with `name: ""` and `type: ""`, and the
    /// `type == "file"` filter dropped it. A wrong answer delivered as a success.
    ///
    /// Reordering the variants does not fix it, it only moves it: with `Many` first, `One` still
    /// absorbs every array that fails to decode. The defect is the fall-through itself, so the
    /// impl below dispatches on the JSON *shape* — a sequence is a list, a map is a single value
    /// — and **propagates** the inner error rather than trying the other arm. A decode failure
    /// inside `Many` is reported as a decode failure, naming the offending element and field.
    ///
    /// `SeqAccessDeserializer` / `MapAccessDeserializer` forward the already-opened access
    /// object to the inner `Deserialize`, so nothing is buffered and serde's own path
    /// information survives. `Serialize` stays derived with `untagged`, which has no such
    /// ambiguity: it writes the inner value and there is nothing to guess.
    ///
    /// `#[non_exhaustive]` like every other generated enum — these crates publish to crates.io,
    /// and a third shape appearing upstream must not be a breaking change. `Default` is written
    /// out rather than derived because `derive(Default)` on an enum needs a unit variant and
    /// neither of these is one; the empty list is the honest zero value.
    ///
    /// `One` is boxed. A `Vec` is three words and a response model is hundreds of bytes, so an
    /// unboxed `One` would make every value of this enum — including a directory listing, the
    /// common case — as large as one whole entry. `clippy::large_enum_variant` says so too, and
    /// the workspace builds with `-D warnings`.
    fn one_or_many_item(
        &self,
        m: &Model,
        inner: &RustType,
        needs: &mut Needs,
    ) -> Result<TokenStream> {
        let name = m.rust.to_ident();
        let doc = doc_attrs(&m.doc);
        // Named before rendering: the message a decode failure prints should say what the route
        // sends, and the synthesized wrapper's own name ("…OrList") is not that.
        let expecting = match inner.direct_model_ref() {
            Some(wire) => format!("a {wire} or a list of them"),
            None => "a single value or a list of them".to_owned(),
        };
        let inner = self.ty(inner, m.rust.as_str(), needs)?;
        let visitor = quote::format_ident!("{}Visitor", m.rust.as_str());
        Ok(quote! {
            #doc
            #[derive(Debug, Clone, PartialEq, Serialize)]
            #[serde(untagged)]
            #[non_exhaustive]
            pub enum #name {
                /// The single-value shape the specification declares. Boxed so that the list
                /// shape — the common one — does not carry the size of a whole entry.
                One(Box<#inner>),
                /// The list shape the route answers with for some requests.
                Many(Vec<#inner>),
            }

            impl #name {
                /// Every entry, whichever shape arrived. A single value becomes a one-element
                /// list, which is what a caller iterating the result wants either way.
                pub fn into_vec(self) -> Vec<#inner> {
                    match self {
                        #name::One(one) => vec![*one],
                        #name::Many(many) => many,
                    }
                }

                /// Every entry, borrowed.
                pub fn as_slice(&self) -> &[#inner] {
                    match self {
                        #name::One(one) => ::std::slice::from_ref(&**one),
                        #name::Many(many) => many,
                    }
                }

                /// The single value, when that is the shape that arrived. `None` for a list,
                /// even a list of one — the distinction is what the caller asked about.
                pub fn one(&self) -> Option<&#inner> {
                    match self {
                        #name::One(one) => Some(one),
                        #name::Many(_) => None,
                    }
                }
            }

            impl Default for #name {
                fn default() -> Self {
                    #name::Many(Vec::new())
                }
            }

            /// Dispatches on the JSON shape rather than trying the variants in turn.
            ///
            /// The distinction matters because every generated model defaults every field, so a
            /// variant holding one would match an empty object — and, under
            /// `#[serde(untagged)]`, would quietly absorb any input the other variant rejected.
            /// Here a sequence is a list and a map is a single value, and an error raised while
            /// decoding either is returned rather than turned into the other variant.
            struct #visitor;

            impl<'de> ::serde::de::Visitor<'de> for #visitor {
                type Value = #name;

                fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    f.write_str(#expecting)
                }

                /// The list shape. Any failure inside an element is the caller's answer: it
                /// names the element and the field, instead of becoming an empty single value.
                fn visit_seq<A>(self, seq: A) -> ::std::result::Result<Self::Value, A::Error>
                where
                    A: ::serde::de::SeqAccess<'de>,
                {
                    let many = Deserialize::deserialize(
                        ::serde::de::value::SeqAccessDeserializer::new(seq),
                    )?;
                    Ok(#name::Many(many))
                }

                /// The single-value shape the specification declares.
                fn visit_map<A>(self, map: A) -> ::std::result::Result<Self::Value, A::Error>
                where
                    A: ::serde::de::MapAccess<'de>,
                {
                    let one = Deserialize::deserialize(
                        ::serde::de::value::MapAccessDeserializer::new(map),
                    )?;
                    Ok(#name::One(Box::new(one)))
                }

                /// A JSON `null` body. Go marshals a nil slice that way, so it means the empty
                /// list here exactly as it does on a field.
                fn visit_unit<E>(self) -> ::std::result::Result<Self::Value, E>
                where
                    E: ::serde::de::Error,
                {
                    Ok(#name::Many(Vec::new()))
                }
            }

            impl<'de> Deserialize<'de> for #name {
                fn deserialize<D>(d: D) -> ::std::result::Result<Self, D::Error>
                where
                    D: ::serde::Deserializer<'de>,
                {
                    d.deserialize_any(#visitor)
                }
            }
        })
    }

    fn struct_item(&self, m: &Model, fields: &[Field], needs: &mut Needs) -> Result<TokenStream> {
        let name = m.rust.to_ident();
        let doc = doc_attrs(&m.doc);

        let mut rendered = Vec::with_capacity(fields.len());
        for f in fields {
            rendered.push(
                self.field(f, m.rust.as_str(), needs)
                    .map_err(|e| format!("{}.{}: {e}", m.wire, f.wire))?,
            );
        }

        Ok(quote! {
            #doc
            #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
            // `default` so `{}` deserializes and a server that drops a field degrades to a zero
            // value. Deliberately no `deny_unknown_fields`: a newer Gitea adding a field must
            // not turn every request into a decode error.
            #[serde(default)]
            pub struct #name {
                #(#rendered)*
            }
        })
    }

    fn field(&self, f: &Field, owner: &str, needs: &mut Needs) -> Result<TokenStream> {
        let ident = f.rust.to_ident();
        let inner = self.ty(&f.ty, owner, needs)?;
        let ty = match f.presence {
            Presence::Required | Presence::DefaultPlain => inner,
            Presence::Optional => quote! { Option<#inner> },
            // `Repository.parent`. Without the `Box`, rustc rejects the crate with "recursive
            // type has infinite size" and points at a struct nobody wrote.
            Presence::OptionalBoxed => quote! { Option<Box<#inner>> },
        };

        let mut args: Vec<TokenStream> = Vec::new();
        if f.needs_rename {
            let wire = &f.wire;
            args.push(quote! { rename = #wire });
        }
        if f.presence != Presence::Required {
            args.push(quote! { default });
        }
        if matches!(f.presence, Presence::Optional | Presence::OptionalBoxed) {
            args.push(quote! { skip_serializing_if = "Option::is_none" });
        }
        if let Some(with) = deserialize_with(&f.ty, f.presence)? {
            args.push(quote! { deserialize_with = #with });
        }
        let serde = (!args.is_empty()).then(|| quote! { #[serde(#(#args),*)] });

        let doc = doc_attrs(&f.doc);
        let deprecated = f.deprecated.then(|| {
            quote! {
                #[deprecated(
                    note = "marked deprecated by the Gitea API specification; \
                            it is still deserialized, but do not build on it"
                )]
            }
        });

        Ok(quote! {
            #doc
            #deprecated
            #serde
            pub #ident: #ty,
        })
    }

    /// Renders a [`RustType`]. `owner` is the type being emitted, so a self-reference does not
    /// import itself.
    fn ty(&self, ty: &RustType, owner: &str, needs: &mut Needs) -> Result<TokenStream> {
        Ok(match ty {
            RustType::Bool => quote! { bool },
            RustType::I32 => quote! { i32 },
            RustType::I64 => quote! { i64 },
            RustType::U64 => quote! { u64 },
            RustType::F64 => quote! { f64 },
            RustType::String => quote! { String },
            RustType::Timestamp => {
                needs.timestamp = true;
                quote! { Timestamp }
            }
            RustType::Vec(inner) => {
                let inner = self.ty(inner, owner, needs)?;
                quote! { Vec<#inner> }
            }
            RustType::Map(inner) => {
                needs.map = true;
                let inner = self.ty(inner, owner, needs)?;
                quote! { BTreeMap<String, #inner> }
            }
            RustType::Model(wire) => {
                let Some(m) = self.by_wire.get(wire.as_str()) else {
                    bail!(
                        "refers to definition {wire:?}, which is not in `Ir::models`. Lowering \
                         resolved a $ref it did not register."
                    );
                };
                let ident = m.rust.to_ident();
                if m.rust.as_str() != owner {
                    needs.generated.insert(m.rust.as_str().to_owned());
                }
                quote! { #ident }
            }
            RustType::OpenEnum(name) => {
                let Some(e) = self.enums.get(name.as_str()) else {
                    bail!(
                        "refers to open enum {name:?}, which is not in `Ir::open_enums`. The IR \
                         carries enum references as raw strings, so lowering and this emitter \
                         must agree on the spelling; teach `RustType::OpenEnum` to carry an \
                         `Ident` if they ever cannot."
                    );
                };
                let ident = e.name.to_ident();
                if e.name.as_str() != owner {
                    needs.generated.insert(e.name.as_str().to_owned());
                }
                quote! { #ident }
            }
            RustType::Newtype(name) => {
                let ident = ident(name)?;
                needs.ids.insert(name.clone());
                quote! { #ident }
            }
            RustType::Json => quote! { ::serde_json::Value },
            // These are response/request shapes, not model fields. Reaching one here means
            // lowering put an operation-level type on a definition.
            RustType::File | RustType::Bytes | RustType::Text | RustType::Unit => {
                bail!("{ty:?} is not a model field type")
            }
        })
    }

    // ------------------------------------------------------------------------- the open enums

    /// `enums.rs`: the 19 `open_enum!` invocations.
    ///
    /// **The one place this emitter writes source text instead of tokens**, and the reason is
    /// `prettyplease`: a macro invocation's body is an opaque token stream, so it gets
    /// word-wrapped at the margin rather than formatted. A 27-variant enum comes out as a
    /// paragraph of tokens with line breaks in the middle of `=>` arms, which is exactly the
    /// unreviewable output that committing generated code is supposed to avoid.
    ///
    /// The two properties the token path was chosen for are kept anyway:
    ///
    /// - **Escaping is not hand-rolled.** Every string goes through
    ///   [`proc_macro2::Literal::string`], which is what `quote!` itself uses, so a description
    ///   containing a quote or a backslash cannot break the file.
    /// - **The output is parsed before it is written.** `syn::parse_file` runs on the result, so
    ///   a malformed enum fails here with the source in hand rather than during `cargo build`.
    ///
    /// It is also stable under `cargo fmt`, which leaves macro-invocation bodies alone.
    fn enums_file(&self) -> Result<String> {
        let mut s = String::from(concat!(
            "//! Enums that tolerate values this build has never heard of.\n",
            "//!\n",
            "//! Each of these is a string that the specification — or `overrides.toml`, for the\n",
            "//! six Go named string types whose values the spec omits — lists known values for.\n",
            "//! They are *open*: an unlisted value deserializes into `Unknown` and re-serializes\n",
            "//! verbatim, so pointing `gea` at a newer Gitea cannot turn a listing into a\n",
            "//! decode error. See `crate::open_enum` for the four properties that buys.\n",
        ));

        for e in &self.ir.open_enums {
            s.push('\n');
            s.push_str(&self.open_enum(e).map_err(|err| format!("{}: {err}", e.name))?);
        }

        syn::parse_file(&s).map_err(|e| {
            format!("generated enums.rs is not valid Rust: {e}\n--- source ---\n{s}")
        })?;
        Ok(format!("{}{s}", super::banner(&self.ir.spec_version, &self.ir.spec_sha256)))
    }

    fn open_enum(&self, e: &OpenEnum) -> Result<String> {
        if e.variants.is_empty() {
            bail!(
                "has no known values, so there is no `default` variant to name. Add its values \
                 under `[enum_values]` in crates/xtask/src/overrides.toml (the list may be \
                 incomplete — an open enum accepts anything)."
            );
        }
        // The macro's own catch-all arm is `Unknown(String)`, so a listed value that mangles to
        // `Unknown` would declare the variant twice. Dropping the value is the right fix and
        // costs nothing: it still deserializes, into the arm it was going to collide with, and
        // still round-trips verbatim.
        if let Some(v) = e.variants.iter().find(|v| v.rust.as_str() == "Unknown") {
            bail!(
                "lists the value {:?}, which mangles to the variant `Unknown` — the same name \
                 `open_enum!` gives its catch-all arm. Remove it from `[enum_values]` in \
                 crates/xtask/src/overrides.toml; the open-enum arm already covers it and \
                 round-trips it verbatim.",
                v.wire
            );
        }

        let mut s = String::from("crate::open_enum! {\n");
        if e.doc.is_empty() {
            s.push_str(&doc_line(&format!(" The `{}` values this build knows.", e.origin), 4));
        } else {
            for line in &e.doc.rustdoc {
                s.push_str(&doc_line(&format!(" {line}"), 4));
            }
        }
        s.push_str(&doc_line("", 4));
        s.push_str(&doc_line(&format!(" Derived from `{}` in the specification.", e.origin), 4));
        s.push_str(&format!("    pub enum {} {{\n", e.name));
        for v in &e.variants {
            s.push_str(&format!(
                "        {} => {},\n",
                proc_macro2::Literal::string(&v.wire),
                v.rust
            ));
        }
        s.push_str("    }\n");
        s.push_str(&format!("    default = {};\n}}\n", e.variants[0].rust));
        Ok(s)
    }

    // ------------------------------------------------------------------------ the module tree

    /// `models/mod.rs`: one `pub mod` per type, in file order.
    fn models_mod(&self) -> TokenStream {
        let mods: Vec<TokenStream> = self
            .emitted()
            .map(|m| {
                let module = m.module.to_ident();
                let doc = format!(" `{}`.", m.wire);
                quote! {
                    #[doc = #doc]
                    pub mod #module;
                }
            })
            .collect();

        quote! {
            #![doc = " One module per type, so that `git log` on a model is about that model."]
            #![doc = ""]
            #![doc = " Everything here is re-exported from the crate root;"]
            #![doc = " `gitea_model::PullRequest` is the path to use."]

            #(#mods)*
        }
    }

    /// `generated/mod.rs`: the flat re-export surface.
    ///
    /// Explicit `pub use` lines rather than a glob, because a glob makes "which types does this
    /// crate expose" unanswerable from the source and makes a spec bump's new type invisible in
    /// review. One line per type is a diff that reads as an API changelog.
    fn root_mod(&self) -> TokenStream {
        let models: Vec<TokenStream> = self
            .emitted()
            .map(|m| {
                let module = m.module.to_ident();
                let ty = m.rust.to_ident();
                quote! { pub use self::models::#module::#ty; }
            })
            .collect();
        let enums: Vec<TokenStream> = self
            .ir
            .open_enums
            .iter()
            .map(|e| {
                let ty = e.name.to_ident();
                quote! { pub use self::enums::#ty; }
            })
            .collect();

        quote! {
            #![doc = " Generated types for the Gitea API."]
            #![doc = ""]
            #![doc = " Everything is re-exported flat, so `gitea_model::PullRequest` works and"]
            #![doc = " the module layout stays an implementation detail. The `pub use` lines are"]
            #![doc = " explicit rather than a glob so that a spec bump adding a type shows up as a"]
            #![doc = " line in the diff."]

            pub mod enums;
            pub mod models;

            #(#enums)*
            #(#models)*
        }
    }

    /// The models that get their own file — everything except the bare `type Foo string`
    /// definitions, which live in `enums.rs`.
    ///
    /// Sorted by **module name**, not by definition name. `Ir::models` is in definition order
    /// (`APIError` before `AccessToken`), but the emitted `pub mod` and `pub use` lines are
    /// spelled with the module name, and rustfmt sorts a contiguous run of those. Emitting them
    /// in definition order means `cargo fmt --all --check` fails in CI on a file the generator
    /// owns.
    fn emitted(&self) -> impl Iterator<Item = &'a Model> {
        let mut out: Vec<&Model> =
            self.ir.models.iter().filter(|m| !matches!(m.kind, ModelKind::OpenEnum(_))).collect();
        out.sort_by(|a, b| a.module.as_str().cmp(b.module.as_str()));
        out.into_iter()
    }
}

/// Which `deserialize_with` a field needs, if any.
///
/// Every entry here exists because Gitea is written in Go and Go's `encoding/json` has habits
/// that a strict deserializer rejects. The unifying one is `null`: Go marshals a nil pointer, a
/// nil slice and a nil map all as `null`, the specification records none of the three, and
/// `#[serde(default)]` covers an *absent* key rather than an explicit `null`. So every plain
/// (non-`Option`) field gets a `null`-tolerant path — not only the ones a server has been
/// caught sending `null` for, because the spec gives no signal which those are.
fn deserialize_with(ty: &RustType, presence: Presence) -> Result<Option<&'static str>> {
    match (ty, presence) {
        // Go's zero time, `"0001-01-01T00:00:00Z"`, is what Gitea sends for an unset
        // `merged_at`. Parsed literally it renders as "2025 years ago" in a table.
        (RustType::Timestamp, Presence::Optional | Presence::OptionalBoxed) => {
            Ok(Some("gitea_core::types::opt_timestamp"))
        }
        (RustType::Timestamp, Presence::Required | Presence::DefaultPlain) => {
            Ok(Some("crate::de::lenient_timestamp"))
        }
        // `format: uint64` is not valid Swagger 2.0; Go marshals large uint64 in ways that do
        // not always come back as a JSON number.
        (RustType::U64, Presence::Required | Presence::DefaultPlain) => {
            Ok(Some("crate::de::lenient_u64"))
        }
        (RustType::U64, Presence::Optional | Presence::OptionalBoxed) => bail!(
            "an `Option<u64>` field appeared. `crate::de::lenient_u64` deserializes a bare \
             `u64`; add an `Option` flavour to crates/gitea-model/src/de.rs and map it here."
        ),
        // A `Vec<Timestamp>` or `BTreeMap<_, Timestamp>` would silently skip the zero-time
        // handling, which is exactly the bug `opt_timestamp` exists to prevent.
        (RustType::Vec(inner) | RustType::Map(inner), _)
            if matches!(**inner, RustType::Timestamp | RustType::U64) =>
        {
            bail!(
                "a collection of {:?} appeared. Its elements would bypass the lenient \
                 deserializers; teach de.rs to handle the collection before generating it.",
                inner
            )
        }
        // Go marshals a `nil` slice as `null`, not `[]`, so `{"assignees": null}` is the
        // ordinary wire form of an unassigned issue — and a derived `Vec<User>` rejects it with
        // "invalid type: null, expected a sequence", failing the whole response over a field the
        // user never asked about. Every non-optional collection gets the tolerant path; an
        // `Option<Vec<_>>` (a request-body field) already turns `null` into `None` by itself, so
        // adding a deserializer there would only be a second way to spell the same thing.
        (RustType::Vec(_), Presence::Required | Presence::DefaultPlain) => {
            Ok(Some("crate::de::null_as_empty_vec"))
        }
        (RustType::Map(_), Presence::Required | Presence::DefaultPlain) => {
            Ok(Some("crate::de::null_as_empty_map"))
        }
        // The scalar case, and the widest one. Gitea spells an optional scalar `*string`,
        // `*int64` or `*bool` throughout, and a nil pointer marshals as `null` exactly as a nil
        // slice does. The specification cannot tell us which fields those are — it records
        // `"type": "string"` for a `*string` and for a `string` alike — so the tolerant path
        // goes on all of them rather than on the handful we have watched fail.
        //
        // The field that made this a `pr`-group outage rather than a curiosity is
        // `PullRequest.merge_commit_sha`, `null` on every *open* pull request.
        //
        // `Option<T>` is deliberately excluded: it already turns `null` into `None` by itself,
        // and a second spelling of the same thing would only be a place to disagree.
        (
            RustType::Bool
            | RustType::I32
            | RustType::I64
            | RustType::F64
            | RustType::String
            | RustType::Text
            | RustType::Newtype(_)
            | RustType::OpenEnum(_),
            Presence::Required | Presence::DefaultPlain,
        ) => Ok(Some("crate::de::null_as_default")),
        _ => Ok(None),
    }
}

/// Emits the rustdoc form of a [`Doc`], one `#[doc]` per wrapped line.
///
/// The rustdoc form, not the plain one: rustdoc reads `[owner]` as an intra-doc link, and with
/// `RUSTDOCFLAGS=-D warnings` in CI that is a failed build somewhere inside 9k generated lines.
/// `#[doc = "…"]` with a `quote`-escaped literal is also immune to a description containing
/// `*/` or a stray backslash, which a `///`-emitting string template is not.
fn doc_attrs(doc: &Doc) -> TokenStream {
    let mut ts = TokenStream::new();
    for line in &doc.rustdoc {
        // `prettyplease` renders `#[doc = "x"]` as `///x`. The leading space is what makes the
        // 7,600 lines of generated documentation read like documentation; rustdoc strips it.
        let line = format!(" {line}");
        ts.extend(quote! { #[doc = #line] });
    }
    ts
}

/// One `#[doc = "…"]` line of source text, indented, with the literal escaped by
/// `proc_macro2` rather than by hand.
fn doc_line(text: &str, indent: usize) -> String {
    format!("{:indent$}#[doc = {}]\n", "", proc_macro2::Literal::string(text))
}

/// A `proc_macro2::Ident` from a name the IR carries as a plain string (the ID newtypes).
fn ident(name: &str) -> Result<proc_macro2::Ident> {
    let ok = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        bail!("{name:?} is not a valid Rust identifier");
    }
    Ok(proc_macro2::Ident::new(name, proc_macro2::Span::call_site()))
}

/// What a single model file has to import. Collected while rendering, so no file carries an
/// unused `use` — which would be a warning, and with `-D warnings` a failed build.
#[derive(Default)]
struct Needs {
    /// Types from `crate::generated`, excluding the file's own.
    generated: BTreeSet<String>,
    /// ID newtypes from `gitea_core::types::ids`.
    ids: BTreeSet<String>,
    timestamp: bool,
    map: bool,
}

impl Needs {
    /// The imports for one file, **in rustfmt's canonical order**.
    ///
    /// The order is load-bearing rather than cosmetic: CI runs `cargo fmt --all --check`, which
    /// covers the generated tree, and rustfmt reorders a contiguous run of `use` statements
    /// alphabetically. Emitting them in any other order is a failed build.
    fn imports(&self, is_struct: bool) -> TokenStream {
        let mut ts = TokenStream::new();
        ts.extend(use_path(quote! { crate::generated }, &self.generated));
        if self.timestamp {
            ts.extend(quote! { use gitea_core::types::Timestamp; });
        }
        ts.extend(use_path(quote! { gitea_core::types::ids }, &self.ids));
        if is_struct {
            ts.extend(quote! { use serde::{Deserialize, Serialize}; });
        }
        if self.map {
            ts.extend(quote! { use std::collections::BTreeMap; });
        }
        ts
    }
}

/// One `use base::Name;` per name, alphabetically.
///
/// Deliberately not a brace list. `prettyplease` wraps at column 89 and rustfmt at the 100 this
/// workspace configures, so a brace list long enough to need wrapping gets wrapped *differently*
/// by each — and CI runs `cargo fmt --all --check` over the generated tree. One name per line is
/// short enough that the two tools cannot disagree, and it makes "this model grew a reference to
/// `Label`" a one-line diff.
fn use_path(base: TokenStream, names: &BTreeSet<String>) -> TokenStream {
    let mut ts = TokenStream::new();
    for name in names {
        let ident = proc_macro2::Ident::new(name, proc_macro2::Span::call_site());
        ts.extend(quote! { use #base::#ident; });
    }
    ts
}

/// Tests against the committed spec.
///
/// The synthetic tests below pin the rendering rules; these are the ones that would actually
/// have caught a mistake, because a generator bug is exactly what a small synthetic input hides.
#[cfg(test)]
mod vendored {
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
        FILES.get_or_init(|| emit(ir()).expect("emitting the vendored spec succeeds"))
    }

    fn file(path: &str) -> &'static GeneratedFile {
        files()
            .iter()
            .find(|f| f.path.ends_with(path))
            .unwrap_or_else(|| panic!("{path} was not emitted"))
    }

    #[test]
    fn every_model_gets_exactly_one_file() {
        let ir = ir();
        let structs =
            ir.models.iter().filter(|m| !matches!(m.kind, ModelKind::OpenEnum(_))).count();
        // + models/mod.rs, enums.rs, mod.rs
        assert_eq!(files().len(), structs + 3);

        // One type per file is the whole reviewability argument, so a duplicate path — two
        // definitions whose names mangle to the same module — must fail loudly rather than
        // silently letting one overwrite the other.
        let paths: BTreeSet<_> = files().iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths.len(), files().len());
    }

    #[test]
    fn no_emitted_file_busts_the_review_cap() {
        for f in files() {
            assert!(
                f.line_count() <= super::super::MAX_LINES,
                "{} is {} lines; the point of committing generated code is that its diff is \
                 reviewable",
                f.path.display(),
                f.line_count()
            );
        }
    }

    #[test]
    fn emission_is_deterministic() {
        // A `HashMap` iteration inside a generator is a phantom diff on somebody else's PR.
        // `clippy.toml` bans the type; this is the assertion behind the ban.
        assert_eq!(emit(ir()).unwrap(), emit(ir()).unwrap());
    }

    #[test]
    fn repository_parent_is_boxed() {
        // The single field in the whole 250-definition set that needs it. Without the SCC pass in
        // lowering plus this rendering, rustc rejects the crate with "recursive type has
        // infinite size", pointing at a struct nobody wrote.
        assert!(
            file("models/repository.rs").contents.contains("pub parent: Option<Box<Repository>>,"),
            "{}",
            file("models/repository.rs").contents
        );
    }

    #[test]
    fn every_optional_timestamp_routes_through_the_zero_time_deserializer() {
        // The highest-consequence attribute in this emitter. A single missed field renders as
        // "2025 years ago" in a table, and nothing about the type says it is wrong.
        let optional_timestamps: usize = ir()
            .models
            .iter()
            .filter_map(|m| match &m.kind {
                ModelKind::Struct(fields) => Some(fields),
                _ => None,
            })
            .flatten()
            .filter(|f| {
                f.ty == RustType::Timestamp
                    && matches!(f.presence, Presence::Optional | Presence::OptionalBoxed)
            })
            .count();
        let emitted: usize = files()
            .iter()
            .map(|f| f.contents.matches("gitea_core::types::opt_timestamp").count())
            .sum();
        assert_eq!(emitted, optional_timestamps);
        assert!(optional_timestamps > 90, "expected ~93, got {optional_timestamps}");
    }

    #[test]
    fn every_plain_collection_tolerates_a_null() {
        // Go marshals a nil slice as `null`, so `{"assignees": null}` is the ordinary shape of
        // an unassigned issue — and a derived `Vec<User>` fails the whole response on it. One
        // missed field costs the user a command, with an error that names a type they never
        // wrote.
        let plain_collections: usize = ir()
            .models
            .iter()
            .filter_map(|m| match &m.kind {
                ModelKind::Struct(fields) => Some(fields),
                _ => None,
            })
            .flatten()
            .filter(|f| {
                matches!(f.ty, RustType::Vec(_) | RustType::Map(_))
                    && matches!(f.presence, Presence::Required | Presence::DefaultPlain)
            })
            .count();
        let emitted: usize = files()
            .iter()
            .map(|f| {
                f.contents.matches("crate::de::null_as_empty_vec").count()
                    + f.contents.matches("crate::de::null_as_empty_map").count()
            })
            .sum();
        assert_eq!(emitted, plain_collections);
        assert!(plain_collections > 60, "expected ~72, got {plain_collections}");

        // An `Option<Vec<_>>` — a request-body field — turns `null` into `None` by itself, so
        // it must *not* also carry the deserializer.
        let src = file("models/edit_issue_option.rs").contents.clone();
        assert!(src.contains("pub assignees: Option<Vec<String>>,"), "{src}");
        assert!(!src.contains("null_as_empty_vec"), "{src}");
    }

    #[test]
    fn every_plain_scalar_tolerates_a_null() {
        // The widest of the `null` rules, and the one that took out an entire command group.
        //
        // Go marshals a nil pointer as `null`, Gitea spells optional scalars `*string` /
        // `*int64` / `*bool` throughout, and the specification records none of it — a `*string`
        // and a `string` are both `"type": "string"`. So there is no subset of fields that is
        // "the nullable ones"; either every plain scalar tolerates a `null` or the next one a
        // server decides to leave unset fails the whole response.
        //
        // `PullRequest.merge_commit_sha` is the one that proved it: `null` on every *open* pull
        // request, typed `String`, and therefore `pr list` / `view` / `diff` / `status` /
        // `checks` all exited 1 against any repository with an open pull request.
        let plain_scalars: usize = ir()
            .models
            .iter()
            .filter_map(|m| match &m.kind {
                ModelKind::Struct(fields) => Some(fields),
                _ => None,
            })
            .flatten()
            .filter(|f| {
                matches!(
                    f.ty,
                    RustType::Bool
                        | RustType::I32
                        | RustType::I64
                        | RustType::F64
                        | RustType::String
                        | RustType::Text
                        | RustType::Newtype(_)
                        | RustType::OpenEnum(_)
                ) && matches!(f.presence, Presence::Required | Presence::DefaultPlain)
            })
            .count();
        let emitted: usize =
            files().iter().map(|f| f.contents.matches("crate::de::null_as_default").count()).sum();
        assert_eq!(emitted, plain_scalars);
        assert!(plain_scalars > 600, "expected ~713, got {plain_scalars}");

        // The field the bug was found on, spelled out, so a presence-rule change that quietly
        // drops it fails here by name rather than against a live server.
        let pr = &file("models/pull_request.rs").contents;
        assert!(
            pr.contains("deserialize_with = \"crate::de::null_as_default\"")
                && pr.contains("pub merge_commit_sha: String,"),
            "{pr}"
        );

        // An `Option<T>` already maps `null` to `None`; a second spelling of the same thing
        // would only be somewhere for the two to disagree.
        let src = &file("models/edit_issue_option.rs").contents;
        assert!(src.contains("pub title: Option<String>,"), "{src}");
        assert!(!src.contains("null_as_default"), "{src}");
    }

    #[test]
    fn get_contents_gets_a_shape_dispatched_one_or_many_enum() {
        let src = file("models/contents_response_or_list.rs").contents.clone();
        // Serialization is untagged — that direction has nothing to guess. *Deserialization*
        // must not be: `#[serde(untagged)]` takes the first variant that parses, and because
        // every generated model defaults every field, `One` parses anything. That is how a real
        // directory array became an all-default entry with `name: ""` and `type: ""`, which
        // `gea workflow list` then filtered away — reporting no workflows, with exit 0, for a
        // repository that has one.
        assert!(src.contains("#[derive(Debug, Clone, PartialEq, Serialize)]"), "{src}");
        assert!(src.contains("#[serde(untagged)]"), "{src}");
        assert!(
            !src.contains("Serialize, Deserialize"),
            "Deserialize must be written out, not derived: an untagged derive lets `One` \
             swallow an array that failed `Many`.\n{src}"
        );
        assert!(src.contains("impl<'de> Deserialize<'de> for ContentsResponseOrList"), "{src}");
        // The dispatch itself: shape, not trial and error. `visit_seq` propagates the inner
        // error with `?` rather than falling through to the other variant.
        assert!(src.contains("fn visit_seq<A>"), "{src}");
        assert!(src.contains("fn visit_map<A>"), "{src}");
        assert!(src.contains("SeqAccessDeserializer::new(seq)"), "{src}");
        assert!(src.contains("MapAccessDeserializer::new(map)"), "{src}");
        // Published crate: a third shape upstream must not be a breaking change.
        assert!(src.contains("#[non_exhaustive]"), "{src}");
        // Boxed: `clippy::large_enum_variant`, and the list shape should not pay for the
        // single-entry shape's size.
        assert!(src.contains("One(Box<ContentsResponse>)"), "{src}");
        assert!(src.contains("Many(Vec<ContentsResponse>)"), "{src}");
        // `derive(Default)` needs a unit variant, so the impl is written out instead.
        assert!(src.contains("impl Default for ContentsResponseOrList"), "{src}");
    }

    #[test]
    fn the_two_uint64_fields_use_the_lenient_deserializer() {
        // `PullReviewComment.position` and `.original_position`. `format: uint64` is not valid
        // Swagger 2.0 at all; Go marshals these in ways a strict `u64` rejects.
        let c = &file("models/pull_review_comment.rs").contents;
        assert_eq!(c.matches("crate::de::lenient_u64").count(), 2, "{c}");
    }

    #[test]
    fn no_struct_forbids_unknown_fields() {
        // A newer Gitea adding a field must be invisible to us, not fatal. This is cheap to
        // assert and catastrophic to get wrong, since the symptom is a total outage against a
        // newer server rather than a test failure.
        for f in files() {
            assert!(!f.contents.contains("deny_unknown_fields"), "{}", f.path.display());
        }
    }

    #[test]
    fn every_type_is_reachable_from_the_crate_root() {
        // `gitea_model::PullRequest` is the documented path. A missing re-export makes a type
        // exist but be unusable, which no compile error catches.
        let root = &file("generated/mod.rs").contents;
        for m in ir().models.iter() {
            let line = match &m.kind {
                ModelKind::OpenEnum(_) => format!("pub use self::enums::{};", m.rust),
                _ => format!("pub use self::models::{}::{};", m.module, m.rust),
            };
            assert!(root.contains(&line), "missing: {line}");
        }
        for e in &ir().open_enums {
            assert!(root.contains(&format!("pub use self::enums::{};", e.name)));
        }
    }

    #[test]
    fn the_seventeen_open_enums_are_all_emitted() {
        let c = &file("generated/enums.rs").contents;
        assert_eq!(c.matches("crate::open_enum! {").count(), ir().open_enums.len());
        assert_eq!(ir().open_enums.len(), 17);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::names::Ident as IrIdent;
    use crate::ir::{EnumVariant, Group};

    fn doc(s: &str) -> Doc {
        Doc::from_text(Some(s))
    }

    fn field(wire: &str, rust: &str, ty: RustType, presence: Presence) -> Field {
        Field {
            wire: wire.to_owned(),
            rust: IrIdent::new(rust),
            needs_rename: rust != wire,
            ty,
            presence,
            doc: Doc::default(),
            deprecated: false,
            required_demoted: false,
        }
    }

    fn model(wire: &str, rust: &str, module: &str, kind: ModelKind) -> Model {
        Model {
            wire: wire.to_owned(),
            rust: IrIdent::new(rust),
            kind,
            doc: Doc::default(),
            module: IrIdent::new(module),
        }
    }

    fn ir(models: Vec<Model>, open_enums: Vec<OpenEnum>) -> Ir {
        Ir {
            spec_version: "1.27.2".to_owned(),
            spec_sha256: "0".repeat(64),
            models,
            open_enums,
            operations: Vec::new(),
            groups: Vec::<Group>::new(),
            cyclic_models: BTreeSet::new(),
        }
    }

    /// Renders one model's file, for readable assertions on the emitted source.
    fn render_one(ir: &Ir, wire: &str) -> String {
        let ctx = Ctx::new(ir).unwrap();
        let m = ir.models.iter().find(|m| m.wire == wire).unwrap();
        let tokens = ctx.model_file(m).unwrap();
        render(tokens, &ir.spec_version, &ir.spec_sha256).unwrap()
    }

    #[test]
    fn presence_drives_the_type_and_the_serde_attributes() {
        let ir = ir(
            vec![model(
                "Thing",
                "Thing",
                "thing",
                ModelKind::Struct(vec![
                    field("req", "req", RustType::String, Presence::Required),
                    field("plain", "plain", RustType::I64, Presence::DefaultPlain),
                    field("opt", "opt", RustType::Model("Thing".into()), Presence::Optional),
                ]),
            )],
            Vec::new(),
        );
        let out = render_one(&ir, "Thing");

        // Required: a bare `T` and no `default`. The container-level `#[serde(default)]` is what
        // keeps a server dropping the field from failing the request; the `deserialize_with` is
        // what keeps a server sending an explicit `null` from failing it.
        assert!(out.contains("pub req: String,"), "{out}");
        assert!(out.contains("#[serde(default)]\npub struct Thing"), "{out}");
        assert!(
            out.contains(
                "#[serde(deserialize_with = \"crate::de::null_as_default\")]\n    pub req"
            ),
            "{out}"
        );
        // DefaultPlain: `T` plus a field-level default, plus the same `null` tolerance.
        assert!(
            out.contains(
                "#[serde(default, deserialize_with = \"crate::de::null_as_default\")]\n    pub plain: i64,"
            ),
            "{out}"
        );
        // Optional: `Option<T>` that does not serialize when absent, so a round-tripped body
        // does not grow a pile of explicit nulls.
        assert!(
            out.contains("#[serde(default, skip_serializing_if = \"Option::is_none\")]"),
            "{out}"
        );
        assert!(out.contains("pub opt: Option<Thing>,"), "{out}");
    }

    #[test]
    fn a_cycle_is_boxed_and_does_not_import_itself() {
        // `Repository.parent`. Two failure modes in one test: without the `Box`, rustc rejects
        // the crate with "recursive type has infinite size"; with a self-import, the file has
        // `use crate::generated::Repository;` next to `pub struct Repository`, which is an error
        // pointing at a `use` line nobody wrote.
        let ir = ir(
            vec![model(
                "Repository",
                "Repository",
                "repository",
                ModelKind::Struct(vec![field(
                    "parent",
                    "parent",
                    RustType::Model("Repository".into()),
                    Presence::OptionalBoxed,
                )]),
            )],
            Vec::new(),
        );
        let out = render_one(&ir, "Repository");
        assert!(out.contains("pub parent: Option<Box<Repository>>,"), "{out}");
        assert!(!out.contains("use crate::generated"), "{out}");
    }

    #[test]
    fn timestamps_route_through_the_zero_time_deserializer() {
        // The single highest-value attribute this emitter produces: without it, an unmerged
        // pull request's `merged_at` renders as "2025 years ago".
        let ir = ir(
            vec![model(
                "Pr",
                "Pr",
                "pr",
                ModelKind::Struct(vec![field(
                    "merged_at",
                    "merged_at",
                    RustType::Timestamp,
                    Presence::Optional,
                )]),
            )],
            Vec::new(),
        );
        let out = render_one(&ir, "Pr");
        assert!(out.contains("deserialize_with = \"gitea_core::types::opt_timestamp\""), "{out}");
        assert!(out.contains("use gitea_core::types::Timestamp;"), "{out}");
    }

    #[test]
    fn uint64_routes_through_the_lenient_deserializer() {
        let ir = ir(
            vec![model(
                "C",
                "C",
                "c",
                ModelKind::Struct(vec![field(
                    "position",
                    "position",
                    RustType::U64,
                    Presence::DefaultPlain,
                )]),
            )],
            Vec::new(),
        );
        let out = render_one(&ir, "C");
        assert!(out.contains("deserialize_with = \"crate::de::lenient_u64\""), "{out}");
    }

    #[test]
    fn an_optional_uint64_fails_codegen_rather_than_generating_a_type_error() {
        // The lenient deserializer returns a bare `u64`. A spec bump producing `Option<u64>`
        // should stop here with a note about de.rs, not 40k lines later in rustc.
        let err = deserialize_with(&RustType::U64, Presence::Optional).unwrap_err().to_string();
        assert!(err.contains("de.rs"), "{err}");
    }

    #[test]
    fn renamed_and_keyword_fields_keep_their_wire_name() {
        // `MergePullRequestOption.Do` is both a rename *and* a Rust keyword. Getting either
        // half wrong sends `{"do": ...}` to a server that wants `{"Do": ...}`, which fails with
        // a validation error that mentions no field name.
        let ir = ir(
            vec![model(
                "MergePullRequestOption",
                "MergePullRequestOption",
                "merge_pull_request_option",
                ModelKind::Struct(vec![field("Do", "do", RustType::String, Presence::Required)]),
            )],
            Vec::new(),
        );
        let out = render_one(&ir, "MergePullRequestOption");
        assert!(out.contains("rename = \"Do\""), "{out}");
        assert!(out.contains("pub r#do: String,"), "{out}");
    }

    #[test]
    fn doc_comments_use_the_bracket_escaped_form() {
        // rustdoc reads `[owner]` as an intra-doc link; with `RUSTDOCFLAGS=-D warnings` that
        // fails the docs build for a published crate.
        let mut m = model("T", "T", "t", ModelKind::Struct(vec![]));
        m.doc = doc("replaces [owner] in the path");
        let out = render_one(&ir(vec![m], Vec::new()), "T");
        assert!(out.contains(r"/// replaces \[owner\] in the path"), "{out}");
    }

    #[test]
    fn a_deprecated_field_says_so_in_the_type_system() {
        let mut f = field("old", "old", RustType::String, Presence::DefaultPlain);
        f.deprecated = true;
        let ir = ir(vec![model("T", "T", "t", ModelKind::Struct(vec![f]))], Vec::new());
        let out = render_one(&ir, "T");
        assert!(out.contains("#[deprecated("), "{out}");
    }

    #[test]
    fn aliases_and_free_form_objects_become_type_aliases() {
        let ir = ir(
            vec![
                model(
                    "CreateHookOptionConfig",
                    "CreateHookOptionConfig",
                    "create_hook_option_config",
                    ModelKind::Alias(RustType::Map(Box::new(RustType::String))),
                ),
                model("ForgeLike", "ForgeLike", "forge_like", ModelKind::FreeForm),
            ],
            Vec::new(),
        );
        let out = render_one(&ir, "CreateHookOptionConfig");
        assert!(
            out.contains("pub type CreateHookOptionConfig = BTreeMap<String, String>;"),
            "{out}"
        );
        assert!(out.contains("use std::collections::BTreeMap;"), "{out}");
        // An alias needs no serde import, and an unused one is a warning.
        assert!(!out.contains("use serde::"), "{out}");

        let out = render_one(&ir, "ForgeLike");
        assert!(out.contains("pub type ForgeLike = ::serde_json::Value;"), "{out}");
    }

    #[test]
    fn an_enum_value_colliding_with_the_catch_all_arm_fails_codegen() {
        // `ReviewStateType` lists `UNKNOWN`, which mangles to `Unknown` — the name
        // `open_enum!` gives its catch-all. Emitting both declares the variant twice, and the
        // rustc error points inside a macro expansion.
        let e = OpenEnum {
            name: IrIdent::new("ReviewStateType"),
            doc: Doc::default(),
            variants: vec![
                EnumVariant { wire: "APPROVED".into(), rust: IrIdent::new("Approved") },
                EnumVariant { wire: "UNKNOWN".into(), rust: IrIdent::new("Unknown") },
            ],
            curated: true,
            origin: "ReviewStateType".to_owned(),
        };
        let ir = ir(Vec::new(), vec![e]);
        let err = Ctx::new(&ir).unwrap().enums_file().unwrap_err().to_string();
        assert!(err.contains("catch-all"), "{err}");
        assert!(err.contains("overrides.toml"), "{err}");
    }

    #[test]
    fn an_enum_with_no_known_values_fails_codegen() {
        let e = OpenEnum {
            name: IrIdent::new("Mystery"),
            doc: Doc::default(),
            variants: Vec::new(),
            curated: true,
            origin: "Mystery".to_owned(),
        };
        let ir = ir(Vec::new(), vec![e]);
        let err = Ctx::new(&ir).unwrap().enums_file().unwrap_err().to_string();
        assert!(err.contains("enum_values"), "{err}");
    }

    #[test]
    fn the_first_listed_value_is_the_default() {
        let e = OpenEnum {
            name: IrIdent::new("StateType"),
            doc: doc("State of an issue"),
            variants: vec![
                EnumVariant { wire: "open".into(), rust: IrIdent::new("Open") },
                EnumVariant { wire: "closed".into(), rust: IrIdent::new("Closed") },
            ],
            curated: true,
            origin: "StateType".to_owned(),
        };
        let ir = ir(Vec::new(), vec![e]);
        let out = Ctx::new(&ir).unwrap().enums_file().unwrap();
        assert!(out.contains("default = Open;"), "{out}");
        assert!(out.contains("\"open\" => Open,"), "{out}");
        // One variant per line: a macro body is opaque to prettyplease, so the emitter formats
        // it. A 27-variant enum word-wrapped mid-arm is not reviewable.
        assert!(out.contains("        \"closed\" => Closed,\n"), "{out}");
    }

    #[test]
    fn emission_is_a_pure_function_of_the_ir() {
        // Nondeterminism in a generator shows up as a phantom diff on an unrelated PR, which is
        // why `clippy.toml` bans `HashMap` in this crate. This is the assertion behind that ban.
        let build = || {
            ir(
                vec![
                    model(
                        "B",
                        "B",
                        "b",
                        ModelKind::Struct(vec![
                            field("z", "z", RustType::String, Presence::DefaultPlain),
                            field("a", "a", RustType::Model("A".into()), Presence::Optional),
                        ]),
                    ),
                    model("A", "A", "a", ModelKind::Struct(vec![])),
                ],
                Vec::new(),
            )
        };
        assert_eq!(emit(&build()).unwrap(), emit(&build()).unwrap());
    }

    #[test]
    fn imports_are_one_per_line_and_alphabetical() {
        // Not a brace list: prettyplease wraps at column 89 and rustfmt at the 100 this
        // workspace configures, so a long brace list is wrapped differently by each and
        // `cargo fmt --all --check` fails on a generated file.
        let names: BTreeSet<String> = ["User".to_owned(), "Label".to_owned()].into_iter().collect();
        assert_eq!(
            use_path(quote! { crate::generated }, &names).to_string(),
            quote! {
                use crate::generated::Label;
                use crate::generated::User;
            }
            .to_string()
        );
    }
}

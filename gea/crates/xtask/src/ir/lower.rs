//! Lowering: spec → [`Ir`]. Every decision in the generator is made here.
//!
//! The passes, in order, because several of them depend on the previous one's output:
//!
//! 1. **Classify definitions.** A `$ref` cannot be resolved to a Rust type until we know
//!    whether its target is a struct, a free-form object, a named string type, or a bare alias.
//! 2. **Build the *size* graph and find its strongly connected components.** See
//!    [`Lowerer::find_cycles`] — this is what stops the models emitter from producing
//!    "recursive type has infinite size".
//! 3. **Find the request-body definitions.** See [`Lowerer::find_request_models`]. A model
//!    serialized into a request body needs the opposite `Presence` policy from one
//!    deserialized out of a response, and getting this backwards makes every PATCH a full
//!    overwrite.
//! 4. **Lower models**, assigning [`crate::ir::types::Presence`] using the sets from passes 2 and 3.
//! 5. **Lower operations**, which is where names, paths, parameters and bodies are decided.
//! 6. **Assert the uniqueness invariants.** 482 unique `(module, fn)` and `(group, command)`.
//!
//! Passes 2 and 3 have to come before pass 4 and pass 6 has to come last; the rest is just
//! reading order.

use std::collections::{BTreeMap, BTreeSet};

use crate::Result;
use crate::ir::doc::Doc;
use crate::ir::names::{self, Ident, ident_pascal, ident_snake, kebab, words};
use crate::ir::paths;
use crate::ir::types::{self, Mime, Role, RustType};
use crate::ir::{
    Body, CtxFill, EnumVariant, Field, FlatField, Group, Ir, Model, ModelKind, OpenEnum, Operation,
    Pagination, Param, PathEncoding, Success,
};
use crate::overrides::Overrides;
use crate::spec::Loaded;
use crate::swagger::{ParamIn, Parameter, Schema, Spec};

/// What kind of Rust item a definition becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefClass {
    /// `type: object` with properties. The 229 real models.
    Struct,
    /// `type: object` with neither properties nor `additionalProperties`.
    FreeForm,
    /// A bare `type: string` — Gitea's Go `type StateType string`. The spec carries no
    /// variants, so they come from `overrides.toml [enum_values]`.
    StringEnum,
    /// A bare scalar, array, map, or `$ref` to another definition. Emitted as `pub type`, and
    /// **resolved through** when a field refers to it: a field typed `Duration` is more useful
    /// as an `i64` than as a one-line alias nobody remembers the meaning of.
    Alias,
}

pub fn lower(loaded: &Loaded, overrides: &Overrides) -> Result<Ir> {
    let mut l = Lowerer::new(&loaded.spec, overrides);
    l.classify();
    l.resolve_aliases()?;
    let cycles = l.find_cycles();
    l.cyclic = cycles;
    l.request_models = l.find_request_models();

    let models = l.lower_models()?;
    let operations = l.lower_operations()?;
    let groups = l.groups(&operations);

    let mut all_models = models;
    all_models.extend(std::mem::take(&mut l.synth_models).into_values());
    all_models.sort_by(|a, b| a.wire.cmp(&b.wire));

    let mut open_enums: Vec<OpenEnum> = std::mem::take(&mut l.open_enums).into_values().collect();
    open_enums.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Ir {
        spec_version: loaded.lock.version.clone(),
        spec_sha256: loaded.lock.canonical_sha256.clone(),
        models: all_models,
        open_enums,
        operations,
        groups,
        cyclic_models: l.cyclic,
    })
}

struct Lowerer<'a> {
    spec: &'a Spec,
    ov: &'a Overrides,
    class: BTreeMap<&'a str, DefClass>,
    /// Resolved type for each `Alias` definition, memoized because alias chains exist
    /// (`CreatePullReviewCommentOptions` → `CreatePullReviewComment`).
    alias_ty: BTreeMap<String, RustType>,
    cyclic: BTreeSet<String>,
    /// Owner → field pairs whose `$ref` closes a cycle, so exactly those fields get boxed.
    cycle_fields: BTreeSet<(String, String)>,
    /// Definitions reachable from an `in: body` parameter. See [`Lowerer::find_request_models`]
    /// — this is the set whose fields get [`crate::ir::types::Presence::Optional`] instead of `DefaultPlain`.
    request_models: BTreeSet<String>,
    open_enums: BTreeMap<String, OpenEnum>,
    /// Structs synthesized for anonymous nested objects. One exists in Forgejo v16.0.4 (the spec this generator was first written against)
    /// (`QuotaUsedAttachment.contained_in`); giving it a name keeps the tight-typing promise
    /// instead of degrading the field to `serde_json::Value`.
    synth_models: BTreeMap<String, Model>,
}

impl<'a> Lowerer<'a> {
    fn new(spec: &'a Spec, ov: &'a Overrides) -> Self {
        Lowerer {
            spec,
            ov,
            class: BTreeMap::new(),
            alias_ty: BTreeMap::new(),
            cyclic: BTreeSet::new(),
            cycle_fields: BTreeSet::new(),
            request_models: BTreeSet::new(),
            open_enums: BTreeMap::new(),
            synth_models: BTreeMap::new(),
        }
    }

    // --------------------------------------------------------------------- pass 1: classify

    fn classify(&mut self) {
        for (name, def) in &self.spec.definitions {
            let class = if def.ref_.is_some() {
                DefClass::Alias
            } else if def.is_free_form_object() {
                DefClass::FreeForm
            } else if !def.properties.is_empty() {
                DefClass::Struct
            } else if def.ty.as_deref() == Some("string") {
                DefClass::StringEnum
            } else {
                DefClass::Alias
            };
            self.class.insert(name.as_str(), class);
        }
    }

    fn resolve_aliases(&mut self) -> Result<()> {
        let names: Vec<&str> =
            self.class.iter().filter(|(_, c)| **c == DefClass::Alias).map(|(n, _)| *n).collect();
        for name in names {
            let ty = self.alias_type(name, 0)?;
            self.alias_ty.insert(name.to_owned(), ty);
        }
        Ok(())
    }

    fn alias_type(&self, name: &str, depth: usize) -> Result<RustType> {
        if depth > 8 {
            bail!(
                "definition {name:?} is part of an alias chain more than 8 deep, or an alias \
                 cycle. Neither can be turned into a `pub type`; the spec needs inspecting."
            );
        }
        let Some(def) = self.spec.definitions.get(name) else {
            bail!("definition {name:?} does not exist");
        };
        if let Some(target) = def.ref_target() {
            return match self.class.get(target) {
                Some(DefClass::Alias) => self.alias_type(target, depth + 1),
                Some(DefClass::Struct) => Ok(RustType::Model(target.to_owned())),
                Some(DefClass::StringEnum) => Ok(RustType::OpenEnum(target.to_owned())),
                Some(DefClass::FreeForm) => Ok(RustType::Json),
                None => bail!("definition {name:?} refers to unknown definition {target:?}"),
            };
        }
        if def.is_array() {
            let inner = match &def.items {
                Some(items) => self.pure_type(items, depth + 1)?,
                None => RustType::Json,
            };
            return Ok(RustType::Vec(Box::new(inner)));
        }
        if let Some(ap) = &def.additional_properties {
            let inner = match ap.schema() {
                Some(s) => self.pure_type(s, depth + 1)?,
                None => RustType::Json,
            };
            return Ok(RustType::Map(Box::new(inner)));
        }
        Ok(types::scalar(def.ty.as_deref(), def.format.as_deref()))
    }

    /// A type resolution that registers nothing, for use in the alias and cycle passes where
    /// side effects would run before the model pass and duplicate work.
    fn pure_type(&self, s: &Schema, depth: usize) -> Result<RustType> {
        if let Some(target) = s.ref_target() {
            return match self.class.get(target) {
                Some(DefClass::Alias) => self.alias_type(target, depth + 1),
                Some(DefClass::Struct) => Ok(RustType::Model(target.to_owned())),
                Some(DefClass::StringEnum) => Ok(RustType::OpenEnum(target.to_owned())),
                Some(DefClass::FreeForm) => Ok(RustType::Json),
                None => bail!("unknown definition {target:?}"),
            };
        }
        if s.is_array() {
            let inner = match &s.items {
                Some(items) => self.pure_type(items, depth + 1)?,
                None => RustType::Json,
            };
            return Ok(RustType::Vec(Box::new(inner)));
        }
        if let Some(ap) = &s.additional_properties {
            let inner = match ap.schema() {
                Some(inner) => self.pure_type(inner, depth + 1)?,
                None => RustType::Json,
            };
            return Ok(RustType::Map(Box::new(inner)));
        }
        if s.ty.as_deref() == Some("object") {
            return Ok(RustType::Json);
        }
        Ok(types::scalar(s.ty.as_deref(), s.format.as_deref()))
    }

    /// Resolves a definition name to the type a *field* referring to it should have.
    fn def_type(&self, name: &str) -> Result<RustType> {
        match self.class.get(name) {
            Some(DefClass::Struct) => Ok(RustType::Model(name.to_owned())),
            Some(DefClass::StringEnum) => Ok(RustType::OpenEnum(name.to_owned())),
            Some(DefClass::FreeForm) => Ok(RustType::Json),
            Some(DefClass::Alias) => self
                .alias_ty
                .get(name)
                .cloned()
                .ok_or_else(|| format!("alias {name:?} was not resolved").into()),
            None => bail!(
                "a $ref points at #/definitions/{name}, which does not exist. The vendored \
                 spec is inconsistent; re-run update-spec."
            ),
        }
    }

    // -------------------------------------------------------------- pass 2: the size graph

    /// Finds definitions on a `$ref` cycle, and the exact fields that close each cycle.
    ///
    /// **Only direct `$ref` properties are edges.** A `Vec<T>` or `BTreeMap<_, T>` field is
    /// already an indirection, so it cannot make a type infinitely sized: `GPGKey.subkeys:
    /// Vec<GPGKey>` is a cycle in the *reference* graph but compiles fine, and boxing it would
    /// add a pointless allocation plus an unusable `Option<Box<Vec<_>>>`. Building the graph
    /// from size edges only is what makes `Repository.parent` the single field that needs a
    /// `Box` in the whole 246-definition set.
    ///
    /// Without this pass, rustc rejects the models crate with "recursive type has infinite
    /// size", pointing at a struct nobody wrote, somewhere inside 9,000 generated lines.
    fn find_cycles(&mut self) -> BTreeSet<String> {
        let mut graph: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (name, def) in &self.spec.definitions {
            let mut edges = BTreeSet::new();
            for prop in def.properties.values() {
                if let Some(target) = prop.ref_target()
                    && let Ok(ty) = self.def_type(target)
                    && let Some(m) = ty.direct_model_ref()
                {
                    edges.insert(m.to_owned());
                }
            }
            graph.insert(name.as_str(), edges);
        }

        let sccs = tarjan(&graph);
        let mut cyclic = BTreeSet::new();
        for comp in &sccs {
            let is_cycle =
                comp.len() > 1 || graph.get(comp[0].as_str()).is_some_and(|e| e.contains(&comp[0]));
            if !is_cycle {
                continue;
            }
            let members: BTreeSet<&str> = comp.iter().map(String::as_str).collect();
            for name in comp {
                cyclic.insert(name.clone());
                let Some(def) = self.spec.definitions.get(name) else {
                    continue;
                };
                for (field, prop) in &def.properties {
                    if let Some(target) = prop.ref_target()
                        && members.contains(target)
                    {
                        self.cycle_fields.insert((name.clone(), field.clone()));
                    }
                }
            }
        }
        cyclic
    }

    // ------------------------------------------------- pass 3: which models are request bodies

    /// Every definition reachable from an `in: body` parameter, transitively through `$ref`.
    ///
    /// ## Why this pass exists
    ///
    /// [`types::presence`] used to run one policy over all 222 definitions. That policy is
    /// right for a *response* — an absent `description` really is `""` — and catastrophic for a
    /// *request*: with `#[serde(default)]` and no `skip_serializing_if`, a field the caller
    /// never set serializes as its zero value, so `PATCH /issues/42 {"title":"new"}` goes out as
    /// `{"title":"new","body":"","ref":"","milestone":0,…}` and Gitea faithfully blanks
    /// everything the user did not mention. Nothing errors; the data is simply gone.
    ///
    /// ## The rule
    ///
    /// The seed set is the `$ref` in every `in: body` parameter schema — 124 of the 125 bodies
    /// in Forgejo v16.0.4 (`renderMarkdownRaw` takes an inline bare `string`), naming 83 distinct
    /// definitions, 123 of which are typed (`ForgeLike` is a free-form object with nothing to
    /// type). From there it is a transitive closure over `$ref` edges, because a nested object
    /// inside a request body is just as much a request body: `CreateFileOptions.author` is an
    /// `Identity`, and an `Identity` with a defaulted `""` email is the same bug one level down.
    ///
    /// Collections are edges here, unlike in [`Lowerer::find_cycles`]: `Vec<T>` cannot make a
    /// type infinitely sized, but the `T` inside it is still serialized into the request.
    ///
    /// ## Definitions used as both
    ///
    /// Five definitions are in both closures in Forgejo v16.0.4: the open enums `CommitStatusState` and
    /// `ReviewStateType` (which carry no fields, so the policy does not apply to them), and the
    /// structs `ExternalTracker`, `ExternalWiki` and `InternalTracker` — all three appear on
    /// `Repository` (a response) and on `EditRepoOption` (a request body).
    ///
    /// **Request wins.** The two failure modes are not symmetric: an over-optional response
    /// costs the reader an `.unwrap_or_default()`, while an under-optional request silently
    /// destroys a repository's issue-tracker configuration the first time someone runs
    /// `gea repo edit --description`. A membership test on this set, rather than a
    /// request-minus-response difference, is what encodes that precedence.
    fn find_request_models(&self) -> BTreeSet<String> {
        let mut stack: Vec<String> = Vec::new();
        for item in self.spec.paths.values() {
            let per_op = item.operations().into_iter().flat_map(|(_, op)| op.parameters.iter());
            for p in item.parameters.iter().chain(per_op) {
                if p.location != ParamIn::Body {
                    continue;
                }
                if let Some(schema) = &p.schema {
                    stack.extend(
                        schema.walk().into_iter().filter_map(Schema::ref_target).map(str::to_owned),
                    );
                }
            }
        }

        let mut seen = BTreeSet::new();
        while let Some(name) = stack.pop() {
            let Some(def) = self.spec.definitions.get(&name) else {
                // A dangling `$ref` is reported with a far better message by `def_type`.
                continue;
            };
            if !seen.insert(name) {
                continue;
            }
            stack.extend(def.walk().into_iter().filter_map(Schema::ref_target).map(str::to_owned));
        }
        seen
    }

    /// Which `Presence` policy a definition's fields follow.
    fn role(&self, owner: &str) -> Role {
        if self.request_models.contains(owner) { Role::RequestBody } else { Role::Response }
    }

    // ------------------------------------------------------------------- pass 3: the models

    fn lower_models(&mut self) -> Result<Vec<Model>> {
        let names: Vec<String> = self.spec.definitions.keys().cloned().collect();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            out.push(self.lower_model(&name)?);
        }
        Ok(out)
    }

    fn lower_model(&mut self, name: &str) -> Result<Model> {
        let def = &self.spec.definitions[name];
        let doc = Doc::from_parts(def.title.as_deref(), def.description.as_deref());
        let class = self.class[name];

        let kind = match class {
            DefClass::FreeForm => ModelKind::FreeForm,
            DefClass::Alias => ModelKind::Alias(self.def_type(name)?),
            DefClass::StringEnum => {
                let variants = self.ov.enum_values.get(name).cloned().unwrap_or_default();
                self.register_enum(name, name, &variants, true, doc.clone());
                ModelKind::OpenEnum(name.to_owned())
            }
            DefClass::Struct => {
                let required: BTreeSet<&str> = def.required.iter().map(String::as_str).collect();
                let mut fields = Vec::with_capacity(def.properties.len());
                for (wire, prop) in &def.properties {
                    fields.push(self.lower_field(
                        name,
                        wire,
                        prop,
                        required.contains(wire.as_str()),
                    )?);
                }
                ModelKind::Struct(fields)
            }
        };

        Ok(Model {
            wire: name.to_owned(),
            rust: ident_pascal(name),
            kind,
            doc,
            module: ident_snake(name),
        })
    }

    fn lower_field(
        &mut self,
        owner: &str,
        wire: &str,
        prop: &Schema,
        spec_required: bool,
    ) -> Result<Field> {
        let key = format!("{owner}.{wire}");
        let mut ty = self.schema_type(owner, wire, prop)?;

        if let Some(newtype) = self.ov.newtype.get(&key) {
            if !matches!(ty, RustType::I32 | RustType::I64) {
                bail!(
                    "overrides.toml [newtype] maps {key:?} to {newtype:?}, but that field is \
                     {ty:?}, not an integer. ID newtypes wrap an i64; check for a typo in the \
                     field name."
                );
            }
            ty = RustType::Newtype(newtype.clone());
        }

        let demoted = self.ov.demote_required.iter().any(|d| d == &key);
        let required = spec_required && !demoted;
        let on_cycle = self.cycle_fields.contains(&(owner.to_owned(), wire.to_owned()));

        let rust = ident_snake(wire);
        Ok(Field {
            needs_rename: rust.as_str() != wire,
            wire: wire.to_owned(),
            rust,
            presence: types::presence(&ty, required, on_cycle, self.role(owner)),
            ty,
            doc: Doc::from_parts(prop.title.as_deref(), prop.description.as_deref()),
            deprecated: prop.extra.contains_key("x-deprecated"),
            required_demoted: demoted,
        })
    }

    /// Resolves a property schema to a Rust type, registering any type it implies: an inline
    /// `enum` becomes a named open enum, an anonymous nested object becomes a named struct.
    fn schema_type(&mut self, owner: &str, field: &str, s: &Schema) -> Result<RustType> {
        if let Some(target) = s.ref_target() {
            return self.def_type(target);
        }

        if let Some(values) = &s.enum_values {
            let key = format!("{owner}.{field}");
            let name = self
                .ov
                .enum_name
                .get(&key)
                .cloned()
                .unwrap_or_else(|| format!("{}{}", ident_pascal(owner), ident_pascal(field)));
            let doc = Doc::from_text(s.description.as_deref());
            self.register_enum(&name, &key, values, false, doc);
            return Ok(RustType::OpenEnum(name));
        }

        if s.is_array() {
            let inner = match &s.items {
                Some(items) => self.schema_type(owner, field, items)?,
                None => RustType::Json,
            };
            return Ok(RustType::Vec(Box::new(inner)));
        }

        if let Some(ap) = &s.additional_properties {
            let inner = match ap.schema() {
                // An `additionalProperties: {}` (empty schema) means "any value".
                Some(inner) if inner.ty.is_some() || inner.ref_.is_some() => {
                    self.schema_type(owner, field, inner)?
                }
                _ => RustType::Json,
            };
            return Ok(RustType::Map(Box::new(inner)));
        }

        // An anonymous nested object. Naming it keeps the field typed rather than degrading it
        // to `serde_json::Value`; v1.27.2 has exactly one.
        if s.ty.as_deref() == Some("object") && !s.properties.is_empty() {
            let name = format!("{}{}", ident_pascal(owner), ident_pascal(field));
            // An anonymous object inside a request body is still a request body. Registering it
            // before its fields are lowered is what makes `self.role(&name)` answer correctly.
            if self.role(owner) == Role::RequestBody {
                self.request_models.insert(name.clone());
            }
            let required: BTreeSet<&str> = s.required.iter().map(String::as_str).collect();
            let mut fields = Vec::with_capacity(s.properties.len());
            for (wire, prop) in &s.properties {
                fields.push(self.lower_field(
                    &name,
                    wire,
                    prop,
                    required.contains(wire.as_str()),
                )?);
            }
            self.synth_models.insert(
                name.clone(),
                Model {
                    wire: name.clone(),
                    rust: ident_pascal(&name),
                    kind: ModelKind::Struct(fields),
                    doc: Doc::from_text(Some(&format!(
                        "The anonymous object in `{owner}.{field}`. Named by the generator; the \
                         spec declares it inline."
                    ))),
                    module: ident_snake(&name),
                },
            );
            return Ok(RustType::Model(name));
        }

        if s.ty.as_deref() == Some("object") {
            return Ok(RustType::Json);
        }

        Ok(types::scalar(s.ty.as_deref(), s.format.as_deref()))
    }

    fn register_enum(
        &mut self,
        name: &str,
        origin: &str,
        values: &[String],
        curated: bool,
        doc: Doc,
    ) {
        let variants =
            values.iter().map(|v| EnumVariant { wire: v.clone(), rust: ident_pascal(v) }).collect();
        // A shared name (e.g. `PermissionLevel` used by three models) legitimately registers
        // twice; the first registration wins and the second is identical by construction,
        // since it comes from the same override entry.
        self.open_enums.entry(name.to_owned()).or_insert(OpenEnum {
            name: ident_pascal(name),
            doc,
            variants,
            curated,
            origin: origin.to_owned(),
        });
    }

    // --------------------------------------------------------------- pass 4: the operations

    fn lower_operations(&mut self) -> Result<Vec<Operation>> {
        // (op_id, tags, method, path) collected first so that name checking can happen over
        // the whole set before any per-operation work reports a confusing secondary error.
        let mut named: Vec<(String, names::OpName)> = Vec::new();
        for (path, item) in &self.spec.paths {
            for (_method, op) in item.operations() {
                let n = names::op_name(&op.operation_id, &op.tags, self.ov)
                    .map_err(|e| format!("{e}\n  (operation {} on {path})", op.operation_id))?;
                named.push((op.operation_id.clone(), n));
            }
        }
        names::check_unique(&named)?;

        let lookup: BTreeMap<&str, &names::OpName> =
            named.iter().map(|(id, n)| (id.as_str(), n)).collect();

        let mut out = Vec::with_capacity(named.len());
        for (path, item) in &self.spec.paths {
            for (method, op) in item.operations() {
                let n = lookup[op.operation_id.as_str()];
                out.push(
                    self.lower_operation(path, method, op, n)
                        .map_err(|e| format!("{}: {e}", op.operation_id))?,
                );
            }
        }
        // Sorted by the names users type, so `--dump-ir` and every generated table read in
        // command order rather than in URL order.
        out.sort_by(|a, b| {
            (a.group.as_str(), a.command.as_str()).cmp(&(b.group.as_str(), b.command.as_str()))
        });
        Ok(out)
    }

    fn lower_operation(
        &mut self,
        raw_path: &str,
        method: crate::swagger::HttpMethod,
        op: &crate::swagger::Operation,
        n: &names::OpName,
    ) -> Result<Operation> {
        let path = paths::tokenize(raw_path)?;

        let declared: Vec<&Parameter> = self
            .spec
            .paths
            .get(raw_path)
            .map(|i| i.parameters.iter().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(&op.parameters)
            .collect();

        let mut path_params = Vec::new();
        let mut query_params = Vec::new();
        let mut form_data = Vec::new();
        let mut body_param: Option<&Parameter> = None;

        for p in &declared {
            match p.location {
                ParamIn::Path => path_params.push(self.lower_param(p)?),
                ParamIn::Query => query_params.push(self.lower_param(p)?),
                ParamIn::FormData => form_data.push(self.lower_param(p)?),
                ParamIn::Body => {
                    if body_param.is_some() {
                        bail!("declares more than one body parameter");
                    }
                    body_param = Some(p);
                }
                // Header parameters are the security schemes (`Sudo`, `X-GITEA-OTP`), which
                // the runtime injects once in `Client::send`. Exposing them per-operation
                // would mean 482 identical `--sudo` flags.
                ParamIn::Header => {}
            }
        }

        // Positional arguments follow the *path*, not the spec's parameter array. They happen
        // to agree for all 506 operations in Forgejo v16.0.4, and this check is what will tell us
        // loudly if a future spec stops agreeing — the alternative is arguments silently
        // swapping places in a published API.
        let declared_names: Vec<&str> = declared
            .iter()
            .filter(|p| p.location == ParamIn::Path)
            .map(|p| p.name.as_str())
            .collect();
        if declared_names != path.params.iter().map(String::as_str).collect::<Vec<_>>() {
            bail!(
                "path parameters disagree with the path template.\n  template: {:?}\n  \
                 declared: {declared_names:?}\n  \
                 Positional arguments follow the template, so a mismatch would silently \
                 reorder a published function's arguments.",
                path.params,
            );
        }
        query_params.sort_by(|a, b| a.wire.cmp(&b.wire));
        form_data.sort_by(|a, b| a.wire.cmp(&b.wire));

        let body = match body_param {
            Some(p) => Some(self.lower_body(&op.operation_id, p)?),
            None => None,
        };

        let consumes =
            op.consumes.first().or_else(|| self.spec.consumes.first()).map(|m| Mime::parse(m));
        let produces: Vec<Mime> = if op.produces.is_empty() {
            self.spec.produces.iter().map(|m| Mime::parse(m)).collect()
        } else {
            op.produces.iter().map(|m| Mime::parse(m)).collect()
        };

        let success = self.success(op, &produces)?;
        let pagination = pagination(&query_params, success.response_name.as_deref());

        let query_struct = (!query_params.is_empty())
            .then(|| Ident::new(format!("{}Query", ident_pascal(&op.operation_id))));

        Ok(Operation {
            op_id: op.operation_id.clone(),
            group: n.group.clone(),
            command: n.command.clone(),
            module: n.module.clone(),
            sub_bucket: sub_bucket(&n.group, raw_path),
            fn_name: n.fn_name.clone(),
            method,
            path,
            path_params,
            query_params,
            form_data,
            body,
            consumes,
            produces,
            success,
            pagination,
            scope: self.scope(op, method),
            doc: Doc::from_parts(op.summary.as_deref(), op.description.as_deref()),
            deprecated: op.deprecated.then(|| {
                "deprecated by the Gitea API; see the endpoint's documentation for the \
                 replacement"
                    .to_owned()
            }),
            query_struct,
        })
    }

    fn lower_param(&mut self, p: &Parameter) -> Result<Param> {
        let w = words(&p.name);
        let ty = if p.ty.as_deref() == Some("array") {
            let inner = match &p.items {
                Some(items) => types::scalar(items.ty.as_deref(), items.format.as_deref()),
                None => RustType::String,
            };
            RustType::Vec(Box::new(inner))
        } else {
            types::scalar(p.ty.as_deref(), p.format.as_deref())
        };

        let encoding = if p.location == ParamIn::Path && self.ov.is_path_like(&p.name) {
            PathEncoding::PathLike
        } else {
            PathEncoding::Segment
        };

        // Only `owner`, `repo` and an explicit `branch` are context-fillable. `ref` looks
        // fillable but is not: it also accepts a sha or a tag, and silently substituting the
        // current branch would produce a plausible wrong answer.
        let ctx_fill = match p.name.as_str() {
            "owner" => Some(CtxFill::Owner),
            "repo" => Some(CtxFill::Repo),
            "branch" => Some(CtxFill::Branch),
            _ => None,
        };

        let enum_values =
            p.enum_values.clone().or_else(|| p.items.as_ref().and_then(|i| i.enum_values.clone()));

        Ok(Param {
            rust: ident_snake(&p.name),
            flag: kebab(&w),
            wire: p.name.clone(),
            ty,
            required: p.required,
            encoding,
            ctx_fill,
            enum_values,
            // `collectionFormat: multi` means repeat the key. This is why `Request::query` is
            // an ordered `Vec` rather than a map.
            repeated: p.collection_format.as_deref() == Some("multi"),
            default: p.default.as_ref().map(render_default),
            doc: Doc::from_text(p.description.as_deref()),
        })
    }

    /// Lowers a body parameter, flattening its fields to depth 1 for layer-2 flags.
    ///
    /// 124 of 125 bodies are `$ref`s to named definitions with typed properties, which is what
    /// makes per-field typed flags possible at all. The one inline body (`renderMarkdownRaw`,
    /// a bare string) falls out as a body with no flat fields.
    fn lower_body(&mut self, op_id: &str, p: &Parameter) -> Result<Body> {
        let Some(schema) = &p.schema else {
            bail!("body parameter {:?} has no schema", p.name);
        };
        let ty = self.schema_type(op_id, &p.name, schema)?;

        let mut flat = Vec::new();
        let mut deep = Vec::new();
        if let RustType::Model(model) = &ty
            && let Some(def) = self.spec.definitions.get(model)
        {
            let required: BTreeSet<&str> = def.required.iter().map(String::as_str).collect();
            let model = model.clone();
            for (wire, prop) in &def.properties {
                let field_ty = self.schema_type(&model, wire, prop)?;
                if flattenable(&field_ty) {
                    flat.push(FlatField {
                        flag: kebab(&words(wire)),
                        wire: wire.clone(),
                        enum_values: prop.enum_values.clone(),
                        required: required.contains(wire.as_str()),
                        ty: field_ty,
                        doc: Doc::from_parts(prop.title.as_deref(), prop.description.as_deref()),
                    });
                } else {
                    // Excluded rather than silently dropped: `--help` names these and points
                    // at `--body-file`, because a flag that quietly does nothing is worse than
                    // an admitted limitation.
                    deep.push(wire.clone());
                }
            }
        }

        Ok(Body { ty, required: p.required, flat, deep })
    }

    fn success(&mut self, op: &crate::swagger::Operation, produces: &[Mime]) -> Result<Success> {
        let mut codes: Vec<u16> = op
            .responses
            .keys()
            .filter_map(|k| k.parse::<u16>().ok())
            .filter(|c| (200..300).contains(c))
            .collect();
        codes.sort_unstable();

        // Gitea declares a handful of downloads as *only* a redirect — `downloadArtifact`
        // answers `302` to a signed blob URL and declares no 2xx at all. The HTTP client
        // follows redirects, so what the caller actually receives is the target's `200` and
        // its bytes. Modelling that is more honest than refusing to generate the operation.
        if codes.is_empty()
            && op.responses.keys().any(|k| matches!(k.as_str(), "301" | "302" | "303" | "307"))
        {
            return Ok(Success { status: 200, ty: RustType::Bytes, response_name: None });
        }

        let Some(&status) = codes.first() else {
            bail!(
                "declares no 2xx response, so there is no success type to generate. \
                 Declared: {:?}",
                op.responses.keys().collect::<Vec<_>>()
            );
        };

        let resp = &op.responses[&status.to_string()];
        let response_name = resp.ref_name().map(str::to_owned);
        let schema = match &response_name {
            Some(name) => self
                .spec
                .responses
                .get(name)
                .ok_or_else(|| format!("refers to unknown shared response {name:?}"))?
                .schema
                .as_ref(),
            None => resp.schema.as_ref(),
        };

        // `produces` wins over the declared schema for non-JSON endpoints: a zip archive is
        // declared as `type: string` (Go's `[]byte`), and decoding 200 MB of zip into a
        // `String` is not what anyone wants.
        //
        // The order of these arms matters. `application/json` is checked *before* the text
        // types because the document-level default is `["application/json", "text/html"]`, so
        // the 6 operations that declare no `produces` of their own inherit `text/html` — and
        // reading `WikiPage` or `WatchInfo` as an opaque `String` would have been a silent,
        // type-level lie about six endpoints.
        let ty = if produces.iter().any(Mime::is_binary) {
            RustType::Bytes
        } else if schema.is_none() {
            RustType::Unit
        } else if produces.contains(&Mime::Json) {
            let name = response_name.as_deref().unwrap_or(&op.operation_id);
            self.schema_type(name, "response", schema.expect("checked above"))?
        } else if produces.iter().any(|m| matches!(m, Mime::TextPlain | Mime::TextHtml)) {
            RustType::Text
        } else if produces.contains(&Mime::LdJson) {
            RustType::Json
        } else {
            let name = response_name.as_deref().unwrap_or(&op.operation_id);
            self.schema_type(name, "response", schema.expect("checked above"))?
        };

        let ty = match self.ov.response_type.get(&op.operation_id).map(String::as_str) {
            None => ty,
            // Only bytes: a text response needs an `Accept` derived from `produces`, and these
            // routes' `produces` is the (wrong) JSON default. A log is read fine as bytes.
            Some("bytes") => RustType::Bytes,
            // The inverse lie: a schema declared for a route whose handler answers `204`.
            Some("empty") => RustType::Unit,
            // And the lie in the other direction: no schema, and a JSON body anyway.
            Some("json") => RustType::Json,
            Some(other) => bail!(
                "overrides.toml [response_type] gives {:?} the type {other:?}; expected \
                 \"bytes\", \"empty\" or \"json\"",
                op.operation_id
            ),
        };

        let ty = match self.ov.one_or_many.get(&op.operation_id) {
            Some(name) => self.one_or_many(name, ty, &op.operation_id)?,
            None => ty,
        };

        Ok(Success { status, ty, response_name })
    }

    /// Replaces a declared response type with a synthesized untagged `One(T) / Many(Vec<T>)`
    /// enum, per `overrides.toml [one_or_many]`.
    ///
    /// The declared type must be a single model: if the spec already says `Vec<T>` there is
    /// nothing ambiguous to model, and if it is a scalar or a byte stream the override is
    /// pointed at the wrong operation. Both are a hard error rather than a silent no-op,
    /// because an override that quietly does nothing is the worst kind.
    fn one_or_many(&mut self, name: &str, declared: RustType, op_id: &str) -> Result<RustType> {
        let RustType::Model(_) = &declared else {
            bail!(
                "overrides.toml [one_or_many] names {op_id:?}, whose declared success type is                  {declared:?}. The override wraps a single model in a One/Many enum; there is                  nothing for it to do here."
            );
        };
        if self.spec.definitions.contains_key(name) {
            bail!(
                "overrides.toml [one_or_many] wants to synthesize {name:?} for {op_id:?}, but                  the spec already defines a {name:?}. Pick a name the spec does not use."
            );
        }
        let doc = Doc::from_text(Some(&format!(
            "Either one entry or a list of them, depending on the request.

             `{op_id}` is declared in the specification as returning a single value, and              returns an array instead for some requests. Neither shape is a superset of the              other, so the type accepts both and `into_vec` collapses the distinction for              callers that do not care which arrived."
        )));
        self.synth_models.insert(
            name.to_owned(),
            Model {
                wire: name.to_owned(),
                rust: ident_pascal(name),
                kind: ModelKind::OneOrMany(declared),
                doc,
                module: ident_snake(name),
            },
        );
        Ok(RustType::Model(name.to_owned()))
    }

    /// The token scope this operation needs.
    ///
    /// Derived from `tags[0]` plus the HTTP method, and correctable in `overrides.toml`. It
    /// feeds the `InsufficientScope` error message, which must name a scope the user can
    /// actually create: Gitea scopes are fixed when a token is minted, so a wrong name here
    /// costs the user a pointless trip through the web UI.
    fn scope(
        &self,
        op: &crate::swagger::Operation,
        method: crate::swagger::HttpMethod,
    ) -> Option<String> {
        if let Some(s) = self.ov.scope.get(&op.operation_id) {
            return Some(s.clone());
        }
        let tag = op.tags.first()?.as_str();
        // Gitea's scope areas are the tag names, with two exceptions: `miscellaneous` and
        // `settings` both live under `misc`.
        let area = match tag {
            "miscellaneous" | "settings" => "misc",
            other => other,
        };
        let access = if matches!(
            method,
            crate::swagger::HttpMethod::Get
                | crate::swagger::HttpMethod::Head
                | crate::swagger::HttpMethod::Options
        ) {
            "read"
        } else {
            "write"
        };
        Some(format!("{access}:{area}"))
    }

    fn groups(&self, operations: &[Operation]) -> Vec<Group> {
        let mut by_group: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, op) in operations.iter().enumerate() {
            by_group.entry(op.group.as_str()).or_default().push(i);
        }
        by_group
            .into_iter()
            .map(|(name, ops)| Group {
                doc: self.ov.group_doc.get(name).cloned().unwrap_or_default(),
                module: ident_snake(name),
                name: name.to_owned(),
                operations: ops,
            })
            .collect()
    }
}

/// Whether a body field can become a typed layer-2 flag.
///
/// Depth 1 only: a nested object or an array of objects has no sensible flag spelling, and
/// inventing one (`--foo.bar`, or JSON-in-a-flag) would be a worse experience than
/// `--body-file`. Scalars, enums, maps and arrays of scalars all have obvious spellings.
fn flattenable(ty: &RustType) -> bool {
    match ty {
        RustType::Model(_) | RustType::Json | RustType::File | RustType::Bytes => false,
        RustType::Vec(inner) | RustType::Map(inner) => flattenable(inner),
        _ => true,
    }
}

fn pagination(query: &[Param], response_name: Option<&str>) -> Pagination {
    let page = query.iter().find(|p| p.wire == "page");
    let Some(page) = page else {
        return Pagination::None;
    };
    Pagination::Paged {
        page_param: page.wire.clone(),
        limit_param: query.iter().find(|p| p.wire == "limit").map(|p| p.wire.clone()),
        // The `FooList` / `FooListWithoutPagination` naming convention is the only in-spec
        // signal that an endpoint paginates. It drives help text; the runtime decides when to
        // stop from the actual `Link` header, because the spec declares none.
        shared_response_paginated: response_name
            .is_some_and(|n| n.ends_with("List") && !n.ends_with("WithoutPagination")),
    }
}

/// Which file within a group an operation is emitted into.
///
/// The 198-operation `repo` group would bust the 1500-line-per-file cap several times over, so
/// it is split by the first literal path segment after `/repos/{owner}/{repo}/` — a rule that
/// tracks the API's own structure rather than an arbitrary alphabetical split, so a new endpoint
/// lands in the file a reader would look in.
///
/// Other groups are one file each. When a group busts the cap, the emitter fails and names this
/// function; add a rule here rather than raising the cap.
fn sub_bucket(group: &str, path: &str) -> String {
    if group != "repo" {
        return "mod".to_owned();
    }
    let Some(rest) = path.strip_prefix("/repos/") else {
        return "misc".to_owned();
    };
    // Skip `{owner}/{repo}`; what follows is the API's own grouping.
    let mut segments = rest.split('/').skip(2);
    let Some(seg) = segments.next().filter(|s| !s.is_empty()) else {
        return "misc".to_owned();
    };
    match seg {
        "pulls" => "pulls",
        "issues" => "issues",
        "contents" | "contents_ext" | "raw" | "media" | "edit" => "contents",
        "branches" | "branch_protections" => "branches",
        "hooks" | "git_hooks" => "hooks",
        "releases" | "tags" => "releases",
        "actions" | "runners" | "secrets" | "variables" | "dispatches" | "workflows" | "tasks"
        | "artifacts" | "runs" => "actions",
        "git" | "commits" | "compare" | "notes" | "blobs" | "trees" | "refs" | "keys" => "git",
        "collaborators" | "teams" | "assignees" | "reviewers" | "topics" | "subscribers" => {
            "collaborators"
        }
        "wiki" => "wiki",
        _ => "misc",
    }
    .to_owned()
}

fn render_default(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Tarjan's strongly-connected components, over a graph keyed by definition name.
///
/// Iterative rather than recursive: the recursion depth is bounded by the number of definitions
/// (222 today) but the shape of the graph is upstream's to change, and a stack overflow inside a
/// code generator is an unpleasant way to learn that.
fn tarjan(graph: &BTreeMap<&str, BTreeSet<String>>) -> Vec<Vec<String>> {
    struct State {
        index: BTreeMap<String, usize>,
        low: BTreeMap<String, usize>,
        on_stack: BTreeSet<String>,
        stack: Vec<String>,
        next: usize,
        out: Vec<Vec<String>>,
    }

    let mut st = State {
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };

    // Each frame is (node, index of the next successor to visit).
    for root in graph.keys() {
        if st.index.contains_key(*root) {
            continue;
        }
        let mut frames: Vec<(String, usize)> = vec![((*root).to_owned(), 0)];
        st.index.insert((*root).to_owned(), st.next);
        st.low.insert((*root).to_owned(), st.next);
        st.next += 1;
        st.stack.push((*root).to_owned());
        st.on_stack.insert((*root).to_owned());

        while let Some((node, cursor)) = frames.pop() {
            let empty = BTreeSet::new();
            let succ: Vec<&String> = graph.get(node.as_str()).unwrap_or(&empty).iter().collect();

            if cursor < succ.len() {
                let w = succ[cursor].clone();
                frames.push((node.clone(), cursor + 1));
                if !st.index.contains_key(&w) {
                    st.index.insert(w.clone(), st.next);
                    st.low.insert(w.clone(), st.next);
                    st.next += 1;
                    st.stack.push(w.clone());
                    st.on_stack.insert(w.clone());
                    frames.push((w, 0));
                } else if st.on_stack.contains(&w) {
                    let low = st.low[&node].min(st.index[&w]);
                    st.low.insert(node, low);
                }
                continue;
            }

            // All successors visited: close the node.
            if st.low[&node] == st.index[&node] {
                let mut comp = Vec::new();
                while let Some(w) = st.stack.pop() {
                    st.on_stack.remove(&w);
                    let done = w == node;
                    comp.push(w);
                    if done {
                        break;
                    }
                }
                comp.sort();
                st.out.push(comp);
            }
            if let Some((parent, _)) = frames.last() {
                let low = st.low[parent].min(st.low[&node]);
                st.low.insert(parent.clone(), low);
            }
        }
    }
    st.out
}

/// Tests against the committed spec.
///
/// These are the ones that would actually have caught a mistake: everything else in this crate
/// is unit-tested against small synthetic inputs, and small synthetic inputs are exactly what a
/// generator bug hides from. The vendored spec is committed, so these are hermetic despite
/// reading a file.
#[cfg(test)]
mod vendored {
    use super::*;
    use std::sync::OnceLock;

    fn ir() -> &'static Ir {
        static IR: OnceLock<Ir> = OnceLock::new();
        IR.get_or_init(|| {
            let root = crate::workspace_root();
            let loaded = crate::spec::load(&root).expect("spec/ is committed");
            let ov = Overrides::load().expect("overrides.toml is committed");
            lower(&loaded, &ov).expect("lowering the vendored spec must succeed")
        })
    }

    /// The request-body `Presence` policy, against the real spec.
    ///
    /// The bug this pins: with the response policy applied to a request model,
    /// `gea issue edit 42 --title x` serializes `"body":""` and Gitea erases the issue body.
    #[test]
    fn no_request_body_model_has_a_default_plain_field() {
        let ir = ir();
        let by_wire: BTreeMap<&str, &Model> =
            ir.models.iter().map(|m| (m.wire.as_str(), m)).collect();

        let root = crate::workspace_root();
        let loaded = crate::spec::load(&root).unwrap();
        let ov = Overrides::load().unwrap();
        let mut l = Lowerer::new(&loaded.spec, &ov);
        l.classify();
        let request_models = l.find_request_models();

        // The seed set is 120 `$ref` bodies naming 81 definitions; the closure adds the nested
        // ones (`CreateFileOptions.author: Identity`, `ChangeFilesOptions.files[]`).
        assert_eq!(request_models.len(), 89, "{request_models:?}");

        let mut structs = 0;
        for name in &request_models {
            let Some(m) = by_wire.get(name.as_str()) else { continue };
            let ModelKind::Struct(fields) = &m.kind else { continue };
            structs += 1;
            for f in fields {
                assert_ne!(
                    f.presence,
                    crate::ir::types::Presence::DefaultPlain,
                    "{name}.{} is a request-body field spelled as a bare `T`: a caller who \
                     never set it would send its zero value and blank whatever the server had",
                    f.wire
                );
            }
        }
        assert_eq!(structs, 88, "the 88 request-body structs");
    }

    /// The other half: the response policy is unchanged, because weakening it would put an
    /// `.unwrap_or_default()` at every call site that reads a description or a label list.
    #[test]
    fn response_models_keep_the_tight_typing_policy() {
        let ir = ir();
        let repo = ir.models.iter().find(|m| m.wire == "Repository").unwrap();
        let ModelKind::Struct(fields) = &repo.kind else { panic!("Repository is a struct") };
        let by_name: BTreeMap<&str, &Field> = fields.iter().map(|f| (f.wire.as_str(), f)).collect();
        assert_eq!(by_name["description"].presence, crate::ir::types::Presence::DefaultPlain);
        assert_eq!(by_name["topics"].presence, crate::ir::types::Presence::DefaultPlain);
        // ...and the two exceptions the policy always carved out stay exceptions.
        assert_eq!(by_name["created_at"].presence, crate::ir::types::Presence::Optional);
        assert_eq!(by_name["parent"].presence, crate::ir::types::Presence::OptionalBoxed);
    }

    /// The three definitions used as *both* a request body and a response resolve as request
    /// bodies. An over-optional response costs an `.unwrap_or_default()`; an under-optional
    /// request wipes a repository's tracker configuration.
    #[test]
    fn a_definition_used_as_both_resolves_as_a_request_body() {
        let ir = ir();
        for name in ["ExternalTracker", "ExternalWiki", "InternalTracker"] {
            let m = ir.models.iter().find(|m| m.wire == name).unwrap();
            let ModelKind::Struct(fields) = &m.kind else { panic!("{name} is a struct") };
            assert!(!fields.is_empty());
            for f in fields {
                assert_eq!(
                    f.presence,
                    crate::ir::types::Presence::Optional,
                    "{name}.{} — reachable from EditRepoOption as well as from Repository",
                    f.wire
                );
            }
        }
    }

    /// `repoGetContents` answers an array when `filepath` names a directory, which the spec
    /// does not say. Without the override the generated method hard-fails decoding a directory.
    #[test]
    fn get_contents_returns_one_or_many() {
        let ir = ir();
        let op = ir.operation("repoGetContents").unwrap();
        assert_eq!(op.success.ty, RustType::Model("ContentsResponseOrList".into()));

        let m = ir.models.iter().find(|m| m.wire == "ContentsResponseOrList").unwrap();
        assert!(matches!(
            &m.kind,
            ModelKind::OneOrMany(RustType::Model(inner)) if inner == "ContentsResponse"
        ));

        // The sibling route, which really does only ever answer an array, is untouched.
        let list = ir.operation("repoGetContentsList").unwrap();
        assert_eq!(
            list.success.ty,
            RustType::Vec(Box::new(RustType::Model("ContentsResponse".into())))
        );
    }

    #[test]
    fn the_loader_reproduces_the_independently_verified_counts() {
        let root = crate::workspace_root();
        let loaded = crate::spec::load(&root).unwrap();
        crate::stats::Stats::compute(&loaded.spec).verify(&loaded.lock.version).unwrap();
    }

    #[test]
    fn four_hundred_and_eighty_two_operations_with_no_name_collisions() {
        // The M2 exit criterion. A collision found here costs an `overrides.toml` entry; the
        // same collision found after the emitters land costs a renamed public command.
        let ir = ir();
        assert_eq!(ir.operations.len(), 482);
        assert_eq!(ir.name_counts(), (482, 482));
    }

    #[test]
    fn the_committed_name_lock_matches_the_ir() {
        // If this fails, either lowering changed a name or someone edited the lock by hand.
        // Both are exactly what `--accept-renames` exists to make deliberate.
        let text =
            std::fs::read_to_string(crate::spec::name_lock_path(&crate::workspace_root())).unwrap();
        let locked: BTreeMap<String, crate::name_lock::Entry> = toml::from_str(&text).unwrap();
        assert_eq!(locked.len(), 482);
        for op in &ir().operations {
            let e = locked
                .get(&op.op_id)
                .unwrap_or_else(|| panic!("{} is missing from spec/name-lock.toml", op.op_id));
            assert_eq!(
                (e.group.as_str(), e.command.as_str(), e.fn_name.as_str()),
                (op.group.as_str(), op.command.as_str(), op.fn_name.as_str()),
                "{} drifted from the lock",
                op.op_id
            );
        }
    }

    #[test]
    fn repository_parent_is_the_only_boxed_field() {
        // `Repository.parent: Option<Box<Repository>>`. Without the SCC pass, rustc rejects
        // the models crate with "recursive type has infinite size", pointing at a struct
        // nobody wrote, somewhere inside 9,000 generated lines.
        //
        // `GPGKey.subkeys: Vec<GPGKey>` is deliberately *not* here: a Vec is already an
        // indirection, so boxing it would add an allocation and an unusable
        // `Option<Box<Vec<_>>>`.
        let ir = ir();
        assert_eq!(ir.cyclic_models, ["Repository".to_owned()].into_iter().collect());

        let boxed: Vec<String> = ir
            .models
            .iter()
            .filter_map(|m| match &m.kind {
                ModelKind::Struct(fields) => Some((m, fields)),
                _ => None,
            })
            .flat_map(|(m, fields)| {
                fields
                    .iter()
                    .filter(|f| f.presence == crate::ir::types::Presence::OptionalBoxed)
                    .map(move |f| format!("{}.{}", m.wire, f.wire))
            })
            .collect();
        assert_eq!(boxed, ["Repository.parent"]);
    }

    #[test]
    fn path_like_encoding_applies_to_filepath_and_ref_and_nothing_else() {
        // Get this wrong and `gea raw repo get-contents o r src/main.rs` 404s with no hint
        // why, because `src/main.rs` went out as `src%2Fmain.rs`. Get it wrong the other way
        // and an owner literally named `a/b` escapes into the path.
        let mut path_like = BTreeSet::new();
        let mut segment = BTreeSet::new();
        for op in &ir().operations {
            for p in &op.path_params {
                match p.encoding {
                    PathEncoding::PathLike => path_like.insert(p.wire.clone()),
                    PathEncoding::Segment => segment.insert(p.wire.clone()),
                };
            }
        }
        assert_eq!(path_like, ["filepath".to_owned(), "ref".to_owned()].into_iter().collect());
        assert!(segment.contains("owner"), "owner must be segment-encoded");
        assert!(segment.contains("sha"));
        assert!(!segment.contains("filepath"));
    }

    #[test]
    fn both_dotted_paths_keep_the_dot_as_a_literal_chunk() {
        for op_id in ["repoDownloadPullDiffOrPatch", "repoDownloadCommitDiffOrPatch"] {
            let op = ir().operation(op_id).unwrap_or_else(|| panic!("{op_id} is missing"));
            let dots = op
                .path
                .chunks
                .iter()
                .filter(|c| matches!(c, crate::ir::paths::Chunk::Lit(l) if l == "."))
                .count();
            assert_eq!(dots, 1, "{op_id}: {:?}", op.path.chunks);
            // The parameter must not have absorbed the dot into its name.
            assert!(op.path.params.contains(&"diffType".to_owned()));
        }
    }

    #[test]
    fn the_eleven_pascal_case_operations_are_reachable_under_humane_groups() {
        let ir = ir();
        for (op_id, group, command) in [
            ("GetWorkflowRun", "run", "view"),
            ("ActionsDispatchWorkflow", "workflow", "dispatch"),
            ("GetTree", "git", "tree"),
            ("ListActionTasks", "task", "list"),
            // Not PascalCase, but the same problem: a verb where the group word should be.
            ("getWorkflowRuns", "run", "list"),
            ("getArtifacts", "artifact", "list"),
        ] {
            let op = ir.operation(op_id).unwrap();
            assert_eq!((op.group.as_str(), op.command.as_str()), (group, command));
        }
        // ...and none of them ended up buried in the 170-command `repo` group.
        for op in &ir.operations {
            if op.op_id.starts_with("Actions") || op.op_id.starts_with("ListAction") {
                assert_ne!(op.group, "repo", "{} landed in the junk group", op.op_id);
            }
        }
    }

    #[test]
    fn every_operation_has_a_scope_and_a_success_type() {
        // `scope` feeds the `InsufficientScope` error message; an operation without one would
        // produce "needs: unknown" for no reason.
        for op in &ir().operations {
            assert!(op.scope.is_some(), "{} has no scope", op.op_id);
            assert!((200..300).contains(&op.success.status), "{}", op.op_id);
        }
    }

    #[test]
    fn the_three_multipart_operations_have_file_form_data() {
        let file_ops: Vec<&str> = ir()
            .operations
            .iter()
            .filter(|o| o.form_data.iter().any(|p| p.ty == RustType::File))
            .map(|o| o.op_id.as_str())
            .collect();
        assert_eq!(
            file_ops,
            [
                "issueCreateIssueAttachment",
                "issueCreateIssueCommentAttachment",
                "repoCreateReleaseAttachment"
            ]
        );
    }

    #[test]
    fn bodies_are_almost_all_named_definitions_with_typed_properties() {
        // Layer 2's per-field typed flags only exist because bodies are overwhelmingly `$ref`s
        // to definitions with real properties. If that ratio moves, the design assumption needs
        // revisiting rather than silently degrading every affected command to `--body-file`.
        //
        // The exact breakdown, so a spec bump that shifts it fails loudly:
        //   120  $ref to a definition with typed properties  -> flags
        //     1  inline `type: string` (renderMarkdownRaw)    -> a bare String body
        // No body is a free-form object (Forgejo's `ForgeLike` was), so none is untyped JSON.
        let bodies: Vec<&Body> = ir().operations.iter().filter_map(|o| o.body.as_ref()).collect();
        assert_eq!(bodies.len(), 121);
        assert_eq!(bodies.iter().filter(|b| matches!(b.ty, RustType::Model(_))).count(), 120);
        assert_eq!(bodies.iter().filter(|b| b.ty == RustType::Json).count(), 0);
        assert_eq!(bodies.iter().filter(|b| b.ty == RustType::String).count(), 1);

        // And flattening actually produced flags for the typed ones.
        let create_pr = ir().operation("repoCreatePullRequest").unwrap().body.as_ref().unwrap();
        assert!(create_pr.flat.iter().any(|f| f.flag == "title"));
        assert!(create_pr.flat.iter().any(|f| f.flag == "head"));
        // `labels` is an array of integers, which has an obvious flag spelling...
        assert!(create_pr.flat.iter().any(|f| f.flag == "labels"));
        // ...whereas a nested object would not, and is named in `deep` for `--help` to mention.
        assert!(create_pr.deep.iter().all(|d| !create_pr.flat.iter().any(|f| &f.wire == d)));
    }

    #[test]
    fn no_generated_identifier_is_empty_or_starts_with_a_digit() {
        // `format_ident!` panics on these, and a panic inside the emitters is a much worse
        // error message than an assertion here.
        let ir = ir();
        for op in &ir.operations {
            for i in [&op.module, &op.fn_name] {
                assert!(!i.as_str().is_empty(), "{}", op.op_id);
                assert!(!i.as_str().starts_with(|c: char| c.is_ascii_digit()), "{}", op.op_id);
            }
        }
        for m in &ir.models {
            if let ModelKind::Struct(fields) = &m.kind {
                for f in fields {
                    assert!(!f.rust.as_str().is_empty(), "{}.{}", m.wire, f.wire);
                }
            }
        }
    }

    #[test]
    fn keyword_fields_are_raw_escaped() {
        // Gitea has fields named `type` and `ref`; without `r#` the models crate does not
        // compile.
        let hook = ir().models.iter().find(|m| m.wire == "Hook").unwrap();
        let ModelKind::Struct(fields) = &hook.kind else { panic!("Hook is a struct") };
        let ty = fields.iter().find(|f| f.wire == "type").unwrap();
        assert_eq!(ty.rust.to_string(), "r#type");
        assert!(!ty.needs_rename, "the wire name and the bare ident agree, so no serde rename");
    }

    #[test]
    fn the_ir_dump_is_valid_json() {
        // `--dump-ir` is how the emitter agents read lowering's decisions; if it is not
        // pipeable through jq it is not much use.
        let dumped = ir().dump();
        let _: serde_json::Value = serde_json::from_str(&dumped).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> BTreeMap<&'static str, BTreeSet<String>> {
        let mut g = BTreeMap::new();
        for (from, tos) in edges {
            let from: &'static str = Box::leak(from.to_string().into_boxed_str());
            g.insert(from, tos.iter().map(|t| (*t).to_owned()).collect());
        }
        g
    }

    #[test]
    fn tarjan_finds_a_self_loop() {
        // `Repository.parent: Repository` — the real case, and the one that would otherwise
        // produce "recursive type has infinite size".
        let g = graph(&[("Repository", &["Repository"]), ("User", &[])]);
        let sccs = tarjan(&g);
        assert!(sccs.contains(&vec!["Repository".to_owned()]));
        assert!(sccs.contains(&vec!["User".to_owned()]));
    }

    #[test]
    fn tarjan_finds_a_mutual_cycle() {
        let g = graph(&[("A", &["B"]), ("B", &["A"]), ("C", &["A"])]);
        let sccs = tarjan(&g);
        assert!(
            sccs.iter().any(|c| c == &vec!["A".to_owned(), "B".to_owned()]),
            "A and B must land in one component: {sccs:?}"
        );
        assert!(sccs.iter().any(|c| c == &vec!["C".to_owned()]));
    }

    #[test]
    fn tarjan_handles_a_long_chain_without_recursing() {
        // The reason this is iterative. A 5,000-node chain would blow a recursive
        // implementation's stack, and "the generator segfaulted" is a bad error message.
        let names: Vec<String> = (0..5000).map(|i| format!("n{i:05}")).collect();
        let mut g: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (i, name) in names.iter().enumerate() {
            let next = names.get(i + 1).cloned();
            g.insert(name.as_str(), next.into_iter().collect());
        }
        assert_eq!(tarjan(&g).len(), 5000);
    }

    #[test]
    fn flattenable_stops_at_depth_one() {
        assert!(flattenable(&RustType::String));
        assert!(flattenable(&RustType::Vec(Box::new(RustType::I64))));
        assert!(flattenable(&RustType::OpenEnum("StateType".into())));
        // A nested object has no sensible flag spelling; `--body-file` covers it.
        assert!(!flattenable(&RustType::Model("Repository".into())));
        assert!(!flattenable(&RustType::Vec(Box::new(RustType::Model("Label".into())))));
        assert!(!flattenable(&RustType::Json));
    }

    #[test]
    fn repo_operations_bucket_by_the_apis_own_structure() {
        // A reader looking for `create-pull-request` should find it in `pulls`, not in
        // whichever alphabetical slice a line-count split happened to produce.
        assert_eq!(sub_bucket("repo", "/repos/{owner}/{repo}/pulls/{index}"), "pulls");
        assert_eq!(sub_bucket("repo", "/repos/{owner}/{repo}/contents/{filepath}"), "contents");
        assert_eq!(sub_bucket("repo", "/repos/{owner}/{repo}/git/trees/{sha}"), "git");
        assert_eq!(sub_bucket("repo", "/repos/{owner}/{repo}"), "misc");
        assert_eq!(sub_bucket("repo", "/repos/search"), "misc");
        // Only `repo` is large enough to need splitting today.
        assert_eq!(sub_bucket("user", "/user/keys"), "mod");
    }

    #[test]
    fn pagination_reads_the_shared_response_naming_convention() {
        let page = Param {
            wire: "page".into(),
            rust: Ident::new("page"),
            flag: "page".into(),
            ty: RustType::I32,
            required: false,
            encoding: PathEncoding::Segment,
            ctx_fill: None,
            enum_values: None,
            repeated: false,
            default: None,
            doc: Doc::default(),
        };
        match pagination(std::slice::from_ref(&page), Some("PullRequestList")) {
            Pagination::Paged { shared_response_paginated, .. } => {
                assert!(shared_response_paginated)
            }
            Pagination::None => panic!("a `page` parameter means paginated"),
        }
        match pagination(std::slice::from_ref(&page), Some("UserListWithoutPagination")) {
            Pagination::Paged { shared_response_paginated, .. } => {
                assert!(!shared_response_paginated)
            }
            Pagination::None => panic!("a `page` parameter means paginated"),
        }
        assert!(matches!(pagination(&[], Some("PullRequestList")), Pagination::None));
    }
}

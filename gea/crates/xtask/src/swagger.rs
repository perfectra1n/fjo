//! A serde model of exactly the Swagger 2.0 subset the Gitea spec uses.
//!
//! Why hand-roll this instead of using an OpenAPI crate: the Gitea spec is Swagger 2.0
//! (not 3.x), it uses no polymorphism at all, and — most importantly — we want to *notice*
//! when upstream starts using a construct we do not model. A general-purpose parser silently
//! accepts `allOf` and hands us a shape the emitters would quietly mistranslate.
//!
//! Two design choices carry that "notice it" property:
//!
//! - [`Schema::extra`] flattens every key we do not model, so [`crate::stats`] can count
//!   `allOf`/`oneOf`/`anyOf`/`not`/`discriminator` and refuse to proceed if any appear.
//! - Maps are [`BTreeMap`], never insertion-ordered. Upstream key order is not a contract,
//!   and letting it leak into the IR would make generated code move for no reason. (The
//!   vendored JSON is key-sorted for the same reason; this is the belt to that suspenders.)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub swagger: String,
    pub info: Info,
    #[serde(default)]
    pub base_path: String,
    /// Document-level default for operations that declare none.
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
    #[serde(default)]
    pub paths: BTreeMap<String, PathItem>,
    #[serde(default)]
    pub definitions: BTreeMap<String, Schema>,
    /// The shared `#/responses/...` section. Gitea declares 174 named responses and almost
    /// every operation `$ref`s into it, so resolving these is mandatory, not optional.
    #[serde(default)]
    pub responses: BTreeMap<String, Response>,
    #[serde(default)]
    pub security_definitions: BTreeMap<String, SecurityScheme>,
}

#[derive(Debug, Deserialize)]
pub struct Info {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub struct SecurityScheme {
    #[serde(rename = "type")]
    pub ty: String,
    pub name: Option<String>,
    #[serde(rename = "in")]
    pub location: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Head => "HEAD",
            HttpMethod::Options => "OPTIONS",
        }
    }

    /// Whether a retry of this method could duplicate a side effect. Mirrors the rule in
    /// `gitea-core::http::retry`; the generated meta table carries it so layer 2 does not
    /// have to re-derive it.
    pub const fn is_idempotent(self) -> bool {
        matches!(
            self,
            HttpMethod::Get
                | HttpMethod::Head
                | HttpMethod::Options
                | HttpMethod::Put
                | HttpMethod::Delete
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct PathItem {
    pub get: Option<Operation>,
    pub post: Option<Operation>,
    pub put: Option<Operation>,
    pub patch: Option<Operation>,
    pub delete: Option<Operation>,
    pub head: Option<Operation>,
    pub options: Option<Operation>,
    /// Path-level parameters shared by every operation on the path. Gitea declares none,
    /// but the field exists so that a future spec adding them fails a test rather than
    /// silently dropping parameters.
    #[serde(default)]
    pub parameters: Vec<Parameter>,
}

impl PathItem {
    /// Operations in a **fixed** method order, deliberately not source order. Source order is
    /// whatever the Go generator emitted and is not a contract we want generated code to
    /// inherit.
    pub fn operations(&self) -> Vec<(HttpMethod, &Operation)> {
        [
            (HttpMethod::Get, &self.get),
            (HttpMethod::Post, &self.post),
            (HttpMethod::Put, &self.put),
            (HttpMethod::Patch, &self.patch),
            (HttpMethod::Delete, &self.delete),
            (HttpMethod::Head, &self.head),
            (HttpMethod::Options, &self.options),
        ]
        .into_iter()
        .filter_map(|(m, o)| o.as_ref().map(|o| (m, o)))
        .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub operation_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    #[serde(default)]
    pub responses: BTreeMap<String, Response>,
    #[serde(default)]
    pub deprecated: bool,
    /// Per-operation security requirements. Gitea declares these once at the document
    /// level, so this is normally empty.
    #[serde(default)]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamIn {
    Path,
    Query,
    Body,
    FormData,
    Header,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: ParamIn,
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,

    // Non-body parameters carry their type inline; body parameters carry a `schema`. Swagger
    // 2.0 splits these, which is why both sets of fields live on one struct here.
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub format: Option<String>,
    pub items: Option<Box<Schema>>,
    #[serde(rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    pub collection_format: Option<String>,
    pub default: Option<serde_json::Value>,
    pub minimum: Option<f64>,

    pub schema: Option<Schema>,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    /// Set when the operation points at the shared `#/responses/...` section instead of
    /// declaring a schema inline.
    #[serde(rename = "$ref")]
    pub ref_: Option<String>,
    #[serde(default)]
    pub description: String,
    pub schema: Option<Schema>,
    /// Contrary to the common claim that this spec declares no response headers, 32 of the
    /// shared responses *do* declare `X-Total-Count`. It is still not declared on the
    /// operations themselves and `Link` is never declared anywhere, so pagination remains
    /// hand-written — but the header is worth surfacing rather than asserting away.
    #[serde(default)]
    pub headers: BTreeMap<String, Schema>,
}

impl Response {
    /// The shared-response name this response points at, e.g. `PullRequestList`.
    ///
    /// The `FooList` / `FooListWithoutPagination` convention in these names is the only
    /// in-spec signal that an endpoint paginates, so the IR keeps it for help text.
    pub fn ref_name(&self) -> Option<&str> {
        self.ref_.as_deref().and_then(|r| r.rsplit('/').next())
    }
}

/// `additionalProperties` is `true`/`false` in some Swagger documents and a schema in others.
/// Gitea only ever uses the schema form, but modelling both means a spec bump produces a
/// clear IR decision instead of a parse error 200 lines into a stack trace.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AdditionalProperties {
    Bool(bool),
    Schema(Box<Schema>),
}

impl AdditionalProperties {
    pub fn schema(&self) -> Option<&Schema> {
        match self {
            AdditionalProperties::Schema(s) => Some(s),
            AdditionalProperties::Bool(_) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    #[serde(rename = "$ref")]
    pub ref_: Option<String>,
    #[serde(rename = "type")]
    pub ty: Option<String>,
    pub format: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Schema>,
    #[serde(default)]
    pub required: Vec<String>,
    pub items: Option<Box<Schema>>,
    pub additional_properties: Option<AdditionalProperties>,
    #[serde(rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub read_only: bool,

    /// Every key we do not model, kept rather than discarded so that
    /// [`crate::stats::Stats`] can prove `allOf`/`oneOf`/`anyOf`/`not`/`discriminator` are
    /// absent. The moment one appears, the polymorphism count stops being zero and codegen
    /// stops — which is exactly the behaviour we want, because "no polymorphism" is the
    /// assumption that makes this generator tractable at all.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// The five keys whose absence the whole generator design rests on.
pub const POLYMORPHISM_KEYS: [&str; 5] = ["allOf", "anyOf", "discriminator", "not", "oneOf"];

impl Schema {
    /// The definition name a `$ref` points at, e.g. `Repository` for
    /// `#/definitions/Repository`.
    pub fn ref_target(&self) -> Option<&str> {
        self.ref_.as_deref().and_then(|r| r.strip_prefix("#/definitions/"))
    }

    pub fn is_array(&self) -> bool {
        self.ty.as_deref() == Some("array")
    }

    /// A `type: object` with neither properties nor `additionalProperties`: Gitea emits
    /// this for Go types the swagger generator could not introspect
    /// (`ForgeLike`, `ForgeOutbox`). There is nothing to type, so these become
    /// `serde_json::Value`.
    pub fn is_free_form_object(&self) -> bool {
        self.ty.as_deref() == Some("object")
            && self.properties.is_empty()
            && self.additional_properties.is_none()
            && self.ref_.is_none()
    }

    /// How many polymorphism keys this node uses. Should be zero, always.
    pub fn polymorphism_keys(&self) -> usize {
        POLYMORPHISM_KEYS.iter().filter(|k| self.extra.contains_key(**k)).count()
    }

    /// `self` plus every schema node nested inside it, in a deterministic order.
    ///
    /// Returned as a `Vec` rather than an iterator because the recursion is what makes this
    /// readable and because the largest input is 246 definitions — the allocation is free
    /// compared to being able to reason about it. This is the traversal `spec-stats` counts
    /// over, and getting the traversal wrong is how a loader ends up 6 short on `int64`.
    pub fn walk(&self) -> Vec<&Schema> {
        let mut out = Vec::new();
        self.walk_into(&mut out);
        out
    }

    fn walk_into<'a>(&'a self, out: &mut Vec<&'a Schema>) {
        out.push(self);
        for child in self.properties.values() {
            child.walk_into(out);
        }
        if let Some(items) = &self.items {
            items.walk_into(out);
        }
        if let Some(AdditionalProperties::Schema(ap)) = &self.additional_properties {
            ap.walk_into(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_reaches_array_items_and_map_values() {
        // The bug this prevents: counting `format: int64` over top-level properties only
        // yields 165 for the Gitea spec, where the true count is 171. The six it misses
        // are array `items` and definitions that are themselves an int64 alias — exactly
        // the nodes this test pins.
        let s: Schema = serde_json::from_str(
            r#"{
                 "type": "object",
                 "properties": {
                   "ids":   { "type": "array", "items": { "type": "integer", "format": "int64" } },
                   "sizes": { "type": "object",
                              "additionalProperties": { "type": "integer", "format": "int64" } }
                 }
               }"#,
        )
        .unwrap();
        let int64s = s.walk().iter().filter(|n| n.format.as_deref() == Some("int64")).count();
        assert_eq!(int64s, 2);
    }

    #[test]
    fn unmodelled_keys_are_kept_so_polymorphism_cannot_hide() {
        let s: Schema =
            serde_json::from_str(r##"{ "allOf": [{ "$ref": "#/definitions/A" }] }"##).unwrap();
        assert_eq!(s.polymorphism_keys(), 1);
    }

    #[test]
    fn additional_properties_accepts_both_shapes() {
        let boolean: Schema = serde_json::from_str(r#"{ "additionalProperties": true }"#).unwrap();
        assert!(boolean.additional_properties.as_ref().unwrap().schema().is_none());
        let schema: Schema =
            serde_json::from_str(r#"{ "additionalProperties": { "type": "string" } }"#).unwrap();
        assert_eq!(
            schema.additional_properties.as_ref().unwrap().schema().unwrap().ty.as_deref(),
            Some("string")
        );
    }

    #[test]
    fn operations_come_back_in_fixed_method_order() {
        // Not source order: generated code must not move because upstream reordered a JSON
        // object.
        let p: PathItem = serde_json::from_str(
            r#"{ "delete": { "operationId": "d", "responses": {} },
                 "get":    { "operationId": "g", "responses": {} },
                 "post":   { "operationId": "p", "responses": {} } }"#,
        )
        .unwrap();
        let ids: Vec<_> = p.operations().iter().map(|(_, o)| o.operation_id.as_str()).collect();
        assert_eq!(ids, ["g", "p", "d"]);
    }
}

//! Spec types → Rust types, and the `Presence` decision.
//!
//! ## The tight-typing policy
//!
//! The principle: **collapse `Option` away only where the zero value is unambiguous and
//! harmless; keep `Option` where "absent" and "zero" mean different things.**
//!
//! Only 39 of 246 definitions declare `required`, so almost every field takes the not-required
//! column of this table:
//!
//! | spec | not-required → | Rust |
//! | --- | --- | --- |
//! | `boolean` / `integer` / `int64` / `number` / `string` | [`Presence::DefaultPlain`] | `bool` / `i32` / `i64` / `f64` / `String` |
//! | `integer: uint64` | `DefaultPlain` | `u64` via `de::lenient_u64` |
//! | `array` | `DefaultPlain` | `Vec<T>` — never `Option<Vec<T>>` |
//! | `additionalProperties` | `DefaultPlain` | `BTreeMap<String, T>` |
//! | free-form object | `DefaultPlain` | `serde_json::Value` |
//! | `string` + `enum` | `DefaultPlain` | an open enum (which has a `Default`) |
//! | `string: date-time` | [`Presence::Optional`] | `Option<Timestamp>` |
//! | `$ref` → struct | `Optional` | `Option<T>`, or [`Presence::OptionalBoxed`] on a cycle |
//!
//! `String::new()` for an absent description is harmless, and `vec![]` for absent labels
//! deletes `.unwrap_or_default()` from every call site. But a defaulted timestamp is a *lie*:
//! rendered through `timeago` it reads "56 years ago". And a default-constructed
//! `Option<User>` prints an empty author column while code does `pr.user.login` and silently
//! gets `""`. Those two are worth an `Option` each.
//!
//! ## The table above is the *response* policy, and only the response policy
//!
//! Everything above reasons about a value **arriving** from the server, where "absent" and
//! "zero" really are interchangeable. Run the same policy over a type that is **serialized
//! into a request body** and it destroys data:
//!
//! ```text
//! EditIssueOption { title: String, body: String, ... }   // DefaultPlain, no skip_serializing_if
//! gea issue edit 42 --title "new title"
//!   => PATCH {"title":"new title","body":"","ref":"","milestone":0, ...}
//! ```
//!
//! Gitea applies a PATCH field-by-field, so every field the user did not mention is
//! overwritten with its zero value: the issue body is erased, the milestone is cleared. The
//! request never looked wrong and nothing errored.
//!
//! So [`Role`] splits the policy in two. A request-body model gets [`Presence::Optional`] for
//! every not-required field — `Option<T>` plus `skip_serializing_if = "Option::is_none"`, so an
//! unset field is **omitted from the JSON** rather than sent as a zero. A response model keeps
//! the table above unchanged.
//!
//! ## `uint64` is a Go-ism
//!
//! `format: uint64` is not valid Swagger 2.0; Gitea emits it for two `PullReviewComment`
//! fields because Go's type is `uint64`. Mapping it to `u64` with a lenient deserializer is
//! more honest than pretending it is an `i64`.

use serde::Serialize;

/// A Rust type in the generated surface. Rendering to tokens is the emitters' job — this is
/// only the decision about *which* type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum RustType {
    Bool,
    I32,
    I64,
    U64,
    F64,
    String,
    /// `gitea_core::types::Timestamp`, always behind an `Option`. See the module docs.
    Timestamp,
    Vec(Box<RustType>),
    /// `BTreeMap<String, T>`. BTree rather than Hash so `--json` output and `Debug` are
    /// deterministic — the same reason `clippy.toml` bans `HashMap` in this workspace.
    Map(Box<RustType>),
    /// A generated struct, by definition name.
    Model(String),
    /// A generated open enum, by type name.
    OpenEnum(String),
    /// A curated ID newtype from `gitea_core::types::ids`.
    Newtype(String),
    /// `serde_json::Value`: free-form objects, and `application/ld+json` responses.
    Json,
    /// A multipart file part. Streamed, never buffered — a 2 GB release asset must not land in
    /// RAM.
    File,
    /// `()`. The 134 endpoints that answer 204.
    Unit,
    /// A streamed byte body: `application/zip`, `octet-stream`, `gzip`.
    Bytes,
    /// `text/plain` and `text/html` responses.
    Text,
}

impl RustType {
    /// Whether the zero value is a harmless stand-in for "absent".
    pub fn zero_is_harmless(&self) -> bool {
        match self {
            RustType::Bool
            | RustType::I32
            | RustType::I64
            | RustType::U64
            | RustType::F64
            | RustType::String
            | RustType::Text
            | RustType::Vec(_)
            | RustType::Map(_)
            | RustType::Json
            | RustType::OpenEnum(_)
            | RustType::Newtype(_)
            | RustType::Unit => true,
            // A defaulted timestamp renders as "56 years ago"; a defaulted struct prints an
            // empty column while the code reads `""` from it.
            RustType::Timestamp | RustType::Model(_) | RustType::File | RustType::Bytes => false,
        }
    }

    /// The definition this type refers to directly — i.e. an edge that contributes to the
    /// type's *size*. `Vec<T>` and `BTreeMap<_, T>` are excluded on purpose: they are already
    /// an indirection, so they cannot make a type infinitely sized and do not need boxing.
    pub fn direct_model_ref(&self) -> Option<&str> {
        match self {
            RustType::Model(name) => Some(name),
            _ => None,
        }
    }
}

/// How a field is spelled, and what serde attributes it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Presence {
    /// `T`. The spec says required and we believe it.
    Required,
    /// `T` with `#[serde(default)]`.
    DefaultPlain,
    /// `Option<T>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
    Optional,
    /// `Option<Box<T>>`. Only for a `$ref` on a cycle.
    ///
    /// Without this, `Repository { parent: Option<Repository> }` is rejected by rustc as
    /// "recursive type has infinite size" — and finding that in 42k generated lines, where the
    /// error points at a struct you did not write, is a genuinely miserable afternoon.
    OptionalBoxed,
}

/// What a model is *for*, which is what decides its `Presence` policy.
///
/// Assigned per definition by the request-body pass in [`crate::ir::lower`], never per field: a
/// definition becomes exactly one Rust struct, so the two policies cannot be mixed inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Role {
    /// Deserialized from a server response. The tight-typing table in the module docs applies:
    /// collapse `Option` away wherever the zero value is a harmless stand-in for "absent".
    Response,
    /// Serialized into a request body. Every not-required field is `Option<T>` with
    /// `skip_serializing_if`, so a field the caller never set is omitted from the JSON instead
    /// of being sent as its zero value and blanking whatever the server had.
    RequestBody,
}

/// Chooses the `Presence` for one field.
///
/// `on_cycle` comes from the strongly-connected-component pass in
/// [`crate::ir::lower`]; it is true only when this field's `$ref` participates in a cycle that
/// includes the field's own struct.
///
/// `role` is the request-vs-response split described in the module docs. It is checked *after*
/// `required`: a field the spec marks required must still be serialized unconditionally, and
/// `CreateIssueOption.title` is not optional just because the struct is a request body.
pub fn presence(ty: &RustType, required: bool, on_cycle: bool, role: Role) -> Presence {
    if on_cycle {
        // Boxing wins even over `required`: a required recursive field is still infinitely
        // sized, and `Option<Box<T>>` at least deserializes when the server sends the field.
        return Presence::OptionalBoxed;
    }
    if required {
        return Presence::Required;
    }
    if role == Role::RequestBody {
        // The whole point: absent must serialize as *absent*. See the module docs.
        return Presence::Optional;
    }
    if ty.zero_is_harmless() { Presence::DefaultPlain } else { Presence::Optional }
}

/// A request or response media type. Drives `Accept`, `Content-Type`, and the return type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Mime {
    Json,
    /// `application/ld+json`, from the two ActivityPub endpoints. Deserialized as a `Value`
    /// because the shape is not in the spec.
    LdJson,
    TextPlain,
    TextHtml,
    Zip,
    OctetStream,
    Gzip,
    MultipartFormData,
    /// A `Content-Type` we do not special-case. Kept verbatim rather than coerced, so a spec
    /// bump surfaces as an unusual value in `--dump-ir` rather than as a wrong `Accept` header.
    Other(String),
}

impl Mime {
    pub fn parse(s: &str) -> Mime {
        match s {
            "application/json" => Mime::Json,
            "application/ld+json" => Mime::LdJson,
            "text/plain" => Mime::TextPlain,
            "text/html" => Mime::TextHtml,
            "application/zip" => Mime::Zip,
            "application/octet-stream" => Mime::OctetStream,
            "application/gzip" => Mime::Gzip,
            "multipart/form-data" => Mime::MultipartFormData,
            other => Mime::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Mime::Json => "application/json",
            Mime::LdJson => "application/ld+json",
            Mime::TextPlain => "text/plain",
            Mime::TextHtml => "text/html",
            Mime::Zip => "application/zip",
            Mime::OctetStream => "application/octet-stream",
            Mime::Gzip => "application/gzip",
            Mime::MultipartFormData => "multipart/form-data",
            Mime::Other(s) => s,
        }
    }

    /// Whether a response of this type must be streamed to a file rather than decoded.
    pub fn is_binary(&self) -> bool {
        matches!(self, Mime::Zip | Mime::OctetStream | Mime::Gzip)
    }
}

/// Maps a non-body parameter's `type`/`format` pair to a Rust type.
///
/// Parameters never become models: Swagger 2.0 forbids `$ref` outside `body`, and Gitea's
/// query parameters are all scalars or arrays of scalars.
pub fn scalar(ty: Option<&str>, format: Option<&str>) -> RustType {
    match (ty, format) {
        (Some("boolean"), _) => RustType::Bool,
        (Some("integer"), Some("int64")) => RustType::I64,
        (Some("integer"), Some("uint64")) => RustType::U64,
        (Some("integer"), _) => RustType::I32,
        (Some("number"), _) => RustType::F64,
        (Some("string"), Some("date-time")) => RustType::Timestamp,
        (Some("file"), _) => RustType::File,
        // A parameter with no declared type is a string as far as the wire is concerned.
        _ => RustType::String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_follow_the_table() {
        assert_eq!(scalar(Some("boolean"), None), RustType::Bool);
        assert_eq!(scalar(Some("integer"), None), RustType::I32);
        assert_eq!(scalar(Some("integer"), Some("int64")), RustType::I64);
        assert_eq!(scalar(Some("number"), None), RustType::F64);
        assert_eq!(scalar(Some("string"), None), RustType::String);
        assert_eq!(scalar(Some("string"), Some("date-time")), RustType::Timestamp);
        assert_eq!(scalar(Some("file"), None), RustType::File);
    }

    #[test]
    fn uint64_is_not_silently_an_i64() {
        // `format: uint64` is a Go-ism, not valid Swagger 2.0. Two PullReviewComment fields
        // use it; mapping them to i64 would be a quiet lie about the range.
        assert_eq!(scalar(Some("integer"), Some("uint64")), RustType::U64);
    }

    #[test]
    fn timestamps_and_structs_stay_optional() {
        // A defaulted timestamp renders as "56 years ago" and a defaulted struct prints an
        // empty author column while the code silently reads "".
        assert_eq!(
            presence(&RustType::Timestamp, false, false, Role::Response),
            Presence::Optional
        );
        assert_eq!(
            presence(&RustType::Model("User".into()), false, false, Role::Response),
            Presence::Optional
        );
    }

    #[test]
    fn scalars_and_collections_collapse_to_a_plain_default() {
        // `Option<Vec<T>>` would put `.unwrap_or_default()` at every call site for no benefit.
        assert_eq!(
            presence(&RustType::String, false, false, Role::Response),
            Presence::DefaultPlain
        );
        assert_eq!(
            presence(&RustType::Vec(Box::new(RustType::String)), false, false, Role::Response),
            Presence::DefaultPlain
        );
        assert_eq!(
            presence(&RustType::Map(Box::new(RustType::String)), false, false, Role::Response),
            Presence::DefaultPlain
        );
        assert_eq!(presence(&RustType::Json, false, false, Role::Response), Presence::DefaultPlain);
    }

    #[test]
    fn a_request_body_field_is_optional_so_an_unset_field_is_omitted() {
        // The PATCH-overwrite bug. Under the response policy these are all `DefaultPlain`, and
        // a `String` field the caller never touched serializes as `""` — which Gitea applies,
        // blanking the issue body nobody asked to change.
        for ty in [
            RustType::String,
            RustType::Bool,
            RustType::I64,
            RustType::Vec(Box::new(RustType::String)),
            RustType::Map(Box::new(RustType::String)),
            RustType::OpenEnum("StateType".into()),
            RustType::Newtype("IssueIndex".into()),
        ] {
            assert_eq!(
                presence(&ty, false, false, Role::RequestBody),
                Presence::Optional,
                "{ty:?} in a request body must be Option<T> + skip_serializing_if"
            );
        }
    }

    #[test]
    fn a_required_request_body_field_is_still_required() {
        // `CreateIssueOption.title` is not optional just because the struct is a request body;
        // making it so would move a server-side 422 into a silently empty title.
        assert_eq!(presence(&RustType::String, true, false, Role::RequestBody), Presence::Required);
    }

    #[test]
    fn a_field_on_a_cycle_is_boxed_even_when_required() {
        // A required recursive field is still infinitely sized; rustc does not care that the
        // spec insisted.
        assert_eq!(
            presence(&RustType::Model("Repository".into()), true, true, Role::Response),
            Presence::OptionalBoxed
        );
    }

    #[test]
    fn only_direct_refs_count_as_size_edges() {
        // `Vec<GPGKey>` inside `GPGKey` is a cycle in the reference graph but *not* in the size
        // graph: Vec is already an indirection. Boxing it would add a pointless allocation and
        // an awkward `Option<Box<Vec<_>>>`.
        assert_eq!(RustType::Model("Repository".into()).direct_model_ref(), Some("Repository"));
        assert_eq!(
            RustType::Vec(Box::new(RustType::Model("GPGKey".into()))).direct_model_ref(),
            None
        );
    }

    #[test]
    fn mime_round_trips_and_classifies_binaries() {
        for s in [
            "application/json",
            "application/ld+json",
            "text/plain",
            "text/html",
            "application/zip",
            "application/octet-stream",
            "application/gzip",
            "multipart/form-data",
        ] {
            assert_eq!(Mime::parse(s).as_str(), s);
        }
        assert!(Mime::parse("application/zip").is_binary());
        assert!(!Mime::parse("application/json").is_binary());
        // An unknown type is preserved rather than coerced, so it shows up in --dump-ir.
        assert_eq!(Mime::parse("application/x-tar").as_str(), "application/x-tar");
    }
}

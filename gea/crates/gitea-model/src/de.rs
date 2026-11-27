//! Lenient deserializers for Go's marshalling habits.
//!
//! Gitea is written in Go, and Go's `encoding/json` is looser on the wire than a strict Rust
//! deserializer expects. Four cases show up in the generated models, and each would otherwise
//! turn a single odd field into a failed command:
//!
//! - **A nil *pointer* marshals as `null`.** Gitea spells an optional scalar `*string`,
//!   `*int64` or `*bool` throughout, and the specification records only `"type": "string"`, so
//!   the generator emits a plain `String`. `#[serde(default)]` covers an **absent** key, not an
//!   explicit `null`, so `{"merge_commit_sha": null}` — the ordinary wire form of an *open*
//!   pull request — is a hard failure against `String`. That one field took out every `gea pr`
//!   command against any repository with an open pull request. [`null_as_default`] maps `null`
//!   to the zero value, which is what the absent key would have produced anyway, and it is
//!   attached to **every** plain scalar rather than to the fields we happen to have caught a
//!   server sending `null` for.
//!
//! - **A nil slice or map marshals as `null`, not `[]` or `{}`.** The same defect one container
//!   over, and just as routine: `json.Marshal` of a `nil` `[]*User` emits `null`, so
//!   `{"assignees": null}` is the *normal* wire form of an unassigned issue. Against a plain
//!   `Vec<User>` that is a hard deserialization failure — `invalid type: null, expected a
//!   sequence` — and the user loses the whole issue list over a field they never asked about.
//!   [`null_as_empty_vec`] and [`null_as_empty_map`] map `null` to the empty collection.
//!
//! - **`format: uint64`.** Not valid Swagger 2.0 at all — Gitea emits it for two
//!   `PullReviewComment` fields because the Go type is `uint64`. Values do not reliably arrive
//!   as JSON numbers: a large `uint64` may be quoted, and an unset one may be `null`.
//! - **Timestamps declared `required`.** Optional timestamps go through
//!   [`gitea_core::types::opt_timestamp`], which maps Go's zero time to `None`. A *required*
//!   timestamp has no `None` to map to, so it needs its own tolerant path rather than a hard
//!   failure on a value the server is entitled to send.
//!
//! The rule both follow: **never fail the request over one field.** A value we cannot use
//! becomes the zero value plus a note in [`gitea_core::error::compat`], which the binary
//! drains into one grouped message at exit. Tolerant, but not silent.

use std::collections::BTreeMap;
use std::fmt;

use gitea_core::error::compat;
use gitea_core::types::Timestamp;
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Deserializes any defaultable value, mapping JSON `null` to its zero value.
///
/// Attached by the models emitter to **every** plain (non-`Option`) scalar field: `bool`,
/// `i32`, `i64`, `f64`, `String`, the curated ID newtypes, and the open enums.
///
/// ## Why every one of them, and not just the fields we have seen fail
///
/// Go marshals a nil pointer as `null`, and Gitea's models use `*string`, `*int64` and
/// `*bool` for optional scalars throughout. The Swagger specification records none of that —
/// it says `"type": "string"` for a `*string` exactly as it does for a `string` — so the
/// generator has no signal to distinguish the two and every plain scalar is equally exposed.
///
/// `#[serde(default)]`, the container-level attribute every generated model carries, does not
/// help: it covers an **absent** key. An explicit `null` is a present key holding a value of
/// the wrong type, and serde reports `invalid type: null, expected a string`, failing the whole
/// response.
///
/// The consequence is not theoretical. `PullRequest.merge_commit_sha` is `null` on every open
/// pull request, and against `String` that made `pr list`, `pr view`, `pr diff`, `pr status`
/// and `pr checks` exit 1 for any repository containing one. Nothing about the type said it was
/// wrong, and merging the pull request out of band made the identical command succeed.
///
/// ## Why the zero value rather than an `Option`
///
/// The tight-typing policy's rule is that `Option` is collapsed away wherever the zero value is
/// an unambiguous stand-in for "absent". For these types it is: an absent description really is
/// `""`, and keeping it that way removes an `.unwrap_or_default()` from every call site.
/// `null` is simply a third spelling of "absent", alongside the missing key and the empty
/// string, and it lands in the same place. The types where zero would be a *lie* — timestamps
/// and `$ref`s to structs — are already `Option` and never reach this function.
///
/// Unlike [`lenient_u64`] this records no compat note: a `null` pointer is correct, idiomatic
/// Go output, not a value we failed to understand.
pub fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Deserializes a `Vec<T>`, mapping JSON `null` to an empty vector.
///
/// Attached by the models emitter to **every** non-optional `Vec` field, because Go's
/// `encoding/json` marshals a `nil` slice as `null` rather than `[]`. Gitea therefore sends
/// `{"assignees": null}`, `{"labels": null}` and `{"parents": null}` as a matter of routine, and
/// a derived `Vec<T>` rejects all three outright.
///
/// The plan's rule that a `Vec` is never an `Option<Vec>` is what makes this the right shape:
/// "the server sent null" and "the server sent `[]`" and "the field was absent" all mean the
/// same thing to a caller, so all three produce `vec![]` and nothing has to be unwrapped.
///
/// Unlike [`lenient_u64`] this records no compat note: a `null` slice is correct, idiomatic Go
/// output, not a value we failed to understand.
pub fn null_as_empty_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// Deserializes a `BTreeMap<String, T>`, mapping JSON `null` to an empty map.
///
/// The same Go behaviour as [`null_as_empty_vec`], one container over: a `nil`
/// `map[string]string` marshals as `null`, so a webhook with no configuration arrives as
/// `{"config": null}` and fails a derived `BTreeMap`.
pub fn null_as_empty_map<'de, D, T>(d: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<BTreeMap<String, T>>::deserialize(d)?.unwrap_or_default())
}

/// Deserializes a `u64` from a number, a quoted number, `null`, or a whole float.
///
/// Out-of-range and negative inputs become `0` with a compat note. Clamping rather than failing
/// is the point: a count field arriving as `-1` must not cost the user their pull request list.
pub fn lenient_u64<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_any(LenientU64)
}

struct LenientU64;

impl<'de> Visitor<'de> for LenientU64 {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an unsigned integer, a string containing one, or null")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
        Ok(v)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
        Ok(u64::try_from(v).unwrap_or_else(|_| {
            compat::note_unparsed("uint64", &v.to_string());
            0
        }))
    }

    fn visit_i128<E: de::Error>(self, v: i128) -> Result<u64, E> {
        Ok(u64::try_from(v).unwrap_or_else(|_| {
            compat::note_unparsed("uint64", &v.to_string());
            0
        }))
    }

    fn visit_u128<E: de::Error>(self, v: u128) -> Result<u64, E> {
        Ok(u64::try_from(v).unwrap_or_else(|_| {
            compat::note_unparsed("uint64", &v.to_string());
            0
        }))
    }

    /// `1.0` is a `u64` as far as anyone is concerned. `1.5` is not, and truncating it silently
    /// would hide a real disagreement about the field's type, so it is noted.
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
        if v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v <= u64::MAX as f64 {
            return Ok(v as u64);
        }
        compat::note_unparsed("uint64", &v.to_string());
        Ok(0)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<u64, E> {
        compat::note_unparsed("uint64", &v.to_string());
        Ok(0)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
        let s = v.trim();
        if s.is_empty() {
            return Ok(0);
        }
        if let Ok(n) = s.parse::<u64>() {
            return Ok(n);
        }
        // A quoted negative or a quoted float, in that order of likelihood.
        if let Ok(n) = s.parse::<i64>() {
            return self.visit_i64(n);
        }
        if let Ok(n) = s.parse::<f64>() {
            return self.visit_f64(n);
        }
        compat::note_unparsed("uint64", s);
        Ok(0)
    }

    /// JSON `null`.
    fn visit_unit<E: de::Error>(self) -> Result<u64, E> {
        Ok(0)
    }

    fn visit_none<E: de::Error>(self) -> Result<u64, E> {
        Ok(0)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<u64, D::Error> {
        d.deserialize_any(self)
    }
}

/// Deserializes a non-optional [`Timestamp`], falling back to the epoch.
///
/// The 94 optional timestamps use [`gitea_core::types::opt_timestamp`]; exactly one field in
/// v1.27.2 (`EditDeadlineOption.due_date`) is declared `required`, so it has no `None` to fall
/// back to. It falls back to [`Timestamp::default`] — the epoch, which
/// [`Timestamp::is_unset`] already reports as unset, so renderers treat it the same way they
/// treat a `None`.
///
/// This exists so that Go's zero time, `null`, `""`, and a malformed date all behave here the
/// way they behave everywhere else. A strict `Deserialize` would make one bad date in one row
/// fail the whole command.
pub fn lenient_timestamp<'de, D>(d: D) -> Result<Timestamp, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(gitea_core::types::opt_timestamp(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct U {
        #[serde(default, deserialize_with = "lenient_u64")]
        n: u64,
    }

    fn u(json: &str) -> u64 {
        serde_json::from_str::<U>(json).expect("lenient_u64 must never fail").n
    }

    #[test]
    fn a_plain_number_is_the_common_case() {
        assert_eq!(u(r#"{"n":42}"#), 42);
        assert_eq!(u(r#"{"n":18446744073709551615}"#), u64::MAX);
    }

    #[test]
    fn a_quoted_number_survives() {
        // Go marshals large uint64 as a string in some encoders, and a plain
        // `#[derive(Deserialize)]` u64 rejects `"42"` outright.
        assert_eq!(u(r#"{"n":"42"}"#), 42);
        assert_eq!(u(r#"{"n":" 42 "}"#), 42);
    }

    #[test]
    fn null_and_missing_are_zero_not_an_error() {
        assert_eq!(u(r#"{"n":null}"#), 0);
        assert_eq!(u(r#"{}"#), 0);
        assert_eq!(u(r#"{"n":""}"#), 0);
    }

    #[test]
    fn a_whole_float_is_an_integer() {
        // `1.0` is what a JSON encoder that went through a double produces.
        assert_eq!(u(r#"{"n":1.0}"#), 1);
        assert_eq!(u(r#"{"n":"3.0"}"#), 3);
    }

    #[test]
    fn a_negative_value_clamps_instead_of_failing() {
        // The bug this prevents: `-1` in a count field failing the whole response decode, so
        // the user loses their review comments over a field they never asked for.
        assert_eq!(u(r#"{"n":-1}"#), 0);
        assert_eq!(u(r#"{"n":"-7"}"#), 0);
    }

    #[test]
    fn nonsense_is_zero_plus_a_note() {
        assert_eq!(u(r#"{"n":"not a number"}"#), 0);
        assert_eq!(u(r#"{"n":true}"#), 0);
        assert_eq!(u(r#"{"n":1.5}"#), 0);
    }

    #[derive(Debug, Deserialize)]
    struct T {
        #[serde(default, deserialize_with = "lenient_timestamp")]
        t: Timestamp,
    }

    fn t(json: &str) -> Timestamp {
        serde_json::from_str::<T>(json).expect("lenient_timestamp must never fail").t
    }

    #[test]
    fn a_required_timestamp_tolerates_the_go_zero_time() {
        // `"0001-01-01T00:00:00Z"` is what Go's `time.Time` zero value marshals to, and Gitea
        // sends it for unset values. Parsed literally it renders as "2025 years ago".
        assert!(t(r#"{"t":"0001-01-01T00:00:00Z"}"#).is_unset());
        assert!(t(r#"{"t":null}"#).is_unset());
        assert!(t(r#"{"t":""}"#).is_unset());
        assert!(t(r#"{}"#).is_unset());
        assert!(t(r#"{"t":"not a date"}"#).is_unset());
    }

    #[test]
    fn a_real_required_timestamp_survives() {
        let ts = t(r#"{"t":"2026-09-12T10:30:00Z"}"#);
        assert!(!ts.is_unset());
        assert_eq!(ts.to_string(), "2026-09-12T10:30:00Z");
    }

    #[derive(Debug, Deserialize)]
    struct V {
        #[serde(default, deserialize_with = "null_as_empty_vec")]
        v: Vec<String>,
    }

    fn v(json: &str) -> Vec<String> {
        serde_json::from_str::<V>(json).expect("null_as_empty_vec must never fail").v
    }

    #[test]
    fn a_null_slice_is_an_empty_vec() {
        // Go marshals a nil slice as `null`. Without this, a derived `Vec<String>` answers
        // "invalid type: null, expected a sequence" and the whole response fails to decode.
        assert_eq!(v(r#"{"v":null}"#), Vec::<String>::new());
        assert_eq!(v(r#"{"v":[]}"#), Vec::<String>::new());
        assert_eq!(v(r#"{}"#), Vec::<String>::new());
        assert_eq!(v(r#"{"v":["a","b"]}"#), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn an_unassigned_issue_decodes() {
        // The actual wire shape this exists for, against the actual generated model: Gitea
        // sends `null` for every empty slice on an issue, and before `null_as_empty_vec` this
        // input failed to deserialize — so `gea issue list` lost every unassigned issue.
        let issue: crate::Issue = serde_json::from_str(
            r#"{"number":1,"title":"t","assignees":null,"labels":null,"assets":null}"#,
        )
        .expect("an issue with null slices must deserialize");
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
        assert_eq!(issue.title, "t");
    }

    #[derive(Debug, Deserialize)]
    struct M {
        #[serde(default, deserialize_with = "null_as_empty_map")]
        m: BTreeMap<String, String>,
    }

    fn m(json: &str) -> BTreeMap<String, String> {
        serde_json::from_str::<M>(json).expect("null_as_empty_map must never fail").m
    }

    #[derive(Debug, Deserialize)]
    struct S {
        #[serde(default, deserialize_with = "null_as_default")]
        s: String,
        #[serde(default, deserialize_with = "null_as_default")]
        n: i64,
        #[serde(default, deserialize_with = "null_as_default")]
        b: bool,
    }

    #[test]
    fn a_null_scalar_is_its_zero_value() {
        // Go marshals a nil pointer as `null`, and Gitea uses `*string` / `*int64` / `*bool`
        // for optional scalars throughout. Without this, one `null` fails the whole response.
        let s: S = serde_json::from_str(r#"{"s":null,"n":null,"b":null}"#)
            .expect("null_as_default must never fail on a null");
        assert_eq!(s.s, "");
        assert_eq!(s.n, 0);
        assert!(!s.b);
    }

    #[test]
    fn a_real_scalar_survives_untouched() {
        let s: S = serde_json::from_str(r#"{"s":"x","n":-7,"b":true}"#).unwrap();
        assert_eq!(s.s, "x");
        assert_eq!(s.n, -7);
        assert!(s.b);
    }

    #[test]
    fn a_wrong_type_is_still_an_error() {
        // Tolerating `null` must not turn into tolerating anything. A string where an `i64`
        // belongs is a real disagreement about the API, and swallowing it would hide a spec
        // bump that changed a field's type.
        let err = serde_json::from_str::<S>(r#"{"n":"12"}"#).unwrap_err();
        assert!(err.to_string().contains("invalid type: string"), "{err}");
    }

    #[test]
    fn an_open_pull_request_decodes() {
        // The wire shape this exists for, against the actual generated model. Gitea sends
        // `"merge_commit_sha": null` on an open pull request; before `null_as_default` that
        // failed to deserialize, and with it went `pr list`, `view`, `diff`, `status`, `checks`.
        let pr: crate::PullRequest = serde_json::from_str(
            r#"{"number":1,"title":"open pr","state":"open","merged":false,
                "merge_commit_sha":null,"merged_at":null,"merged_by":null}"#,
        )
        .expect("an open pull request must deserialize");
        assert_eq!(pr.merge_commit_sha, "");
        assert_eq!(pr.title, "open pr");
        assert_eq!(pr.merged_at, None);
    }

    #[test]
    fn a_null_map_is_an_empty_map() {
        assert!(m(r#"{"m":null}"#).is_empty());
        assert!(m(r#"{"m":{}}"#).is_empty());
        assert!(m(r#"{}"#).is_empty());
        assert_eq!(m(r#"{"m":{"a":"b"}}"#).get("a").map(String::as_str), Some("b"));
    }
}

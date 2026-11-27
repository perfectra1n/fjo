//! `--body-file` plus per-field flag overrides.
//!
//! The composition rule is deliberately the simplest one that is always sufficient: the file
//! supplies the **base JSON object**, and each body flag overwrites exactly one location in it,
//! addressed by the JSON pointer the generator recorded for that field.
//!
//! Why this and not something cleverer (a deep merge, or `gh api`'s `key[sub]=v` syntax)? 124
//! of the API's 125 request bodies are named definitions, so the generator can flatten depth-1
//! fields into flags — but not arrays of objects, and not deeper nesting. Those fields have no
//! flag at all, and `--body-file` is the only way to supply them. Pointer-overwrite means the
//! two mechanisms compose without either needing to understand the other: take a body you
//! already have, change one field on the command line. A deep merge would additionally have to
//! decide what merging two *arrays* means, and every answer to that surprises someone.

use std::io::Read;

use gitea_core::error::{Result, usage};
use serde_json::{Map, Value};

/// Read the base body. `-` reads stdin, which is why the reader is injected rather than
/// reached for directly — it keeps this testable and keeps the choice of "what is stdin" with
/// the binary.
pub fn load(path: &str, stdin: &mut dyn Read) -> Result<Value> {
    let text = if path == "-" {
        let mut s = String::new();
        stdin
            .read_to_string(&mut s)
            .map_err(|e| usage(format!("--body-file -: could not read stdin: {e}")))?;
        s
    } else {
        std::fs::read_to_string(path).map_err(|e| usage(format!("--body-file {path}: {e}")))?
    };
    // An empty file (or an empty stdin, which is what a forgotten pipe looks like) is treated
    // as an empty object rather than a parse error, so `--body-file -` with only flags works.
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&text).map_err(|e| {
        usage(format!(
            "--body-file {path} is not valid JSON: {e} (line {}, column {})",
            e.line(),
            e.column()
        ))
    })
}

/// Apply flag overrides to a base body.
///
/// A base that is not an object is passed through untouched when there is nothing to override
/// — the one inline request body in the spec, and any future array body, still work through
/// `--body-file` alone. It is only an error when a flag would have to reach *into* it, because
/// there is no sensible place to put `/title` in an array.
pub fn merge(mut base: Value, overrides: &[(&str, Value)]) -> Result<Value> {
    if overrides.is_empty() {
        return Ok(base);
    }
    if !base.is_object() {
        let flags: Vec<String> =
            overrides.iter().map(|(p, _)| format!("--{}", p.trim_start_matches('/'))).collect();
        return Err(usage(format!(
            "--body-file holds {} but {} must be set inside a JSON object — drop the flag, or give a body file whose top level is an object",
            kind_of(&base),
            flags.join(", ")
        )));
    }
    for (pointer, value) in overrides {
        set_pointer(&mut base, pointer, value.clone())?;
    }
    Ok(base)
}

/// Set one location, creating missing intermediate objects on the way.
///
/// `serde_json`'s own `pointer_mut` returns `None` when an intermediate is absent, which is
/// exactly the case a flag like `--repo-owner` (pointer `/repo/owner`) hits against an empty
/// base. Creating the intermediates is the whole reason this exists.
pub fn set_pointer(root: &mut Value, pointer: &str, value: Value) -> Result<()> {
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(usage(format!("{pointer:?} is not a JSON pointer: it must start with '/'")));
    };
    let tokens: Vec<String> = rest.split('/').map(unescape).collect();
    let (last, parents) = tokens.split_last().expect("split always yields one token");

    let mut cur = root;
    let mut so_far = String::new();
    for token in parents {
        so_far.push('/');
        so_far.push_str(token);
        let map = as_object_mut(cur, pointer, &so_far)?;
        cur = map.entry(token.clone()).or_insert_with(|| Value::Object(Map::new()));
    }
    so_far.push('/');
    so_far.push_str(last);
    as_object_mut(cur, pointer, &so_far)?.insert(last.clone(), value);
    Ok(())
}

fn as_object_mut<'v>(
    v: &'v mut Value,
    pointer: &str,
    at: &str,
) -> Result<&'v mut Map<String, Value>> {
    let kind = kind_of(v);
    v.as_object_mut().ok_or_else(|| {
        // Naming *which* prefix is the wrong shape is the difference between a fixable message
        // and "invalid body".
        let prefix = at.rsplit_once('/').map_or("", |(p, _)| p);
        let prefix = if prefix.is_empty() { "the body" } else { prefix };
        usage(format!("cannot set {pointer} in the --body-file: {prefix} is {kind}, not an object"))
    })
}

/// JSON pointer escaping, unescaped in the order RFC 6901 requires: `~1` before `~0`, or
/// `~01` would decode to `/` instead of `~1`.
fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn stdin(s: &str) -> Cursor<Vec<u8>> {
        Cursor::new(s.as_bytes().to_vec())
    }

    #[test]
    fn a_dash_reads_the_base_object_from_stdin() {
        let v = load("-", &mut stdin(r#"{"title":"from stdin","draft":true}"#)).unwrap();
        assert_eq!(v, json!({"title": "from stdin", "draft": true}));
    }

    #[test]
    fn empty_stdin_is_an_empty_object_not_a_parse_error() {
        assert_eq!(load("-", &mut stdin("   \n")).unwrap(), json!({}));
    }

    #[test]
    fn invalid_json_names_the_line_and_column() {
        let e = load("-", &mut stdin("{oops}")).unwrap_err().to_string();
        assert!(e.contains("not valid JSON"), "{e}");
        assert!(e.contains("column"), "{e}");
    }

    #[test]
    fn a_missing_file_says_which_path() {
        let e = load("/nonexistent/body.json", &mut stdin("")).unwrap_err().to_string();
        assert!(e.contains("/nonexistent/body.json"), "{e}");
    }

    #[test]
    fn flags_override_the_file_field_by_field() {
        let base = load("-", &mut stdin(r#"{"title":"old","base":"main"}"#)).unwrap();
        let merged = merge(base, &[("/title", json!("new"))]).unwrap();
        assert_eq!(merged, json!({"title": "new", "base": "main"}));
    }

    /// The case `serde_json::pointer_mut` cannot do: `/repo/owner` against a base that has no
    /// `repo` at all.
    #[test]
    fn a_pointer_creates_missing_intermediate_objects() {
        let mut v = json!({});
        set_pointer(&mut v, "/repo/owner", json!("perf3ct")).unwrap();
        set_pointer(&mut v, "/repo/name", json!("gea")).unwrap();
        set_pointer(&mut v, "/a/b/c", json!(1)).unwrap();
        assert_eq!(v, json!({"repo": {"owner": "perf3ct", "name": "gea"}, "a": {"b": {"c": 1}}}));
    }

    #[test]
    fn an_intermediate_of_the_wrong_shape_names_the_prefix() {
        let mut v = json!({"repo": "not-an-object"});
        let e = set_pointer(&mut v, "/repo/owner", json!("x")).unwrap_err().to_string();
        assert!(e.contains("/repo is a string"), "{e}");
    }

    /// A non-object body file is fine on its own — but a flag has nowhere to go inside it, and
    /// silently ignoring the flag would be the worst possible answer.
    #[test]
    fn a_non_object_body_file_passes_through_alone_but_rejects_overrides() {
        let base = load("-", &mut stdin("[1, 2, 3]")).unwrap();
        assert_eq!(merge(base.clone(), &[]).unwrap(), json!([1, 2, 3]));
        let e = merge(base, &[("/title", json!("x"))]).unwrap_err().to_string();
        assert!(e.contains("an array"), "{e}");
        assert!(e.contains("--title"), "{e}");
    }

    #[test]
    fn a_pointer_must_be_a_pointer() {
        let mut v = json!({});
        let e = set_pointer(&mut v, "title", json!("x")).unwrap_err().to_string();
        assert!(e.contains("start with '/'"), "{e}");
    }

    #[test]
    fn escaped_pointer_tokens_round_trip() {
        let mut v = json!({});
        set_pointer(&mut v, "/a~1b", json!(1)).unwrap();
        set_pointer(&mut v, "/c~0d", json!(2)).unwrap();
        assert_eq!(v, json!({"a/b": 1, "c~d": 2}));
    }
}

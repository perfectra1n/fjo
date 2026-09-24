//! `-f/--raw-field` and `-F/--field`, and the `key[sub]=v` / `key[]=v` shapes.
//!
//! # The letters look backwards, and they are `gh`'s
//!
//! `-f/--raw-field` is the **string** form and `-F/--field` is the **typed** form. Read as
//! English that is inverted — "raw" sounds like the one that gets special treatment, and the
//! capital letter looks like the emphatic version of the lowercase one. It is nonetheless
//! exactly what `gh api` does, and anyone reaching for `gea api` has `gh api` in their
//! fingers. Matching a confusing convention beats being the only tool where a copied command
//! line silently sends `"true"` instead of `true`.
//!
//! `-F` additionally reads a file when the value begins with `@`, with `@-` meaning stdin. That
//! is also `gh`'s.
//!
//! # Key syntax
//!
//! | written | meaning |
//! | --- | --- |
//! | `title=hi` | `{"title": "hi"}` |
//! | `head[repo]=x` | `{"head": {"repo": "x"}}` |
//! | `labels[]=1` `labels[]=2` | `{"labels": ["1", "2"]}` |
//! | `labels[]` | `{"labels": []}` — the only form with no `=` |
//!
//! `labels[]` without a value exists because "clear this array" is otherwise unexpressible: an
//! empty `labels=` sends an empty *string*, and omitting the key entirely leaves the server's
//! current value alone.

use std::io::Read;

use gitea_core::error::{Error, ErrorKind, Result};
use serde_json::{Map, Value};

/// Which flag a field came from, i.e. whether its value is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typing {
    /// `-f/--raw-field`: the value is a string, always, even if it looks like a number.
    Raw,
    /// `-F/--field`: `true`/`false`/`null` and integers become JSON types, and a leading `@`
    /// reads a file.
    Typed,
}

/// One parsed `key=value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The key as written, brackets included. Used verbatim as a query-string key, which is the
    /// convention every web framework's array/nested-parameter parser already understands.
    pub key: String,
    pub value: Value,
}

/// Parse the `-f` and `-F` arguments **in the order they were given**, since `labels[]=a
/// labels[]=b` depends on it.
pub fn parse(specs: &[(Typing, String)], stdin: &mut dyn Read) -> Result<Vec<Field>> {
    let mut out = Vec::with_capacity(specs.len());
    for (typing, spec) in specs {
        out.push(parse_one(*typing, spec, stdin)?);
    }
    Ok(out)
}

fn parse_one(typing: Typing, spec: &str, stdin: &mut dyn Read) -> Result<Field> {
    let Some((key, raw)) = spec.split_once('=') else {
        // The one valueless form.
        if spec.ends_with("[]") {
            return Ok(Field { key: spec.to_owned(), value: Value::Array(Vec::new()) });
        }
        return Err(usage(format!(
            "{spec:?} is not a field: write it as key=value (or key[] for an empty array)"
        )));
    };
    if key.is_empty() {
        return Err(usage(format!("{spec:?} has no field name before the '='")));
    }
    let value = match typing {
        Typing::Raw => Value::String(raw.to_owned()),
        Typing::Typed => typed_value(key, raw, stdin)?,
    };
    Ok(Field { key: key.to_owned(), value })
}

/// `-F`'s value rules, in `gh`'s order: `@file` first, then the JSON literals, then a string.
fn typed_value(key: &str, raw: &str, stdin: &mut dyn Read) -> Result<Value> {
    if let Some(path) = raw.strip_prefix('@') {
        // File contents are a *string*, not parsed as JSON: `-F body=@notes.md` is the point of
        // this feature, and guessing that a file starting with `[` is an array would corrupt a
        // markdown body that happens to begin with a link.
        return Ok(Value::String(read_file(key, path, stdin)?));
    }
    Ok(match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        // Integers only, matching `gh`. `1.0` stays a string because a float that round-trips
        // through a JSON number can come back as `1` and change meaning.
        _ => match raw.parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(raw.to_owned()),
        },
    })
}

fn read_file(key: &str, path: &str, stdin: &mut dyn Read) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        stdin
            .read_to_string(&mut s)
            .map_err(|e| usage(format!("--field {key}=@-: could not read stdin: {e}")))?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| usage(format!("--field {key}=@{path}: {e}")))
}

/// Fold parsed fields into one JSON object, honouring the bracket syntax.
pub fn to_body(fields: &[Field]) -> Result<Value> {
    let mut root = Value::Object(Map::new());
    for f in fields {
        let (name, segments) = split_key(&f.key)?;
        insert(&mut root, &name, &segments, f.value.clone(), &f.key)?;
    }
    Ok(root)
}

/// Fields as query-string pairs.
///
/// The key keeps its brackets, and the value is flattened to text — which is the only thing a
/// query string can carry. `null` becomes empty rather than the four letters `null`, because
/// `?state=null` would filter on the literal string.
pub fn to_query(fields: &[Field]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for f in fields {
        match &f.value {
            Value::Array(items) if items.is_empty() => out.push((f.key.clone(), String::new())),
            Value::Array(items) => {
                for item in items {
                    out.push((f.key.clone(), scalar_text(item)));
                }
            }
            other => out.push((f.key.clone(), scalar_text(other))),
        }
    }
    out
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A bracket path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    /// `[]`: append to an array.
    Push,
}

/// `head[repo][name]` → `("head", [Key("repo"), Key("name")])`.
fn split_key(key: &str) -> Result<(String, Vec<Seg>)> {
    let open = match key.find('[') {
        None => return Ok((key.to_owned(), Vec::new())),
        Some(i) => i,
    };
    let name = key[..open].to_owned();
    if name.is_empty() {
        return Err(usage(format!("{key:?} has no field name before the '['")));
    }
    let mut segs = Vec::new();
    let mut rest = &key[open..];
    while !rest.is_empty() {
        let Some(close) = rest.find(']') else {
            return Err(usage(format!("{key:?} has an unclosed '['")));
        };
        if !rest.starts_with('[') {
            return Err(usage(format!("{key:?}: expected '[' after ']'")));
        }
        let inner = &rest[1..close];
        segs.push(if inner.is_empty() { Seg::Push } else { Seg::Key(inner.to_owned()) });
        rest = &rest[close + 1..];
    }
    Ok((name, segs))
}

fn insert(root: &mut Value, name: &str, segments: &[Seg], value: Value, key: &str) -> Result<()> {
    let obj = root.as_object_mut().expect("root is constructed as an object");
    match segments.split_first() {
        // A plain `key=value`. A repeat overwrites, matching `gh`: the last one wins, and
        // collecting repeats into an array silently would make `-f title=a -f title=b` send a
        // list where the server wants a string.
        None => {
            obj.insert(name.to_owned(), value);
            Ok(())
        }
        Some((Seg::Push, [])) => {
            let slot = obj.entry(name.to_owned()).or_insert_with(|| Value::Array(Vec::new()));
            match slot {
                Value::Array(items) => {
                    // `labels[]` (an empty array) followed by `labels[]=1` should mean the list
                    // `[1]`, not `[[], 1]`.
                    if let Value::Array(more) = value {
                        items.extend(more);
                    } else {
                        items.push(value);
                    }
                    Ok(())
                }
                _ => Err(conflict(key, name)),
            }
        }
        Some((Seg::Key(k), rest)) => {
            let slot = obj.entry(name.to_owned()).or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                return Err(conflict(key, name));
            }
            insert(slot, k, rest, value, key)
        }
        // `labels[][x]=1` — an array of objects. Reachable, but the only sane reading needs an
        // index, and inventing one would silently attach the value to whichever element came
        // last.
        Some((Seg::Push, _)) => Err(usage(format!(
            "{key:?} indexes into an array element, which --field cannot express; \
             use --input <file> with the body you want"
        ))),
    }
}

fn conflict(key: &str, name: &str) -> Error {
    usage(format!("{key:?} conflicts with an earlier field that set {name:?} to a different shape"))
}

fn usage(msg: String) -> Error {
    Error::new(ErrorKind::Usage(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(specs: &[(Typing, &str)]) -> Value {
        let owned: Vec<(Typing, String)> =
            specs.iter().map(|(t, s)| (*t, (*s).to_owned())).collect();
        let fields = parse(&owned, &mut std::io::empty()).unwrap();
        to_body(&fields).unwrap()
    }

    /// The whole reason both flags exist, and the bug the inverted letters cause: a copied
    /// `gh api -f` command line must send strings and `-F` must send JSON types.
    #[test]
    fn lowercase_f_is_strings_and_uppercase_f_is_typed() {
        assert_eq!(
            body(&[(Typing::Raw, "draft=true"), (Typing::Raw, "milestone=3")]),
            json!({"draft": "true", "milestone": "3"})
        );
        assert_eq!(
            body(&[
                (Typing::Typed, "draft=true"),
                (Typing::Typed, "milestone=3"),
                (Typing::Typed, "closed=false"),
                (Typing::Typed, "due=null"),
                (Typing::Typed, "title=3 blind mice"),
            ]),
            json!({"draft": true, "milestone": 3, "closed": false, "due": null,
                   "title": "3 blind mice"})
        );
    }

    /// A float stays a string: sending `1.0` as a JSON number can come back as `1`.
    #[test]
    fn only_integers_become_numbers() {
        assert_eq!(body(&[(Typing::Typed, "n=1.5")]), json!({"n": "1.5"}));
        assert_eq!(body(&[(Typing::Typed, "n=-12")]), json!({"n": -12}));
        assert_eq!(body(&[(Typing::Typed, "n=007")]), json!({"n": 7}));
    }

    #[test]
    fn brackets_nest_and_repeat() {
        assert_eq!(
            body(&[(Typing::Raw, "head[repo][name]=x"), (Typing::Raw, "head[ref]=main")]),
            json!({"head": {"repo": {"name": "x"}, "ref": "main"}})
        );
        assert_eq!(
            body(&[(Typing::Typed, "labels[]=1"), (Typing::Typed, "labels[]=2")]),
            json!({"labels": [1, 2]})
        );
    }

    /// "Clear this list" is otherwise unexpressible: `labels=` sends an empty string and
    /// omitting the key leaves the server's value alone.
    #[test]
    fn a_bare_bracket_pair_is_an_empty_array() {
        assert_eq!(body(&[(Typing::Raw, "labels[]")]), json!({"labels": []}));
        // …and it composes with values that follow it.
        assert_eq!(
            body(&[(Typing::Raw, "labels[]"), (Typing::Typed, "labels[]=4")]),
            json!({"labels": [4]})
        );
    }

    #[test]
    fn at_reads_a_file_as_a_string_not_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.md");
        std::fs::write(&path, "[a link](x) and more").unwrap();
        let spec = format!("body=@{}", path.display());
        let fields = parse(&[(Typing::Typed, spec)], &mut std::io::empty()).unwrap();
        assert_eq!(to_body(&fields).unwrap(), json!({"body": "[a link](x) and more"}));
    }

    #[test]
    fn at_dash_reads_stdin() {
        let mut stdin = std::io::Cursor::new(b"from stdin".to_vec());
        let fields = parse(&[(Typing::Typed, "body=@-".to_owned())], &mut stdin).unwrap();
        assert_eq!(to_body(&fields).unwrap(), json!({"body": "from stdin"}));
    }

    #[test]
    fn a_missing_equals_says_what_to_write() {
        let e = parse(&[(Typing::Raw, "title".to_owned())], &mut std::io::empty()).unwrap_err();
        assert!(e.to_string().contains("key=value"), "{e}");
        assert_eq!(e.exit_code(), 2);
    }

    /// Repeated query keys are legal and load-bearing in this API, so the query form must keep
    /// every value rather than folding them into a map.
    #[test]
    fn query_pairs_keep_repeats_and_order() {
        let owned = [
            (Typing::Raw, "labels=bug".to_owned()),
            (Typing::Raw, "labels=ci".to_owned()),
            (Typing::Typed, "draft=true".to_owned()),
        ];
        let fields = parse(&owned, &mut std::io::empty()).unwrap();
        assert_eq!(
            to_query(&fields),
            vec![
                ("labels".to_owned(), "bug".to_owned()),
                ("labels".to_owned(), "ci".to_owned()),
                ("draft".to_owned(), "true".to_owned()),
            ]
        );
    }

    #[test]
    fn nested_keys_keep_their_brackets_in_a_query_string() {
        let fields =
            parse(&[(Typing::Raw, "head[ref]=main".to_owned())], &mut std::io::empty()).unwrap();
        assert_eq!(to_query(&fields), vec![("head[ref]".to_owned(), "main".to_owned())]);
    }

    #[test]
    fn a_shape_conflict_is_reported_rather_than_silently_dropped() {
        let owned = [(Typing::Raw, "a=1".to_owned()), (Typing::Raw, "a[b]=2".to_owned())];
        let fields = parse(&owned, &mut std::io::empty()).unwrap();
        let e = to_body(&fields).unwrap_err();
        assert!(e.to_string().contains("conflicts"), "{e}");
    }

    #[test]
    fn malformed_brackets_are_named() {
        for bad in ["a[b=1", "[b]=1"] {
            let fields =
                parse(&[(Typing::Raw, bad.to_owned())], &mut std::io::empty()).unwrap_or_default();
            let e = to_body(&fields)
                .err()
                .or_else(|| parse(&[(Typing::Raw, bad.to_owned())], &mut std::io::empty()).err());
            assert!(e.is_some(), "{bad} should not parse");
        }
    }
}

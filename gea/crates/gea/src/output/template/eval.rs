//! Tree-walking evaluator for the `--template` language.
//!
//! # Missing data is never an error
//!
//! `{{.merged_by.login}}` on an unmerged pull request yields the empty string, not a
//! diagnostic. A CLI template runs over whatever the server sent, across a hundred rows, and
//! aborting the whole render because row 43 lacks an optional field would make templates
//! unusable against a real instance. Go's `text/template` prints `<no value>` here; the empty
//! string is better, because it is what you want in a table cell.
//!
//! Errors are reserved for mistakes in the *template*: an unknown function, a bad arity, an
//! unsupported `timefmt` layout. Those are the author's bugs, and they are the same on every
//! row.

use gitea_core::Error;
use serde_json::Value;

use super::funcs;
use super::lex::template_err;
use super::parse::{Expr, Node};
use crate::output::table::Table;
use crate::output::tty::Term;

/// Evaluation state for one render.
pub struct Ctx<'a> {
    pub term: &'a Term,
    /// The value `$` refers to, fixed for the whole render regardless of `range` nesting.
    root: Value,
    /// A stack, not a map: `range` pushes bindings and pops them at `{{end}}`, so an inner
    /// `$i` shadows an outer one and does not leak past the loop.
    vars: Vec<(String, Value)>,
    /// The `tablerow` buffer. Shared with [`Table`] so `tablerender` and the default human
    /// renderer lay out identically.
    pub table: Table,
}

impl<'a> Ctx<'a> {
    pub fn new(root: Value, term: &'a Term) -> Self {
        Self { term, root, vars: Vec::new(), table: Table::new(term) }
    }

    /// Render the buffered rows and clear the buffer.
    pub fn flush_table(&mut self) -> String {
        if self.table.is_empty() {
            return String::new();
        }
        let rendered = self.table.render_to_string();
        self.table = Table::new(self.term);
        rendered
    }
}

/// Render a parsed template.
///
/// # Auto-flush
///
/// If the template ends with rows still buffered, they are flushed automatically. Forgetting
/// `{{tablerender}}` is *the* most common `--template` mistake, and its failure mode is the
/// worst possible one: the command exits 0 and prints nothing, so it looks like the API
/// returned no results. `gh` auto-flushes for exactly this reason.
pub fn render(nodes: &[Node], data: &Value, term: &Term) -> Result<String, Error> {
    let mut ctx = Ctx::new(data.clone(), term);
    let mut out = String::new();
    let dot = data.clone();
    walk(&mut ctx, nodes, &dot, &mut out)?;
    out.push_str(&ctx.flush_table());
    Ok(out)
}

fn walk(ctx: &mut Ctx, nodes: &[Node], dot: &Value, out: &mut String) -> Result<(), Error> {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Emit { expr, .. } => {
                let v = eval(ctx, expr, dot)?;
                out.push_str(&to_text(&v));
            }
            Node::Assign { name, value, .. } => {
                let v = eval(ctx, value, dot)?;
                ctx.vars.push((name.clone(), v));
            }
            Node::If { cond, then, otherwise, .. } => {
                let v = eval(ctx, cond, dot)?;
                let depth = ctx.vars.len();
                let branch = if truthy(&v) { then } else { otherwise };
                walk(ctx, branch, dot, out)?;
                ctx.vars.truncate(depth);
            }
            Node::Range { over, key, val, body, otherwise, .. } => {
                let subject = eval(ctx, over, dot)?;
                let pairs = iterate(&subject);
                if pairs.is_empty() {
                    let depth = ctx.vars.len();
                    walk(ctx, otherwise, dot, out)?;
                    ctx.vars.truncate(depth);
                    continue;
                }
                for (k, v) in pairs {
                    let depth = ctx.vars.len();
                    if let Some(name) = key {
                        ctx.vars.push((name.clone(), k));
                    }
                    if let Some(name) = val {
                        ctx.vars.push((name.clone(), v.clone()));
                    }
                    walk(ctx, body, &v, out)?;
                    ctx.vars.truncate(depth);
                }
            }
        }
    }
    Ok(())
}

fn eval(ctx: &mut Ctx, expr: &Expr, dot: &Value) -> Result<Value, Error> {
    match expr {
        Expr::Root => Ok(ctx.root.clone()),
        Expr::Field(path) => Ok(lookup(dot, path)),
        // An undefined variable resolves to null, and therefore to the empty string. A
        // template that references `$v` outside its `range` is a mistake, but so is failing a
        // hundred-row render over it.
        Expr::Var(name) => Ok(ctx
            .vars
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map_or(Value::Null, |(_, v)| v.clone())),
        Expr::Access { base, path } => {
            let value = eval(ctx, base, dot)?;
            Ok(lookup(&value, path))
        }
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Call { name, args, line } => {
            let mut values = Vec::with_capacity(args.len());
            for a in args {
                values.push(eval(ctx, a, dot)?);
            }
            funcs::call(ctx, name, &values, *line)
        }
        Expr::Pipe(lhs, rhs) => {
            let piped = eval(ctx, lhs, dot)?;
            match &**rhs {
                Expr::Call { name, args, line } => {
                    let mut values = Vec::with_capacity(args.len() + 1);
                    for a in args {
                        values.push(eval(ctx, a, dot)?);
                    }
                    // Go's rule: the piped value becomes the *last* argument, which is what
                    // makes `.title | truncate 30` read as `truncate 30 .title`.
                    values.push(piped);
                    funcs::call(ctx, name, &values, *line)
                }
                other => Err(template_err(
                    line_of(other),
                    "the right side of `|` must be a function".to_string(),
                )),
            }
        }
    }
}

fn line_of(e: &Expr) -> usize {
    match e {
        Expr::Call { line, .. } => *line,
        _ => 0,
    }
}

/// Walk a `.a.b` path. A numeric segment indexes an array, so `.assets.0.name` works without
/// an `index` function.
fn lookup(value: &Value, path: &[String]) -> Value {
    let mut cur = value;
    for segment in path {
        cur = match cur {
            Value::Object(map) => match map.get(segment) {
                Some(v) => v,
                None => return Value::Null,
            },
            Value::Array(items) => match segment.parse::<usize>().ok().and_then(|i| items.get(i)) {
                Some(v) => v,
                None => return Value::Null,
            },
            _ => return Value::Null,
        };
    }
    cur.clone()
}

/// Go's `if` truthiness: the zero value of each kind is false. An empty array or object is
/// false, which is what makes `{{if .labels}}` do the obvious thing.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `(key, value)` pairs for `range`. Arrays yield integer indices, objects yield their keys in
/// document order (`serde_json`'s `preserve_order` feature keeps it). Anything else yields
/// nothing, so `{{range .not_a_list}}` is empty rather than an error.
fn iterate(v: &Value) -> Vec<(Value, Value)> {
    match v {
        Value::Array(items) => {
            items.iter().enumerate().map(|(i, v)| (Value::from(i), v.clone())).collect()
        }
        Value::Object(map) => {
            map.iter().map(|(k, v)| (Value::from(k.clone()), v.clone())).collect()
        }
        _ => Vec::new(),
    }
}

/// Render a JSON value as template output.
///
/// `null` becomes the empty string — the same rule as a missing field, because after
/// `--json` projection an absent field *is* an explicit `null`. Composite values fall back to
/// compact JSON rather than erroring, so `{{.head}}` shows you the object you forgot to reach
/// into instead of failing.
pub fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::template::parse::parse;
    use serde_json::json;

    fn r(src: &str, data: Value) -> String {
        render(&parse(src).expect(src), &data, &Term::tty(80)).expect(src)
    }

    /// Bug this prevents: a missing or null field aborting the render. Over a hundred rows,
    /// one absent `merged_by` would take out the whole command.
    #[test]
    fn missing_fields_are_empty_strings_not_errors() {
        let d = json!({"a": {"b": 1}});
        assert_eq!(r("[{{.nope}}]", d.clone()), "[]");
        assert_eq!(r("[{{.a.nope}}]", d.clone()), "[]");
        assert_eq!(r("[{{.a.b.c.d}}]", d.clone()), "[]");
        assert_eq!(r("[{{.nope.deeper}}]", d.clone()), "[]");
        assert_eq!(r("[{{$nosuchvar}}]", d), "[]");
        assert_eq!(r("[{{.x}}]", json!({"x": null})), "[]");
    }

    #[test]
    fn scalars_and_composites_render() {
        assert_eq!(
            r("{{.n}} {{.b}} {{.s}} {{.o}}", json!({"n":1,"b":true,"s":"x","o":{"k":1}})),
            "1 true x {\"k\":1}"
        );
    }

    /// Bug this prevents: `$` being rebound by `range`, so a template cannot reach the
    /// top-level document from inside a loop.
    #[test]
    fn dollar_is_the_root_even_inside_range() {
        let d = json!({"repo": "o/r", "items": [{"n": 1}, {"n": 2}]});
        assert_eq!(r("{{range .items}}{{$.repo}}#{{.n}} {{end}}", d), "o/r#1 o/r#2 ");
    }

    /// Bug this prevents: `range` variables leaking past `{{end}}` and shadowing an outer
    /// binding for the rest of the template.
    #[test]
    fn range_variables_are_scoped() {
        let d = json!({"a": [1, 2], "b": [3]});
        assert_eq!(
            r("{{range $v := .a}}{{$v}}{{end}}|{{range $v := .b}}{{$v}}{{end}}|{{$v}}", d),
            "12|3|"
        );
    }

    #[test]
    fn range_with_index_and_element() {
        let d = json!(["a", "b"]);
        assert_eq!(r("{{range $i, $v := .}}{{$i}}={{$v}} {{end}}", d), "0=a 1=b ");
    }

    #[test]
    fn range_over_an_object_uses_document_order() {
        let d = json!({"z": 1, "a": 2});
        assert_eq!(r("{{range $k, $v := .}}{{$k}}:{{$v}} {{end}}", d), "z:1 a:2 ");
    }

    /// Bug this prevents: `{{range .missing}}` erroring instead of producing nothing, or the
    /// `{{else}}` branch never firing on an empty list.
    #[test]
    fn range_else_covers_the_empty_case() {
        assert_eq!(r("{{range .x}}y{{else}}none{{end}}", json!({"x": []})), "none");
        assert_eq!(r("{{range .x}}y{{else}}none{{end}}", json!({})), "none");
        assert_eq!(r("{{range .x}}y{{else}}none{{end}}", json!({"x": 7})), "none");
    }

    /// Bug this prevents: Go's zero-value truthiness being replaced with "non-null is true",
    /// so `{{if .labels}}` prints a header above an empty list.
    #[test]
    fn truthiness_follows_go() {
        for (v, want) in [
            (json!(null), false),
            (json!(false), false),
            (json!(true), true),
            (json!(0), false),
            (json!(0.0), false),
            (json!(1), true),
            (json!(""), false),
            (json!("x"), true),
            (json!([]), false),
            (json!([0]), true),
            (json!({}), false),
            (json!({"a":1}), true),
        ] {
            assert_eq!(truthy(&v), want, "{v}");
        }
    }

    /// Bug this prevents: piping into a field or literal being silently accepted and producing
    /// nothing, instead of naming the mistake.
    #[test]
    fn piping_into_a_non_function_is_an_error() {
        let err = render(&parse("{{.a | .b}}").unwrap(), &json!({}), &Term::piped()).unwrap_err();
        assert!(matches!(&*err.kind, gitea_core::ErrorKind::Template { .. }));
    }

    #[test]
    fn assignment_outside_range() {
        assert_eq!(r("{{$n := .n}}{{$n}}-{{$n}}", json!({"n": 5})), "5-5");
    }
}

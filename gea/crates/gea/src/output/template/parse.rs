//! Parser for the `--template` language.
//!
//! The grammar is Go's, cut down to the subset `gh` documents:
//!
//! ```text
//! template := (text | action)*
//! action   := '{{' (control | assign | pipeline) '}}'
//! control  := 'if' pipeline | 'range' [decls ':='] pipeline | 'else' | 'end'
//! assign   := '$name' ':=' pipeline
//! pipeline := command ('|' command)*
//! command  := operand operand*            -- operand[0] is the callee when it is an ident
//! operand  := '.' path | '$' name | string | number | ident | '(' pipeline ')'
//! ```
//!
//! No `with`, no `template`/`define`/`block`, no `printf`, no comparison operators. Every one
//! of those is a feature that a `--jq` expression already covers, and each would need its own
//! documentation and its own edge cases. A Go-template *crate* would give us all of it plus
//! reflection semantics we cannot express over `serde_json::Value`.

use serde_json::Value;

use super::lex::{Item, Token, lex, template_err};
use gitea_core::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Text(String),
    /// Evaluate and print.
    Emit {
        expr: Expr,
        line: usize,
    },
    If {
        cond: Expr,
        then: Vec<Node>,
        otherwise: Vec<Node>,
        line: usize,
    },
    Range {
        over: Expr,
        key: Option<String>,
        val: Option<String>,
        body: Vec<Node>,
        otherwise: Vec<Node>,
        line: usize,
    },
    Assign {
        name: String,
        value: Expr,
        line: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// `$` — the value the template was invoked with, regardless of `range` nesting.
    Root,
    /// `.` / `.a.b`, relative to the current dot.
    Field(Vec<String>),
    Var(String),
    /// `$.a.b` / `$v.a.b` — a path applied to something other than the current dot.
    Access {
        base: Box<Expr>,
        path: Vec<String>,
    },
    Lit(Value),
    Call {
        name: String,
        args: Vec<Expr>,
        line: usize,
    },
    /// `lhs | rhs`. `rhs` must be a call; the value of `lhs` becomes its final argument, which
    /// is Go's rule and the reason `{{.title | truncate 30}}` reads the way it does.
    Pipe(Box<Expr>, Box<Expr>),
}

pub fn parse(src: &str) -> Result<Vec<Node>, Error> {
    let items = lex(src)?;
    let mut p = Parser { items: &items, pos: 0 };
    let (nodes, terminator) = p.block()?;
    match terminator {
        Some((kw, line)) => {
            Err(template_err(line, format!("`{kw}` without a matching `if` or `range`")))
        }
        None => Ok(nodes),
    }
}

struct Parser<'a> {
    items: &'a [Item],
    pos: usize,
}

/// How a block ended: at `{{end}}`, at `{{else}}`, or at the end of the template.
type Terminator = Option<(&'static str, usize)>;

impl Parser<'_> {
    fn block(&mut self) -> Result<(Vec<Node>, Terminator), Error> {
        let mut nodes = Vec::new();
        while self.pos < self.items.len() {
            match &self.items[self.pos] {
                Item::Text(t) => {
                    nodes.push(Node::Text(t.clone()));
                    self.pos += 1;
                }
                Item::Action { tokens, line } => {
                    let line = *line;
                    self.pos += 1;
                    match tokens.first() {
                        None => {
                            // `{{}}` is a no-op, not an error; it shows up in generated
                            // templates where a conditional produced nothing.
                        }
                        Some(Token::Ident(kw)) if kw == "end" => {
                            return Ok((nodes, Some(("end", line))));
                        }
                        Some(Token::Ident(kw)) if kw == "else" => {
                            return Ok((nodes, Some(("else", line))));
                        }
                        Some(Token::Ident(kw)) if kw == "if" => {
                            nodes.push(self.control(tokens, line, false)?);
                        }
                        Some(Token::Ident(kw)) if kw == "range" => {
                            nodes.push(self.control(tokens, line, true)?);
                        }
                        _ => {
                            if let Some(node) = assignment(tokens, line)? {
                                nodes.push(node);
                            } else {
                                nodes.push(Node::Emit { expr: pipeline(tokens, line)?, line });
                            }
                        }
                    }
                }
            }
        }
        Ok((nodes, None))
    }

    fn control(&mut self, tokens: &[Token], line: usize, is_range: bool) -> Result<Node, Error> {
        let kw = if is_range { "range" } else { "if" };
        let rest = &tokens[1..];
        if rest.is_empty() {
            return Err(template_err(line, format!("`{kw}` needs an expression")));
        }

        let (key, val, expr_tokens) =
            if is_range { range_decls(rest, line)? } else { (None, None, rest) };
        let head = pipeline(expr_tokens, line)?;

        let (body, terminator) = self.block()?;
        let (otherwise, terminator) = match terminator {
            Some(("else", _)) => {
                let (e, t) = self.block()?;
                (e, t)
            }
            other => (Vec::new(), other),
        };
        match terminator {
            Some(("end", _)) => {}
            _ => {
                return Err(template_err(line, format!("`{kw}` without a matching `{{{{end}}}}`")));
            }
        }

        Ok(if is_range {
            Node::Range { over: head, key, val, body, otherwise, line }
        } else {
            Node::If { cond: head, then: body, otherwise, line }
        })
    }
}

/// Split `range` variable declarations off the front: `$i, $v := …` or `$v := …`.
///
/// With one variable Go binds the **element**; with two it binds index and element. Matching
/// that is the point — a user copying a `gh` template must not silently get the index where
/// they expected the value.
#[allow(clippy::type_complexity)]
fn range_decls(
    rest: &[Token],
    line: usize,
) -> Result<(Option<String>, Option<String>, &[Token]), Error> {
    let define = rest.iter().position(|t| *t == Token::Define);
    let Some(define) = define else {
        return Ok((None, None, rest));
    };
    let mut names = Vec::new();
    for token in &rest[..define] {
        match token {
            Token::Var(name, path) if !name.is_empty() && path.is_empty() => {
                names.push(name.clone());
            }
            Token::Comma => {}
            other => {
                return Err(template_err(
                    line,
                    format!("`range` variables must be `$name`, got {other:?}"),
                ));
            }
        }
    }
    let tail = &rest[define + 1..];
    match names.len() {
        1 => Ok((None, Some(names.remove(0)), tail)),
        2 => {
            let mut it = names.into_iter();
            Ok((it.next(), it.next(), tail))
        }
        _ => Err(template_err(line, "`range` declares one or two variables")),
    }
}

/// `{{$x := pipeline}}` — returns `None` when the action is not an assignment.
fn assignment(tokens: &[Token], line: usize) -> Result<Option<Node>, Error> {
    match (tokens.first(), tokens.get(1)) {
        (Some(Token::Var(name, path)), Some(Token::Define))
            if !name.is_empty() && path.is_empty() =>
        {
            let value = pipeline(&tokens[2..], line)?;
            Ok(Some(Node::Assign { name: name.clone(), value, line }))
        }
        _ => Ok(None),
    }
}

fn pipeline(tokens: &[Token], line: usize) -> Result<Expr, Error> {
    let mut c = Cursor { tokens, pos: 0, line };
    let expr = c.pipeline()?;
    if c.pos < tokens.len() {
        return Err(template_err(line, format!("unexpected {:?}", tokens[c.pos])));
    }
    Ok(expr)
}

struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
    line: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn pipeline(&mut self) -> Result<Expr, Error> {
        let mut expr = self.command()?;
        while self.peek() == Some(&Token::Pipe) {
            self.pos += 1;
            let rhs = self.command()?;
            expr = Expr::Pipe(Box::new(expr), Box::new(rhs));
        }
        Ok(expr)
    }

    fn command(&mut self) -> Result<Expr, Error> {
        let first = self.operand()?;
        let mut args = Vec::new();
        while !matches!(self.peek(), None | Some(Token::Pipe | Token::RParen | Token::Comma)) {
            args.push(self.operand()?);
        }
        if args.is_empty() {
            return Ok(first);
        }
        match first {
            Expr::Call { name, args: existing, line } if existing.is_empty() => {
                Ok(Expr::Call { name, args, line })
            }
            other => Err(template_err(
                self.line,
                format!("{} cannot take arguments; only functions can", describe(&other)),
            )),
        }
    }

    fn operand(&mut self) -> Result<Expr, Error> {
        let line = self.line;
        let Some(token) = self.peek().cloned() else {
            return Err(template_err(line, "expected a value but the action ended"));
        };
        self.pos += 1;
        Ok(match token {
            Token::Field(path) => Expr::Field(path),
            Token::Var(name, path) => {
                let base = if name.is_empty() { Expr::Root } else { Expr::Var(name) };
                if path.is_empty() { base } else { Expr::Access { base: Box::new(base), path } }
            }
            Token::Str(s) => Expr::Lit(Value::String(s)),
            Token::Num(n) => Expr::Lit(number(n)),
            Token::Ident(name) => match name.as_str() {
                "true" => Expr::Lit(Value::Bool(true)),
                "false" => Expr::Lit(Value::Bool(false)),
                "nil" | "null" => Expr::Lit(Value::Null),
                _ => Expr::Call { name, args: Vec::new(), line },
            },
            Token::LParen => {
                let inner = self.pipeline()?;
                if self.peek() != Some(&Token::RParen) {
                    return Err(template_err(line, "missing `)`"));
                }
                self.pos += 1;
                inner
            }
            other => return Err(template_err(line, format!("unexpected {other:?}"))),
        })
    }
}

/// Integral literals become JSON integers, so `{{truncate 30 .t}}` sees `30` and not `30.0`,
/// and so `{{.n}}` round-trips through a template without gaining a decimal point.
fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

fn describe(e: &Expr) -> &'static str {
    match e {
        Expr::Root => "`$`",
        Expr::Field(_) => "a field",
        Expr::Var(_) => "a variable",
        Expr::Access { .. } => "a field",
        Expr::Lit(_) => "a literal",
        Expr::Call { .. } => "a function",
        Expr::Pipe(..) => "a pipeline",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::ErrorKind;

    #[test]
    fn text_and_field() {
        assert_eq!(
            parse("n={{.number}}\n").unwrap(),
            vec![
                Node::Text("n=".into()),
                Node::Emit { expr: Expr::Field(vec!["number".into()]), line: 1 },
                Node::Text("\n".into()),
            ]
        );
    }

    /// Bug this prevents: parsing `a | f x` as `f x` and dropping `a`, or as `a f x`.
    /// Go's rule is that the piped value becomes the callee's *last* argument.
    #[test]
    fn pipe_associates_left_and_keeps_both_sides() {
        let Node::Emit { expr, .. } = &parse("{{.t | truncate 30}}").unwrap()[0] else { panic!() };
        assert_eq!(
            expr,
            &Expr::Pipe(
                Box::new(Expr::Field(vec!["t".into()])),
                Box::new(Expr::Call {
                    name: "truncate".into(),
                    args: vec![Expr::Lit(Value::from(30))],
                    line: 1
                })
            )
        );
    }

    #[test]
    fn parenthesized_pipelines_nest() {
        assert!(parse(r#"{{tablerow (.n | truncate 4) .t}}"#).is_ok());
    }

    #[test]
    fn if_else_end() {
        let nodes = parse("{{if .draft}}D{{else}}-{{end}}").unwrap();
        let Node::If { then, otherwise, .. } = &nodes[0] else { panic!("{nodes:?}") };
        assert_eq!(then, &vec![Node::Text("D".into())]);
        assert_eq!(otherwise, &vec![Node::Text("-".into())]);
    }

    /// Bug this prevents: binding the single `range` variable to the index instead of the
    /// element, which silently prints `0 1 2` where a `gh` template printed titles.
    #[test]
    fn one_range_variable_is_the_element() {
        let nodes = parse("{{range $v := .items}}{{$v}}{{end}}").unwrap();
        let Node::Range { key, val, .. } = &nodes[0] else { panic!() };
        assert_eq!(key, &None);
        assert_eq!(val.as_deref(), Some("v"));

        let nodes = parse("{{range $i, $v := .items}}{{end}}").unwrap();
        let Node::Range { key, val, .. } = &nodes[0] else { panic!() };
        assert_eq!(key.as_deref(), Some("i"));
        assert_eq!(val.as_deref(), Some("v"));
    }

    /// Bug this prevents: an unbalanced `{{end}}` or a missing one being ignored, so the
    /// template renders a subtly wrong shape instead of reporting the mistake.
    #[test]
    fn unbalanced_blocks_are_errors() {
        for src in ["{{range .}}x", "{{if .}}x", "x{{end}}", "{{else}}"] {
            let err = parse(src).unwrap_err();
            assert!(matches!(&*err.kind, ErrorKind::Template { .. }), "{src}");
        }
    }

    /// Bug this prevents: `{{.a .b}}` being silently accepted as something, when it is a
    /// user error (a field cannot take arguments) that should name the problem.
    #[test]
    fn a_field_cannot_take_arguments() {
        let err = parse("{{.a .b}}").unwrap_err();
        assert!(
            matches!(&*err.kind, ErrorKind::Template { message, .. } if message.contains("field")),
            "{:?}",
            err.kind
        );
    }

    /// Bug this prevents: integral literals arriving as floats, so `truncate 30` receives
    /// `30.0` and any function that wants an integer has to round.
    #[test]
    fn integral_literals_stay_integral() {
        let Node::Emit { expr, .. } = &parse("{{truncate 30 .t}}").unwrap()[0] else { panic!() };
        let Expr::Call { args, .. } = expr else { panic!() };
        assert_eq!(args[0], Expr::Lit(Value::from(30)));
    }

    #[test]
    fn empty_action_is_a_noop() {
        assert_eq!(parse("a{{}}b").unwrap(), vec![Node::Text("a".into()), Node::Text("b".into())]);
    }
}

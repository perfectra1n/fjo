//! Lexer for the `--template` language.
//!
//! Splits a template into literal text and `{{ … }}` actions, and tokenizes each action.
//! Errors carry a **line number** rather than a byte offset, because a `--template` argument
//! is usually one line typed at a shell prompt and, when it is not, it came from a file where
//! the line is what the user can act on.

use gitea_core::{Error, ErrorKind};

/// One piece of a template: literal output, or something to evaluate.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text(String),
    Action { tokens: Vec<Token>, line: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare word: a keyword (`if`, `range`, `else`, `end`), a function name, or one of the
    /// literals `true` / `false` / `nil`.
    Ident(String),
    /// `.` is `Field(vec![])`; `.a.b` is `Field(["a", "b"])`.
    Field(Vec<String>),
    /// `$` is `Var("", [])` (the root value) and `$x` is `Var("x", [])`.
    ///
    /// A trailing path is attached here rather than left as a separate `Field` token, because
    /// adjacency is only visible to the lexer: `{{$.repo}}` is one operand, while
    /// `{{tablerow $ .repo}}` is two, and the parser cannot tell them apart from the token
    /// stream alone.
    Var(String, Vec<String>),
    Str(String),
    Num(f64),
    Pipe,
    LParen,
    RParen,
    Comma,
    /// `:=`
    Define,
}

pub fn lex(src: &str) -> Result<Vec<Item>, Error> {
    let mut items = Vec::new();
    let mut rest = src;
    let mut line = 1usize;
    // Set by a preceding `-}}`: the *next* text run has its leading whitespace removed.
    let mut trim_next_text = false;

    while let Some(open) = rest.find("{{") {
        let (text, after) = rest.split_at(open);
        line += text.matches('\n').count();

        let body_start = &after[2..];
        // `{{-` only trims when the minus is followed by whitespace, matching Go. Without that
        // rule `{{-1}}` (a negative literal) would be read as a trim marker.
        let (trim_prev, body_start) = match body_start.strip_prefix('-') {
            Some(after_dash) if after_dash.starts_with([' ', '\t', '\r', '\n']) => {
                (true, after_dash)
            }
            _ => (false, body_start),
        };

        let Some(close) = body_start.find("}}") else {
            return Err(template_err(line, "unclosed `{{`; add the matching `}}`"));
        };
        let (mut body, after_close) = body_start.split_at(close);
        let after_close = &after_close[2..];

        // `-}}` likewise requires whitespace before the minus.
        let trim_after = match body.strip_suffix('-') {
            Some(before) if before.ends_with([' ', '\t', '\r', '\n']) => {
                body = before;
                true
            }
            _ => false,
        };

        push_text(&mut items, text, trim_next_text, trim_prev);
        trim_next_text = trim_after;

        let action_line = line;
        line += body.matches('\n').count();
        items.push(Item::Action { tokens: tokenize(body, action_line)?, line: action_line });
        rest = after_close;
    }

    push_text(&mut items, rest, trim_next_text, false);
    Ok(items)
}

fn push_text(items: &mut Vec<Item>, text: &str, trim_start: bool, trim_end: bool) {
    let mut t = text;
    if trim_start {
        t = t.trim_start_matches([' ', '\t', '\r', '\n']);
    }
    if trim_end {
        t = t.trim_end_matches([' ', '\t', '\r', '\n']);
    }
    if !t.is_empty() {
        items.push(Item::Text(t.to_string()));
    }
}

fn tokenize(body: &str, line: usize) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            ':' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Define);
                    i += 2;
                } else {
                    return Err(template_err(line, "`:` is only valid as part of `:=`"));
                }
            }
            '"' => {
                let (s, next) = lex_quoted(&chars, i, line)?;
                tokens.push(Token::Str(s));
                i = next;
            }
            '`' => {
                // Go's raw string: no escape processing, which is what you want for a
                // `--template` containing backslashes.
                let end = chars[i + 1..]
                    .iter()
                    .position(|c| *c == '`')
                    .ok_or_else(|| template_err(line, "unterminated ` raw string"))?;
                tokens.push(Token::Str(chars[i + 1..i + 1 + end].iter().collect()));
                i += end + 2;
            }
            '$' => {
                let (name, next) = lex_ident(&chars, i + 1);
                i = next;
                let path = if chars.get(i) == Some(&'.') {
                    let (path, next) = lex_path(&chars, i);
                    i = next;
                    path
                } else {
                    Vec::new()
                };
                tokens.push(Token::Var(name, path));
            }
            '.' => {
                let (path, next) = lex_path(&chars, i);
                i = next;
                tokens.push(Token::Field(path));
            }
            c if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let start = i;
                i += 1;
                while i < chars.len()
                    && (chars[i].is_ascii_digit()
                        || chars[i] == '.'
                        || chars[i] == 'e'
                        || chars[i] == 'E'
                        || ((chars[i] == '-' || chars[i] == '+')
                            && matches!(chars[i - 1], 'e' | 'E')))
                {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let n = text
                    .parse::<f64>()
                    .map_err(|_| template_err(line, format!("{text:?} is not a number")))?;
                tokens.push(Token::Num(n));
            }
            c if c.is_alphabetic() || c == '_' => {
                let (name, next) = lex_ident(&chars, i);
                tokens.push(Token::Ident(name));
                i = next;
            }
            other => {
                return Err(template_err(
                    line,
                    format!("unexpected character {other:?} in `{{{{{body}}}}}`"),
                ));
            }
        }
    }
    Ok(tokens)
}

/// Read `.a.b.c` starting at the leading `.`. A bare `.` yields an empty path.
fn lex_path(chars: &[char], start: usize) -> (Vec<String>, usize) {
    let mut path = Vec::new();
    let mut i = start + 1;
    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
        let (name, next) = lex_ident(chars, i);
        path.push(name);
        i = next;
        if chars.get(i) == Some(&'.') {
            i += 1;
        } else {
            break;
        }
    }
    (path, i)
}

/// Read an identifier: letters, digits, and `_`.
///
/// `-` is deliberately excluded. Gitea's field names are snake_case, and allowing `-` would
/// make `{{.a -}}` ambiguous with a field named `a-`. Map keys containing `-` are reachable
/// with `--jq`.
fn lex_ident(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

fn lex_quoted(chars: &[char], start: usize, line: usize) -> Result<(String, usize), Error> {
    let mut out = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((out, i + 1)),
            '\\' => {
                let esc =
                    chars.get(i + 1).ok_or_else(|| template_err(line, "unterminated string"))?;
                match esc {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    'u' => {
                        let hex: String = chars.get(i + 2..i + 6).unwrap_or(&[]).iter().collect();
                        let cp = u32::from_str_radix(&hex, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(|| {
                                template_err(line, format!("bad \\u escape: \\u{hex}"))
                            })?;
                        out.push(cp);
                        i += 4;
                    }
                    other => {
                        return Err(template_err(line, format!("unknown escape \\{other}")));
                    }
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Err(template_err(line, "unterminated string"))
}

pub(super) fn template_err(line: usize, message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Template { message: message.into(), line })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(src: &str) -> Vec<Item> {
        lex(src).expect(src)
    }

    #[test]
    fn plain_text_only() {
        assert_eq!(actions("hello"), vec![Item::Text("hello".into())]);
        assert_eq!(actions(""), vec![]);
    }

    #[test]
    fn fields_and_paths() {
        let Item::Action { tokens, .. } = &actions("{{.head.ref}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Field(vec!["head".into(), "ref".into()])]);
        let Item::Action { tokens, .. } = &actions("{{.}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Field(vec![])]);
    }

    /// Bug this prevents: treating `{{-1}}` as a whitespace-trim marker followed by `1`,
    /// silently deleting the preceding newline and losing the minus sign.
    #[test]
    fn trim_markers_require_whitespace() {
        // A real trim.
        assert_eq!(
            actions("a\n{{- .x -}}\nb"),
            vec![
                Item::Text("a".into()),
                Item::Action { tokens: vec![Token::Field(vec!["x".into()])], line: 2 },
                Item::Text("b".into())
            ]
        );
        // Not a trim: `-1` is a number.
        let Item::Action { tokens, .. } = &actions("{{-1}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Num(-1.0)]);
    }

    /// Bug this prevents: reporting every template error on line 1, which is useless for a
    /// multi-line template read from a file.
    #[test]
    fn line_numbers_count_newlines_in_text_and_actions() {
        let items = actions("a\nb\n{{.x}}");
        let Item::Action { line, .. } = items[1] else { panic!() };
        assert_eq!(line, 3);

        let err = lex("one\ntwo\n{{ ? }}").unwrap_err();
        assert!(matches!(&*err.kind, ErrorKind::Template { line: 3, .. }), "{:?}", err.kind);
    }

    #[test]
    fn strings_numbers_and_pipes() {
        let Item::Action { tokens, .. } = &actions(r#"{{.t | truncate 30 "…"}}"#)[0] else {
            panic!()
        };
        assert_eq!(
            tokens,
            &[
                Token::Field(vec!["t".into()]),
                Token::Pipe,
                Token::Ident("truncate".into()),
                Token::Num(30.0),
                Token::Str("…".into()),
            ]
        );
    }

    #[test]
    fn raw_strings_skip_escapes() {
        let Item::Action { tokens, .. } = &actions("{{`a\\nb`}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Str("a\\nb".into())]);
    }

    #[test]
    fn variables_and_define() {
        let Item::Action { tokens, .. } = &actions("{{range $i, $v := .items}}")[0] else {
            panic!()
        };
        assert_eq!(
            tokens,
            &[
                Token::Ident("range".into()),
                Token::Var("i".into(), vec![]),
                Token::Comma,
                Token::Var("v".into(), vec![]),
                Token::Define,
                Token::Field(vec!["items".into()]),
            ]
        );
        let Item::Action { tokens, .. } = &actions("{{$}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Var(String::new(), vec![])]);
    }

    /// Bug this prevents: lexing `{{$.repo}}` as two operands (`$` then `.repo`), which the
    /// parser then rejects as "`$` cannot take arguments" — breaking the standard way a
    /// template reaches the root document from inside a `range`.
    #[test]
    fn a_path_after_a_variable_is_one_operand() {
        let Item::Action { tokens, .. } = &actions("{{$.repo.name}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Var(String::new(), vec!["repo".into(), "name".into()])]);
        let Item::Action { tokens, .. } = &actions("{{$v.name}}")[0] else { panic!() };
        assert_eq!(tokens, &[Token::Var("v".into(), vec!["name".into()])]);
        // ...but a space makes them two operands again.
        let Item::Action { tokens, .. } = &actions("{{f $ .name}}")[0] else { panic!() };
        assert_eq!(
            tokens,
            &[
                Token::Ident("f".into()),
                Token::Var(String::new(), vec![]),
                Token::Field(vec!["name".into()])
            ]
        );
    }

    /// Bug this prevents: an unclosed action silently swallowing the rest of the template, so
    /// the user sees empty output and no explanation.
    #[test]
    fn unclosed_action_is_an_error() {
        let err = lex("{{.x").unwrap_err();
        assert!(matches!(&*err.kind, ErrorKind::Template { .. }));
        assert_eq!(err.exit_code(), 2);
    }
}

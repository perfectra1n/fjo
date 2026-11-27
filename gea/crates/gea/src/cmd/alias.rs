//! `gea alias` — shortcuts for commands you run often, plus the expansion that makes them work.
//!
//! # Expansion happens before parsing
//!
//! `gea prs` cannot be a clap subcommand: clap would have to be told about it, and the alias
//! table is not known until `config.toml` has been read. So [`expand`] rewrites `argv` *before*
//! either parse phase, exactly as `gh` does. `gea alias set prs 'pr list --json number,title'`
//! then makes `gea prs` become `gea pr list --json number,title`.
//!
//! Three rules keep that from being a footgun:
//!
//! * **An alias may not shadow a real command.** [`set`] refuses it and names what it collided
//!   with. Without that, `gea alias set pr 'pr list'` would make `gea pr create` unreachable and
//!   the only clue would be a clap error about `create`.
//! * **Expansion is recursion-safe.** An alias whose expansion begins with itself — directly or
//!   through a chain — is refused at `set` time *and* detected at expansion time, because
//!   `config.toml` can be hand-edited. A loop here would hang the process before it printed
//!   anything, which is the worst possible failure mode for a command-line tool.
//! * **`$1`-style placeholders are positional, and unconsumed arguments are appended.** So
//!   `alias set co 'pr checkout $1'` makes `gea co 42 --force` become
//!   `gea pr checkout 42 --force`, and `gea co` fails naming how many arguments `co` needs
//!   rather than sending a request with a literal `$1` in the path.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};

use clap::{Args as ClapArgs, Subcommand};
use gitea_core::config::Config;
use gitea_core::error::Result;
use serde_json::{Value, json};

use super::auth::common::{self};
use crate::cmd::support;
use crate::cmd::support::machine::Triad;
use crate::global::GlobalOpts;
use crate::output::{Table, Term, project::FieldKind, project::FieldSpec};

/// `--json` selectable fields for `alias list`.
const FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "name", kind: FieldKind::Str, doc: "the word you type" },
    FieldSpec { name: "expansion", kind: FieldKind::Str, doc: "what it becomes" },
];

/// The hidden `tea` aliases, and the only built-in aliases there are.
///
/// `gh`'s names are `gea`'s names, so these do **not** appear in `--help`, in completions, or in
/// `gea alias list`. They exist for one reason: somebody with `tea` in their fingers types
/// `gea pull ls` and should land on `gea pr ls` rather than on "unrecognized subcommand". Two of
/// the five expand to *two* words (`login` → `auth login`, `whoami` → `auth status`), which is why
/// they live here rather than as `#[command(alias = …)]` on [`super::Porcelain`] — a clap alias
/// renames one subcommand and cannot reach into another group.
///
/// One table, in one file, per `docs/porcelain-conventions.md`. A user's own alias of the same
/// name **wins**: [`expand`] fills these in only where the user has not defined the name, so
/// `gea alias set pull 'pr list --state all'` does exactly what it says.
pub const BUILTIN: &[(&str, &str)] = &[
    ("pull", "pr"),
    ("labels", "label"),
    ("ms", "milestone"),
    ("login", "auth login"),
    ("whoami", "auth status"),
];

/// How deep a chain of aliases may go before we call it a mistake.
///
/// The cycle check already catches loops; this catches a pathological but acyclic chain, and keeps
/// the loop below obviously terminating rather than obviously-terminating-if-you-trust-the-set.
const MAX_DEPTH: usize = 10;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

const LONG_HELP: &str = "\
Manage command shortcuts stored in config.toml.

Aliases can contain commands and flags. $1 through $9 substitute positional
arguments; unused arguments are appended in order. Aliases cannot replace real
commands or contain expansion cycles.

Built-in aliases: pull -> pr, labels -> label, ms -> milestone, login -> auth login,
whoami -> auth status. They are hidden from `alias list`; user aliases can replace them.

  gea alias set prs 'pr list --json number,title'
  gea prs                                          # runs the above
  gea alias set co 'pr checkout $1'
  gea co 42 --force                                # -> pr checkout 42 --force
  gea alias list
  gea alias delete co
  gea alias import my-aliases.toml
  gea alias import -                               # from stdin";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create or replace an alias
    Set(SetArgs),
    /// Print every alias
    List(ListArgs),
    /// Remove an alias
    Delete(DeleteArgs),
    /// Add aliases from a TOML file; `-` reads stdin
    Import(ImportArgs),
}

#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
    /// The command it expands to, e.g. 'pr list --json number,title'
    #[arg(value_name = "EXPANSION")]
    pub expansion: String,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Debug, ClapArgs)]
pub struct ImportArgs {
    /// TOML file of `name = "expansion"` pairs, or an `[aliases]` table; `-` reads stdin
    #[arg(value_name = "FILE")]
    pub file: String,
    /// Replace aliases that already exist instead of skipping them
    #[arg(long)]
    pub clobber: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.command {
        Cmd::Set(a) => set(globals, a),
        Cmd::List(a) => list(globals, a),
        Cmd::Delete(a) => delete(globals, a),
        Cmd::Import(a) => import(globals, a),
    }
}

// ------------------------------------------------------------------------------------ set

fn set(_globals: &GlobalOpts, args: &SetArgs) -> Result<()> {
    let mut config = Config::load(common::env())?;
    let name = args.name.trim();
    let expansion = args.expansion.trim();

    check_name(name)?;
    check_expansion(name, expansion, &config.aliases())?;

    let replacing = config.alias(name);
    config.set_alias(name, expansion)?;
    config.save()?;

    let term = Term::detect();
    match replacing {
        Some(old) if old != expansion => {
            support::note(&term, &format!("{name} was {old:?}, now {expansion:?}"));
        }
        _ => support::note(&term, &format!("{name} expands to {expansion:?}")),
    }
    Ok(())
}

/// A usable alias name that does not collide with anything `gea` already answers to.
///
/// The command list comes from [`super::completion::root`], the same tree completions are
/// generated from, so a group added to [`super::Porcelain`] immediately becomes unaliasable with no
/// list to maintain here.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') || name.split_whitespace().count() != 1 {
        return Err(support::usage(format!(
            "{name:?} is not a usable alias name: it must be a single word that does not start \
             with '-'"
        )));
    }
    if super::completion::command_names().iter().any(|c| c == name) {
        return Err(support::usage(format!(
            "{name:?} is already a real gea command, so an alias by that name could never run; \
             `gea {name} --help` shows what it does. Pick another name."
        )));
    }
    Ok(())
}

/// The expansion must be non-empty, must start with something `gea` can actually run, and must
/// not close a cycle.
fn check_expansion(name: &str, expansion: &str, existing: &BTreeMap<String, String>) -> Result<()> {
    let words = split(expansion)?;
    let Some(head) = words.first() else {
        return Err(support::usage(format!("the expansion for {name:?} is empty")));
    };
    if head.starts_with('-') {
        return Err(support::usage(format!(
            "the expansion for {name:?} starts with the flag {head:?}; it has to start with a \
             command, because that is the word gea will look up"
        )));
    }
    let known_command = super::completion::command_names().iter().any(|c| c == head);
    let known_alias = existing.contains_key(head.as_str())
        || BUILTIN.iter().any(|(n, _)| *n == head)
        || head == name;
    if !known_command && !known_alias {
        return Err(support::usage(format!(
            "{head:?} is not a real gea command or an existing alias, so {name:?} could never run; \
             `gea --help` lists the commands and `gea raw search {head}` searches the API"
        )));
    }

    // Cycle detection on the table *as it would be after this set*, so the check covers a chain
    // (a -> b -> a) and not merely direct self-reference.
    // The built-ins are part of the table expansion will actually walk, so a chain that runs
    // through one of them has to be visible here too.
    let mut table = with_builtins(existing.clone());
    table.insert(name.to_owned(), expansion.to_owned());
    if let Some(chain) = find_cycle(name, &table) {
        return Err(support::usage(format!(
            "that would make an alias loop: {}. gea would expand it forever, so it is refused.",
            chain.join(" -> ")
        )));
    }
    Ok(())
}

/// The chain of alias names walked from `start`, if it returns to a name already seen.
fn find_cycle(start: &str, table: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let mut chain = vec![start.to_owned()];
    let mut head = start.to_owned();
    for _ in 0..=MAX_DEPTH {
        let expansion = table.get(&head)?;
        let next = split(expansion).ok()?.into_iter().next()?;
        chain.push(next.clone());
        if chain[..chain.len() - 1].contains(&next) {
            return Some(chain);
        }
        head = next;
    }
    Some(chain)
}

// ----------------------------------------------------------------------------------- list

fn list(globals: &GlobalOpts, _args: &ListArgs) -> Result<()> {
    let Some(machine) = Triad::for_local_table(globals, FIELDS)? else { return Ok(()) };
    let config = Config::load(common::env())?;
    let aliases = config.aliases();
    let term = Term::detect();
    let mut out = support::writer(globals)?;

    if machine.is_explicit() {
        let payload = Value::Array(
            aliases
                .iter()
                .map(|(name, expansion)| json!({ "name": name, "expansion": expansion }))
                .collect(),
        );
        machine.pipeline().render(payload, &term, &mut out)?;
        out.flush()?;
        return Ok(());
    }

    // Emptiness is not an error, and the note goes to stderr so `gea alias list | wc -l` stays
    // honest.
    if aliases.is_empty() {
        support::note(
            &term,
            "no aliases configured. Add one with `gea alias set <name> '<command>'`.",
        );
    }
    let mut table = Table::new(&term);
    table.headers(["NAME", "EXPANSION"]);
    for (name, expansion) in &aliases {
        table.row([name.as_str(), expansion.as_str()]);
    }
    table.render(&mut out)?;
    out.flush()?;
    Ok(())
}

// --------------------------------------------------------------------------------- delete

fn delete(_globals: &GlobalOpts, args: &DeleteArgs) -> Result<()> {
    let mut config = Config::load(common::env())?;
    let name = args.name.trim();
    let Some(expansion) = config.alias(name) else {
        return Err(support::usage(format!(
            "no alias {name:?}; `gea alias list` shows the ones you have"
        )));
    };
    config.remove_alias(name);
    config.save()?;
    support::note(&Term::detect(), &format!("deleted {name} ({expansion:?})"));
    Ok(())
}

// --------------------------------------------------------------------------------- import

fn import(_globals: &GlobalOpts, args: &ImportArgs) -> Result<()> {
    let text = if args.file == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| support::usage(format!("could not read stdin: {e}")))?;
        buf
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| support::usage(format!("{}: {e}", args.file)))?
    };

    let incoming = parse_import(&text)?;
    let mut config = Config::load(common::env())?;
    let term = Term::detect();
    let mut added = 0usize;
    let mut skipped = Vec::new();

    for (name, expansion) in &incoming {
        // Each entry is validated on its own and a bad one is reported without stopping the
        // import: a file of thirty aliases where one shadows a command should install
        // twenty-nine, not zero.
        let existing = config.aliases();
        if existing.contains_key(name) && !args.clobber {
            skipped.push(format!("{name}: already set (pass --clobber to replace it)"));
            continue;
        }
        if let Err(e) = check_name(name).and_then(|()| check_expansion(name, expansion, &existing))
        {
            skipped.push(format!("{name}: {e}"));
            continue;
        }
        config.set_alias(name, expansion)?;
        added += 1;
    }

    if added > 0 {
        config.save()?;
    }
    for line in &skipped {
        common::warn(&gitea_core::ErrorKind::Usage(format!("skipped {line}")));
    }
    support::note(&term, &format!("imported {added} of {} alias(es)", incoming.len()));
    Ok(())
}

/// Both shapes a hand-written file takes: a bare table of `name = "expansion"`, and one wrapped in
/// `[aliases]` so that a fragment of `config.toml` can be imported verbatim.
///
/// # Why this is not `toml::from_str`
///
/// `toml` is deliberately not a dependency of the `gea` binary: all configuration parsing lives in
/// `gitea_core::config`, and adding a second TOML reader here would be a second thing to keep in
/// step with the format `Config` writes. What an alias file actually needs is one line shape —
/// `name = "command"` — so that is what is accepted, and anything else is a usage error naming the
/// shape rather than a TOML diagnostic about a construct nobody meant to use.
fn parse_import(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let at = n + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // Tolerated so a slice of `config.toml` imports unchanged; any other table would be a
            // different kind of setting and silently ignoring it would lose data.
            if header.trim() == "aliases" {
                continue;
            }
            return Err(support::usage(format!(
                "line {at}: only an [aliases] table is understood here, not [{}]",
                header.trim()
            )));
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(support::usage(format!(
                "line {at}: expected `name = \"command\"`, got {line:?}"
            )));
        };
        let name = name.trim();
        let value = unquote(value.trim()).ok_or_else(|| {
            support::usage(format!(
                "line {at}: an alias expansion has to be a quoted string, and {:?} is not",
                value.trim()
            ))
        })?;
        if name.is_empty() {
            return Err(support::usage(format!("line {at}: no alias name before the '='")));
        }
        out.insert(name.to_owned(), value);
    }
    if out.is_empty() {
        return Err(support::usage(
            "no aliases in that file; it should hold `name = \"command\"` lines, optionally under \
             an [aliases] table",
        ));
    }
    Ok(out)
}

/// A TOML basic or literal string, minus the quotes.
///
/// Only the escapes TOML's *basic* strings define and an alias could plausibly contain are
/// honoured. A literal (single-quoted) string has no escapes at all, per TOML, which is why
/// `'pr checkout $1'` is the form to prefer.
fn unquote(value: &str) -> Option<String> {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return Some(inner.to_owned());
    }
    let inner = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => return None,
        }
    }
    Some(out)
}

// ------------------------------------------------------------------------------- expansion

/// Long flags that take a separate value, so a leading global flag's *value* is not mistaken for
/// the command word.
///
/// Mirrors `gea_raw::build`'s own list for the same reason it exists there: a wrong guess costs a
/// mis-expansion, which clap then reports as an ordinary unknown-subcommand error, never a crash.
/// It is duplicated rather than shared because that list is private to another crate.
const VALUE_FLAGS: &[&str] = &[
    "--host",
    "--hostname",
    "--login",
    "--repo",
    "-R",
    "--jq",
    "-q",
    "--template",
    "-t",
    "--color",
    "--sudo",
    "--otp",
    "--max-retries",
    "--limit",
    "--output",
];

/// Rewrite `argv` with any leading alias expanded.
///
/// Called from `main` before either parse phase. Returns `argv` unchanged whenever there is
/// nothing to do, which is the overwhelmingly common case; the only unavoidable cost is reading
/// `config.toml`, and that read happens moments later anyway.
///
/// The alias table is the *only* thing consulted. It does not need the command tree, because
/// [`check_name`] already refused any alias that shadows a command — and not building the tree
/// here is what keeps this off the startup budget.
pub fn expand(argv: &[OsString]) -> Result<Vec<OsString>> {
    let Some(at) = command_word_index(argv) else { return Ok(argv.to_vec()) };
    let Some(word) = argv[at].to_str() else { return Ok(argv.to_vec()) };

    let config = Config::load(common::env())?;
    let aliases = with_builtins(config.aliases());
    if !aliases.contains_key(word) {
        return Ok(argv.to_vec());
    }

    let head: Vec<OsString> = argv[..at].to_vec();
    // Substitution is textual, so a non-UTF-8 argument cannot be carried through it without
    // silently mangling the bytes. Refusing is the honest answer, and it names the culprit; a
    // lossy conversion would corrupt a path and blame the server for the 404.
    let mut tail: Vec<String> = Vec::with_capacity(argv.len().saturating_sub(at + 1));
    for arg in &argv[at + 1..] {
        match arg.to_str() {
            Some(s) => tail.push(s.to_owned()),
            None => {
                return Err(support::usage(format!(
                    "alias {word:?} received an argument that is not valid UTF-8. Run the underlying command directly."
                )));
            }
        }
    }
    let expanded = expand_words(word, &tail, &aliases)?;

    let mut out = head;
    out.extend(expanded.into_iter().map(OsString::from));
    Ok(out)
}

/// `user` with [`BUILTIN`] filled in underneath it.
///
/// `or_insert` rather than `insert`: a name the user has defined is theirs, and silently
/// overriding `whoami` with ours would be the tool disagreeing with its own `alias list`.
fn with_builtins(mut user: BTreeMap<String, String>) -> BTreeMap<String, String> {
    for (name, expansion) in BUILTIN {
        user.entry((*name).to_owned()).or_insert_with(|| (*expansion).to_owned());
    }
    user
}

/// The index of the first token that is a command word rather than a flag or a flag's value.
fn command_word_index(argv: &[OsString]) -> Option<usize> {
    let mut i = 1;
    let mut end_of_flags = false;
    while i < argv.len() {
        let tok = argv[i].as_os_str();
        let bytes = tok.as_encoded_bytes();
        if end_of_flags || bytes == b"-" || !bytes.starts_with(b"-") {
            return Some(i);
        }
        if bytes == b"--" {
            end_of_flags = true;
        } else if !bytes.contains(&b'=') && tok.to_str().is_some_and(|s| VALUE_FLAGS.contains(&s)) {
            i += 1;
        }
        i += 1;
    }
    None
}

/// Resolve `name` plus `args` into the words `gea` should actually parse.
fn expand_words(
    name: &str,
    args: &[String],
    aliases: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut seen: Vec<String> = Vec::new();
    let mut head = name.to_owned();
    let mut args: Vec<String> = args.to_vec();
    let mut prefix: Vec<String> = Vec::new();

    for _ in 0..MAX_DEPTH {
        if seen.contains(&head) {
            seen.push(head);
            // Reachable only from a hand-edited config.toml, since `set` refuses cycles. Still
            // checked, because the alternative is an infinite loop before any output.
            return Err(support::usage(format!(
                "the alias {name:?} expands in a loop: {}. Fix it with `gea alias delete \
                 {name}` or by editing config.toml.",
                seen.join(" -> ")
            )));
        }
        let Some(expansion) = aliases.get(&head) else {
            // Not an alias: `head` is the real command, and everything else follows it.
            prefix.push(head);
            prefix.extend(args);
            return Ok(prefix);
        };
        let words = split(expansion)?;
        let (mut substituted, leftover) = substitute(name, &words, &args)?;
        if substituted.is_empty() {
            // Only reachable from a hand-edited `aliases = { x = "" }`; `set` refuses it.
            return Err(support::usage(format!("the alias {head:?} expands to nothing")));
        }
        seen.push(head);
        // The chain continues from the expansion's own first word.
        head = substituted.remove(0);
        args = substituted.into_iter().chain(leftover).collect();
    }

    Err(support::usage(format!(
        "the alias {name:?} expands through more than {MAX_DEPTH} other aliases; that is almost \
         certainly a mistake"
    )))
}

/// Replace `$1`..`$9` and return the arguments no placeholder consumed.
///
/// Unconsumed arguments are *appended* rather than discarded, which is `gh`'s behaviour and the
/// useful one: `alias set co 'pr checkout $1'` still lets `gea co 42 --force` pass `--force`
/// through.
fn substitute(name: &str, words: &[String], args: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut used = [false; 9];
    let mut out = Vec::with_capacity(words.len());
    for word in words {
        let mut rendered = String::with_capacity(word.len());
        let mut chars = word.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                rendered.push(c);
                continue;
            }
            match chars.peek().and_then(|d| d.to_digit(10)).filter(|d| (1..=9).contains(d)) {
                None => rendered.push('$'),
                Some(n) => {
                    chars.next();
                    let idx = n as usize - 1;
                    let Some(value) = args.get(idx) else {
                        return Err(support::usage(format!(
                            "alias {name:?} requires at least {n} arguments (uses ${n}); received {}",
                            args.len()
                        )));
                    };
                    used[idx] = true;
                    rendered.push_str(value);
                }
            }
        }
        out.push(rendered);
    }
    let leftover = args
        .iter()
        .enumerate()
        .filter(|(i, _)| !used.get(*i).copied().unwrap_or(false))
        .map(|(_, a)| a.clone())
        .collect();
    Ok((out, leftover))
}

/// Split an expansion into words, honouring quotes.
///
/// A tiny shell-like splitter rather than `split_whitespace`, because an expansion routinely
/// contains a quoted argument: `alias set bug 'issue create -t "needs triage"'`. Whitespace
/// splitting would send `"needs` and `triage"` as two arguments, complete with the quote
/// characters, and the resulting issue title would be wrong in a way nobody blames the alias for.
///
/// Deliberately *not* a shell: no variable expansion, no globbing, no pipelines. An expansion is a
/// command line for `gea`, and running it through a shell would make `alias set x 'repo view;
/// rm -rf ~'` mean something.
fn split(input: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut have_word = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.push(c),
            (Some('"'), '\\') => match chars.next() {
                // Inside double quotes only `\"` and `\\` are escapes, as in POSIX sh.
                Some(next @ ('"' | '\\')) => word.push(next),
                Some(other) => {
                    word.push('\\');
                    word.push(other);
                }
                None => word.push('\\'),
            },
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                have_word = true;
            }
            (None, '\\') => match chars.next() {
                Some(next) => {
                    word.push(next);
                    have_word = true;
                }
                None => return Err(support::usage("the expansion ends with a lone backslash")),
            },
            (None, c) if c.is_whitespace() => {
                if have_word {
                    out.push(std::mem::take(&mut word));
                    have_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                have_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(support::usage("the expansion has an unclosed quote"));
    }
    if have_word {
        out.push(word);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn words(name: &str, args: &[&str], pairs: &[(&str, &str)]) -> Result<Vec<String>> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        expand_words(name, &args, &table(pairs))
    }

    #[test]
    fn a_simple_alias_becomes_its_expansion_with_the_rest_appended() {
        let got =
            words("prs", &["--limit", "5"], &[("prs", "pr list --json number,title")]).unwrap();
        assert_eq!(got, ["pr", "list", "--json", "number,title", "--limit", "5"]);
    }

    /// Bug this prevents: `split_whitespace`, which turns `-t "needs triage"` into `-t`,
    /// `"needs` and `triage"` — an issue whose title contains a quote character and half a word,
    /// with nothing pointing at the alias as the cause.
    #[test]
    fn quoted_arguments_survive_splitting() {
        assert_eq!(
            split(r#"issue create -t "needs triage" -b 'it is broken'"#).unwrap(),
            ["issue", "create", "-t", "needs triage", "-b", "it is broken"]
        );
        // An empty quoted string is a real argument, not nothing.
        assert_eq!(split(r#"issue edit -b """#).unwrap(), ["issue", "edit", "-b", ""]);
        assert!(split("issue create -t \"unclosed").is_err());
    }

    /// Bug this prevents: passing a literal `$1` through to the API, which produces a baffling
    /// 404 for issue `$1` instead of an error about the alias.
    #[test]
    fn placeholders_are_positional_and_a_missing_one_is_an_error() {
        let got = words("co", &["42", "--force"], &[("co", "pr checkout $1")]).unwrap();
        assert_eq!(got, ["pr", "checkout", "42", "--force"]);

        let e = words("co", &[], &[("co", "pr checkout $1")]).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("$1"), "{e}");

        // A placeholder inside a word, and a `$` that is not one.
        let got =
            words("mine", &["me"], &[("mine", "issue list --assignee=$1 --label=$x")]).unwrap();
        assert_eq!(got, ["issue", "list", "--assignee=me", "--label=$x"]);
    }

    /// The bug this prevents is the worst one in this file: an alias that expands to itself
    /// looping forever, so `gea x` hangs with no output and no clue.
    #[test]
    fn a_self_referential_alias_fails_instead_of_looping() {
        let e = words("x", &[], &[("x", "x --json")]).unwrap_err();
        assert!(e.to_string().contains("loop"), "{e}");
        // And a chain, which a direct self-reference check would miss.
        let e = words("a", &[], &[("a", "b"), ("b", "c"), ("c", "a")]).unwrap_err();
        assert!(e.to_string().contains("loop"), "{e}");
        assert!(e.to_string().contains("a -> b -> c -> a"), "{e}");
    }

    #[test]
    fn an_alias_may_expand_through_another_alias() {
        let got =
            words("mine", &[], &[("mine", "open --assignee @me"), ("open", "issue list")]).unwrap();
        assert_eq!(got, ["issue", "list", "--assignee", "@me"]);
    }

    /// Bug this prevents: `gea alias set pr 'pr list'` succeeding, after which `gea pr create`
    /// expands to `pr list create` and the user gets a clap error about `create`.
    #[test]
    fn an_alias_may_not_shadow_a_real_command() {
        let e = check_name("pr").unwrap_err();
        assert!(e.to_string().contains("already a real gea command"), "{e}");
        assert!(check_name("prs").is_ok());
        assert!(check_name("-x").is_err(), "a flag-looking name would never be reachable");
        assert!(check_name("two words").is_err());
    }

    /// Bug this prevents: storing an expansion whose first word is a typo, which fails much later
    /// with a clap error naming the typo but not the alias it came from.
    #[test]
    fn an_expansion_must_start_with_something_that_exists() {
        let empty = BTreeMap::new();
        assert!(check_expansion("prs", "pr list", &empty).is_ok());
        let e = check_expansion("prs", "prr list", &empty).unwrap_err();
        assert!(e.to_string().contains("not a real gea command"), "{e}");
        let e = check_expansion("prs", "--json number", &empty).unwrap_err();
        assert!(e.to_string().contains("starts with the flag"), "{e}");
        // ...but chaining onto another alias is fine.
        assert!(
            check_expansion("mine", "prs --assignee @me", &table(&[("prs", "pr list")])).is_ok()
        );
    }

    #[test]
    fn set_time_cycle_detection_covers_chains() {
        let e = check_expansion("c", "a", &table(&[("a", "b"), ("b", "c")])).unwrap_err();
        assert!(e.to_string().contains("loop"), "{e}");
    }

    /// Bug this prevents: taking `argv[1]` blindly, so `gea --debug prs` does not expand and the
    /// alias appears to work only sometimes.
    #[test]
    fn the_command_word_is_found_past_leading_global_flags() {
        let argv = |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };
        assert_eq!(command_word_index(&argv(&["gea", "prs"])), Some(1));
        assert_eq!(command_word_index(&argv(&["gea", "--debug", "prs"])), Some(2));
        assert_eq!(command_word_index(&argv(&["gea", "--host", "h", "prs"])), Some(3));
        assert_eq!(command_word_index(&argv(&["gea", "--host=h", "prs"])), Some(2));
        assert_eq!(command_word_index(&argv(&["gea", "--help"])), None);
    }

    /// Both shapes a hand-written import file takes, plus a clear refusal for a non-string value.
    #[test]
    fn import_accepts_a_bare_table_or_an_aliases_table() {
        let bare = parse_import("prs = \"pr list\"\nco = \"pr checkout $1\"\n").unwrap();
        assert_eq!(bare.len(), 2);
        let wrapped = parse_import("[aliases]\nprs = \"pr list\"\n").unwrap();
        assert_eq!(wrapped.get("prs").map(String::as_str), Some("pr list"));
        assert!(parse_import("prs = 7\n").is_err(), "an unquoted value is not a string");
        assert!(parse_import("").is_err());
        assert!(
            parse_import("[other]\nx = \"y\"\n").is_err(),
            "a foreign table must not be silently dropped"
        );
        // A literal string carries no escapes, which is why it is the form to prefer for $1.
        assert_eq!(
            parse_import("co = 'pr checkout $1'\n").unwrap().get("co").map(String::as_str),
            Some("pr checkout $1")
        );
    }

    /// Bug this prevents: a `tea` user's muscle memory hitting "unrecognized subcommand". These
    /// five are the aliases the plan promised, and the expansion has to carry the rest of the
    /// command line through — `gea pull ls --state all` is the whole point, not bare `gea pull`.
    #[test]
    fn the_built_in_tea_aliases_expand_including_the_two_word_ones() {
        let builtins = with_builtins(BTreeMap::new());
        let go = |name: &str, args: &[&str]| -> Vec<String> {
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            expand_words(name, &args, &builtins).expect("a built-in alias expands")
        };
        assert_eq!(go("pull", &["ls", "--state", "all"]), ["pr", "ls", "--state", "all"]);
        assert_eq!(go("labels", &["list"]), ["label", "list"]);
        assert_eq!(go("ms", &["list"]), ["milestone", "list"]);
        assert_eq!(
            go("login", &["--host", "git.example.org"]),
            ["auth", "login", "--host", "git.example.org"]
        );
        assert_eq!(go("whoami", &[]), ["auth", "status"]);
    }

    /// Bug this prevents: a built-in silently overriding the alias a user set by that name, so
    /// `gea alias list` shows one thing and `gea whoami` does another.
    #[test]
    fn a_users_own_alias_beats_a_built_in_of_the_same_name() {
        let user = table(&[("whoami", "api user")]);
        let merged = with_builtins(user);
        assert_eq!(merged.get("whoami").map(String::as_str), Some("api user"));
        // ...and the ones they did not define are still there.
        assert_eq!(merged.get("pull").map(String::as_str), Some("pr"));
    }

    /// Every built-in has to expand to something `gea` can actually run, or it is a worse
    /// failure than the "unrecognized subcommand" it was added to avoid.
    #[test]
    fn every_built_in_expands_to_a_real_command() {
        let commands = super::super::completion::command_names();
        for (name, expansion) in BUILTIN {
            let head = split(expansion).expect("a built-in expansion parses").remove(0);
            assert!(
                commands.contains(&head),
                "the built-in alias {name} expands to {expansion:?}, whose first word is not a \
                 command"
            );
            assert!(
                !commands.iter().any(|c| c == *name),
                "{name} is a real command, so the built-in alias could never run"
            );
        }
    }

    /// Bug this prevents: `alias list`'s `--json` field table drifting from the object it emits.
    #[test]
    fn the_json_row_emits_exactly_the_declared_fields() {
        let v = json!({ "name": "prs", "expansion": "pr list" });
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, FIELDS.iter().map(|f| f.name).collect::<Vec<_>>());
    }
}

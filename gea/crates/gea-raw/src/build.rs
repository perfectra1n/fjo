//! Metadata to [`clap::Command`], built lazily for **only** the subtree the user named.
//!
//! # Why the builder API and not `derive`
//!
//! clap's derive path constructs the entire command tree inside `Parser::parse()`. With 506
//! operations and ~3,000 flags that is thousands of `Arg` allocations on *every* invocation,
//! including `gea --version` — 5–20 ms of work to build a parser the user never reaches, in a
//! tool whose whole pre-network budget is 20 ms. So the binary [`peek`]s at `argv` first,
//! learns which group and leaf were named, and asks [`build`] for that subtree alone:
//!
//! | invocation | what gets built |
//! | --- | --- |
//! | `gea --version` | nothing — [`peek`] returns `None` |
//! | `gea raw --help` | one stub per group (~12 `Command`s, no `Arg`s) |
//! | `gea raw repo --help` | one name-and-`about` stub per op in `repo` |
//! | `gea raw repo get --help` | exactly one full `Command`, with its args |
//!
//! [`lookup::ops_in`] hands back a group as a contiguous slice, so the middle case is
//! O(group), never O(506).
//!
//! # Why every argument is optional to clap
//!
//! Not one `Arg` here is `.required(true)`, including path parameters the API cannot do
//! without. Three separate mechanisms can supply a value that clap cannot see: a path
//! parameter may arrive positionally *or* by flag *or* from resolved repository context, and a
//! body field may arrive by flag *or* inside `--body-file`. Marking them required in clap
//! would reject invocations that are in fact complete. Requiredness is therefore decided in
//! [`mod@crate::bind`], after merging, and `--help` says `(required)` in the argument's help text.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};

use clap::builder::ValueHint;
use clap::{Arg, ArgAction, Command, value_parser};
use gitea_client::meta_types::{
    BodyField, GroupMeta, OpMeta, Pagination, ParamMeta, ValueTy, lookup,
};

/// Engine flags. These are the same on all 506 commands, so they claim their long names
/// first and a colliding *generated* flag is the one that moves (see [`unique_long`]).
pub const ID_BODY_FILE: &str = "gea:body-file";
pub const ID_DRY_RUN: &str = "gea:dry-run";
pub const ID_PAGINATE: &str = "gea:paginate";
pub const ID_LIMIT: &str = "gea:limit";

/// Argument ids are namespaced by parameter location so that an operation with, say, both a
/// `{ref}` path parameter and a `?ref=` query parameter cannot produce two `Arg`s with the
/// same id — which clap answers with a panic, i.e. a crash driven by table contents.
pub fn path_flag_id(flag: &str) -> String {
    format!("path:{flag}")
}

/// Id of the positional twin of a path parameter. Path parameters are accepted **both**
/// positionally and as flags; clap cannot make one `Arg` do both, so there are two and
/// [`mod@crate::bind`] merges them.
pub fn path_pos_id(flag: &str) -> String {
    format!("pos:{flag}")
}

pub fn query_id(flag: &str) -> String {
    format!("query:{flag}")
}

pub fn form_id(flag: &str) -> String {
    format!("form:{flag}")
}

pub fn body_id(flag: &str) -> String {
    format!("body:{flag}")
}

// ---------------------------------------------------------------------------- peek

/// What [`peek`] found in `argv`: the layer-2 subtree to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peeked<'a> {
    /// The group token, if one was present. `None` for `gea raw` / `gea raw --help`.
    pub group: Option<&'a str>,
    /// The leaf token, if one was present.
    pub leaf: Option<&'a str>,
}

/// Global options that take their value as a *separate* token.
///
/// Without this list, `gea --host example.org raw repo get` would see `example.org` as the
/// first non-flag token and conclude the user did not ask for layer 2 at all. The list only
/// needs to cover options that can legally appear *before* a subcommand; a wrong guess costs a
/// mis-peek, which clap then reports as an ordinary unknown-subcommand error, never a crash.
const VALUE_FLAGS: &[&[u8]] = &[
    b"--host",
    b"--hostname",
    b"--login",
    b"--repo",
    b"-R",
    b"--token",
    b"--otp",
    b"--sudo",
    b"--config",
    b"--jq",
    b"-q",
    b"--template",
    b"-t",
    b"--output",
];

/// Find the next token that is not a flag or a flag's value.
fn next_word<'a, I>(it: &mut I, end_of_flags: &mut bool) -> Option<&'a OsStr>
where
    I: Iterator<Item = &'a OsString>,
{
    while let Some(tok) = it.next() {
        let b = tok.as_encoded_bytes();
        if *end_of_flags || b == b"-" || !b.starts_with(b"-") {
            return Some(tok.as_os_str());
        }
        if b == b"--" {
            *end_of_flags = true;
            continue;
        }
        // `--host=x` carries its value inline; `--host x` does not.
        if !b.contains(&b'=') && VALUE_FLAGS.contains(&b) {
            it.next();
        }
    }
    None
}

/// Decide, in nanoseconds and without allocating, whether this invocation is layer 2 — and if
/// so which subtree it needs.
///
/// `x` is the hidden alias for `raw`, matched here as well as in the built `Command` so that
/// `gea x repo get` peeks the same subtree it will later parse.
pub fn peek(argv: &[OsString]) -> Option<Peeked<'_>> {
    let mut it = argv.iter().skip(1);
    let mut end_of_flags = false;
    let first = next_word(&mut it, &mut end_of_flags)?.to_str()?;
    if first != "raw" && first != "x" {
        return None;
    }
    let group = next_word(&mut it, &mut end_of_flags).and_then(OsStr::to_str);
    // Only look for a leaf once a group is known: `gea raw --help` has neither.
    let leaf = group.and_then(|_| next_word(&mut it, &mut end_of_flags).and_then(OsStr::to_str));
    Some(Peeked { group, leaf })
}

// --------------------------------------------------------------------------- build

/// Build the root `gea` command carrying only the requested layer-2 subtree.
///
/// The binary owns the global flags (`--host`, `--jq`, …), so it will normally prefer
/// [`raw_command`] and hang it off its own root. This function exists for tests and for the
/// simple case.
pub fn build(
    ops: &'static [OpMeta],
    groups: &'static [GroupMeta],
    group: Option<&str>,
    leaf: Option<&str>,
) -> Command {
    Command::new("gea").subcommand(raw_command(ops, groups, group, leaf))
}

/// Build the `raw` subcommand, containing only the requested subtree.
pub fn raw_command(
    ops: &'static [OpMeta],
    groups: &'static [GroupMeta],
    group: Option<&str>,
    leaf: Option<&str>,
) -> Command {
    let mut raw = Command::new("raw")
        // Hidden alias, per the naming decision: `.alias` does not appear in help,
        // `.visible_alias` would.
        .alias("x")
        .about("Call any Gitea API operation directly")
        .long_about(
            "Commands for every operation in the bundled Gitea API specification.\n\n\
             Path parameters are accepted positionally or as flags; `--body-file -` reads a \
             JSON request body from stdin, and individual body flags override it. Use \
             `gea raw search <words>` to find an operation by name, summary, or URL path.",
        )
        .subcommand_required(true)
        .arg_required_else_help(true);

    // An unknown group name deliberately falls through to the stub list, so clap can answer
    // with its own "unrecognized subcommand" plus a did-you-mean suggestion.
    match group.and_then(|g| lookup::group(groups, g)) {
        None => {
            for g in groups {
                raw = raw.subcommand(group_stub(g));
            }
            raw = raw.subcommand(crate::search::command());
        }
        Some(g) => {
            let leaf_op = leaf.and_then(|l| lookup::op(ops, g.name, l));
            let mut gc = group_stub(g);
            match leaf_op {
                // The whole point: one full command, not the group's other 197.
                Some(op) => gc = gc.subcommand(leaf_command(op)),
                None => {
                    for op in lookup::ops_in(ops, g) {
                        gc = gc.subcommand(leaf_stub(op));
                    }
                }
            }
            raw = raw.subcommand(gc);
        }
    }
    raw
}

fn group_stub(g: &'static GroupMeta) -> Command {
    Command::new(g.name).about(g.about).subcommand_required(true).arg_required_else_help(true)
}

/// A leaf with a name and an `about` and no arguments: enough for `gea raw repo --help` to
/// list the group, and it costs no `Arg` allocations.
fn leaf_stub(op: &'static OpMeta) -> Command {
    Command::new(op.command).about(about_line(op))
}

fn about_line(op: &'static OpMeta) -> String {
    let summary = if op.summary.is_empty() { op.op_id } else { op.summary };
    match op.deprecated {
        Some(_) => format!("(deprecated) {summary}"),
        None => summary.to_owned(),
    }
}

/// The `--help` body: description, the HTTP call, the token scope, and — when the body has
/// fields too nested to flatten into flags — their names and how to supply them.
fn long_help(op: &'static OpMeta) -> String {
    let mut s = String::new();
    if let Some(note) = op.deprecated {
        s.push_str("DEPRECATED");
        if !note.is_empty() {
            s.push_str(": ");
            s.push_str(note);
        }
        s.push_str("\n\n");
    }
    if !op.description.is_empty() {
        s.push_str(op.description);
        s.push_str("\n\n");
    } else if !op.summary.is_empty() {
        s.push_str(op.summary);
        s.push_str("\n\n");
    }
    s.push_str(&format!("HTTP: {} {}\n", op.method, op.path));
    if let Some(scope) = op.scope {
        s.push_str(&format!("token scope: {scope}\n"));
    }
    if let Some(b) = op.body {
        s.push_str(&format!(
            "request body: {} ({}{})\n",
            b.type_name,
            b.content_type,
            if b.required { ", required" } else { ", optional" }
        ));
        if !b.deep.is_empty() {
            s.push_str(&format!(
                "\nThese body fields are nested too deeply to expose as flags: {}.\n\
                 Supply them with --body-file <PATH> (a JSON object; `-` reads stdin); any \
                 flags you also pass override the file field by field.\n",
                b.deep.join(", ")
            ));
        }
    }
    s
}

/// One operation as a full `Command`, with every argument it accepts.
pub fn leaf_command(op: &'static OpMeta) -> Command {
    let mut cmd = Command::new(op.command).about(about_line(op)).long_about(long_help(op));

    let paged = op.pagination == Pagination::Paged;
    let mut longs: BTreeSet<String> = BTreeSet::new();
    longs.insert("body-file".to_owned());
    longs.insert("dry-run".to_owned());
    if paged {
        longs.insert("paginate".to_owned());
        longs.insert("limit".to_owned());
    }

    // Path parameters, in path order, each as a positional *and* a flag.
    for (i, p) in op.path_params().enumerate() {
        let long = unique_long(&mut longs, p.flag, "param");
        cmd = cmd.arg(positional_arg(p, i + 1)).arg(path_flag_arg(p, &long));
    }
    for p in op.query_params() {
        let long = unique_long(&mut longs, p.flag, "query");
        cmd = cmd.arg(value_arg(query_id(p.flag), &long, p.ty, p.repeatable).help(help_text(
            p.help,
            p.enum_values,
            p.required,
            None,
        )));
    }
    for p in op.form_params() {
        let long = unique_long(&mut longs, p.flag, "form");
        cmd = cmd.arg(value_arg(form_id(p.flag), &long, p.ty, p.repeatable).help(help_text(
            p.help,
            p.enum_values,
            p.required,
            None,
        )));
    }
    if let Some(b) = op.body {
        for f in b.fields {
            let long = unique_long(&mut longs, f.flag, "body");
            cmd = cmd.arg(body_field_arg(f, &long));
        }
    }

    cmd = cmd.arg(body_file_arg()).arg(dry_run_arg());
    if paged {
        cmd = cmd.arg(paginate_arg()).arg(limit_arg());
    }
    cmd
}

/// clap **panics** on a duplicate long name, so every long is funnelled through here: a
/// collision in the generated tables must degrade to an odd flag name, never to a crash.
///
/// The one collision that really happens: `--limit` is reserved on paginated operations for
/// the total number of items to collect across pages, while the API also has a per-page
/// `limit` query parameter. The per-page one becomes `--per-page`, which is clearer anyway.
/// On a non-paginated operation nothing is reserved and `limit` keeps its own name.
fn unique_long(used: &mut BTreeSet<String>, preferred: &str, kind: &str) -> String {
    let mut candidates = vec![preferred.to_owned()];
    if preferred == "limit" {
        candidates.push("per-page".to_owned());
    }
    candidates.push(format!("{kind}-{preferred}"));
    for c in candidates {
        if used.insert(c.clone()) {
            return c;
        }
    }
    let mut n = 2;
    loop {
        let c = format!("{preferred}-{n}");
        if used.insert(c.clone()) {
            return c;
        }
        n += 1;
    }
}

/// Give a computed name the `'static` lifetime clap's `Id` and `Str` require.
///
/// clap only accepts owned `String`s for those when its `string` feature is enabled, and this
/// workspace deliberately does not enable it. The names computed here — namespaced ids and
/// disambiguated longs — are built once per process, for the single subtree the user named
/// (never more than a few dozen strings, and none at all for the stub cases), and are needed
/// until the process exits. Leaking them is an accurate statement of that lifetime rather than
/// a leak in any meaningful sense; the alternative is a self-referential arena or an extra
/// dependency for the same effect.
fn intern(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `owner` → `OWNER`, `diffType` → `DIFF_TYPE`. Also used by [`crate::search`] when it prints
/// a full invocation, so the placeholder a search hit shows is the one `--help` shows.
pub fn value_name(wire: &str) -> String {
    let mut out = String::with_capacity(wire.len() + 2);
    for (i, ch) in wire.char_indices() {
        if ch == '-' || ch == '_' {
            out.push('_');
        } else {
            if ch.is_ascii_uppercase() && i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(ch.to_uppercase());
        }
    }
    out
}

fn positional_arg(p: &'static ParamMeta, index: usize) -> Arg {
    // Path parameters are read as strings whatever their declared type: the value is
    // interpolated into a URL as text, and typing it would make the flag-versus-positional
    // comparison in `bind` depend on the type. A non-numeric `{index}` reaches the server and
    // comes back as a 404 with a real message, which is a fair trade for that simplicity.
    Arg::new(intern(path_pos_id(p.flag)))
        .index(index)
        .value_name(intern(value_name(p.wire)))
        .required(false)
        .value_hint(value_hint(p.ty))
        .help(help_text(p.help, p.enum_values, true, Some(&format!("or --{}", p.flag))))
}

fn path_flag_arg(p: &'static ParamMeta, long: &str) -> Arg {
    Arg::new(intern(path_flag_id(p.flag)))
        .long(intern(long.to_owned()))
        .value_name(intern(value_name(p.wire)))
        .required(false)
        .action(ArgAction::Set)
        .value_hint(value_hint(p.ty))
        .help(help_text(
            p.help,
            p.enum_values,
            true,
            Some(&format!("or positional <{}>", value_name(p.wire))),
        ))
}

fn body_field_arg(f: &'static BodyField, long: &str) -> Arg {
    value_arg(body_id(f.flag), long, f.ty, f.ty == ValueTy::List).help(help_text(
        f.help,
        f.enum_values,
        f.required,
        Some("body field"),
    ))
}

/// The single place where a [`ValueTy`] becomes a clap value parser.
///
/// [`mod@crate::bind`] reads these matches back with `get_one::<T>`, which panics on a type
/// mismatch, so that function and this one are one decision in two places — change them
/// together.
fn value_arg(id: String, long: &str, ty: ValueTy, repeatable: bool) -> Arg {
    let mut a = Arg::new(intern(id)).long(intern(long.to_owned())).required(false);
    a = match ty {
        // `--draft` and `--draft=false` both work; `require_equals` stops `--draft` from
        // swallowing the next positional, which for these commands is a path parameter.
        ValueTy::Bool => a
            .value_name("BOOL")
            .value_parser(value_parser!(bool))
            .num_args(0..=1)
            .require_equals(true)
            .default_missing_value("true"),
        ValueTy::Int => a.value_name("N").value_parser(value_parser!(i64)),
        ValueTy::Float => a.value_name("NUM").value_parser(value_parser!(f64)),
        ValueTy::DateTime => a.value_name("RFC3339"),
        ValueTy::File => a.value_name("PATH").value_hint(ValueHint::FilePath),
        ValueTy::Json => a.value_name("JSON"),
        ValueTy::List | ValueTy::Str => a.value_name("VALUE"),
    };
    if repeatable && ty != ValueTy::Bool {
        a.action(ArgAction::Append)
    } else {
        a.action(ArgAction::Set)
    }
}

fn value_hint(ty: ValueTy) -> ValueHint {
    match ty {
        ValueTy::File => ValueHint::FilePath,
        _ => ValueHint::Other,
    }
}

/// Help text for one argument, including the spec's enum values.
///
/// The values are a **hint**, never a `value_parser` allowlist. Our spec is pinned to one
/// Gitea version while the instance may be newer and accept values this build has never
/// heard of; rejecting them here would make `gea` refuse requests the API would honour, and
/// the user would have no way around it short of dropping to `gea api`. Being permissive
/// costs at most a server-side 422, which carries a better message than we could invent.
fn help_text(help: &str, enum_values: &[&str], required: bool, note: Option<&str>) -> String {
    let mut s = String::new();
    if required {
        s.push_str("(required) ");
    }
    s.push_str(help);
    if !enum_values.is_empty() {
        if !s.ends_with(' ') && !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("[values: {}; not enforced]", enum_values.join(", ")));
    }
    if let Some(n) = note {
        s.push_str(&format!(" [{n}]"));
    }
    s.trim().to_owned()
}

fn body_file_arg() -> Arg {
    Arg::new(ID_BODY_FILE)
        .long("body-file")
        .value_name("PATH")
        .value_hint(ValueHint::FilePath)
        .action(ArgAction::Set)
        .help("Read the JSON request body from PATH; `-` reads stdin. Body flags override it")
}

fn dry_run_arg() -> Arg {
    Arg::new(ID_DRY_RUN)
        .long("dry-run")
        .action(ArgAction::SetTrue)
        .help("Print the method, URL, and body that would be sent, and exit")
}

fn paginate_arg() -> Arg {
    Arg::new(ID_PAGINATE)
        .long("paginate")
        .action(ArgAction::SetTrue)
        .help("Follow pagination and return every page as one list")
}

fn limit_arg() -> Arg {
    Arg::new(ID_LIMIT)
        .long("limit")
        .value_name("N")
        .value_parser(value_parser!(u64).range(1..))
        .action(ArgAction::Set)
        .help("Stop after N items in total (not per page)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, GROUPS, OPS};

    fn argv(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn peeked(words: &[&str]) -> Option<(Option<String>, Option<String>)> {
        let a = argv(words);
        peek(&a).map(|p| (p.group.map(str::to_owned), p.leaf.map(str::to_owned)))
    }

    #[test]
    fn peek_recognises_the_three_depths_of_a_raw_invocation() {
        assert_eq!(peeked(&["gea", "raw"]), Some((None, None)));
        assert_eq!(peeked(&["gea", "raw", "repo"]), Some((Some("repo".into()), None)));
        assert_eq!(
            peeked(&["gea", "raw", "repo", "create-pull-request"]),
            Some((Some("repo".into()), Some("create-pull-request".into())))
        );
    }

    /// The whole point of peeking: an invocation that is not layer 2 must not build any of it.
    #[test]
    fn peek_declines_non_raw_invocations() {
        assert_eq!(peeked(&["gea", "--version"]), None);
        assert_eq!(peeked(&["gea", "pr", "list"]), None);
        assert_eq!(peeked(&["gea"]), None);
    }

    /// A global option's *value* is not the subcommand. Without the VALUE_FLAGS list this
    /// returned `Some(group: "raw")` for `--host x raw repo get` — or worse, `None`.
    #[test]
    fn peek_skips_global_flags_and_their_values() {
        assert_eq!(
            peeked(&["gea", "--host", "x", "raw", "repo", "get"]),
            Some((Some("repo".into()), Some("get".into())))
        );
        assert_eq!(
            peeked(&["gea", "--host=x", "raw", "repo", "get"]),
            Some((Some("repo".into()), Some("get".into())))
        );
        assert_eq!(
            peeked(&["gea", "raw", "--host", "x", "repo", "get"]),
            Some((Some("repo".into()), Some("get".into())))
        );
        // `--help` takes no value, so the next word is still the group.
        assert_eq!(peeked(&["gea", "raw", "--help"]), Some((None, None)));
    }

    #[test]
    fn peek_accepts_the_hidden_x_alias_and_honours_a_double_dash() {
        assert_eq!(
            peeked(&["gea", "x", "repo", "get"]),
            Some((Some("repo".into()), Some("get".into())))
        );
        // After `--`, a leading dash is data, not a flag.
        assert_eq!(
            peeked(&["gea", "raw", "repo", "--", "-weird"]),
            Some((Some("repo".into()), Some("-weird".into())))
        );
    }

    /// The laziness guarantee, and the test that stops someone "simplifying" this into eager
    /// construction: with no group named, the tree holds one stub per group (plus `search`),
    /// never one command per operation.
    #[test]
    fn build_with_no_group_is_a_handful_of_stubs_not_every_operation() {
        let cmd = build(OPS, GROUPS, None, None);
        let raw = cmd.find_subcommand("raw").unwrap();
        let subs: Vec<&str> = raw.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(subs.len(), GROUPS.len() + 1, "groups plus `search`, got {subs:?}");
        assert!(subs.len() < OPS.len(), "must not be one command per operation");
        // And the stubs carry no arguments at all.
        for s in raw.get_subcommands() {
            if s.get_name() != "search" {
                assert_eq!(s.get_arguments().count(), 0, "{} allocated args", s.get_name());
            }
        }
    }

    #[test]
    fn build_with_a_group_lists_only_that_groups_leaves_by_name() {
        let cmd = build(OPS, GROUPS, Some("repo"), None);
        let g = cmd.find_subcommand("raw").unwrap().find_subcommand("repo").unwrap();
        let leaves: Vec<&str> = g.get_subcommands().map(|c| c.get_name()).collect();
        assert_eq!(leaves.len(), 5, "the repo fixture group has 5 ops: {leaves:?}");
        assert!(leaves.contains(&"get-contents"));
        for l in g.get_subcommands() {
            assert_eq!(l.get_arguments().count(), 0, "{} should be a stub", l.get_name());
        }
    }

    #[test]
    fn build_with_a_leaf_builds_exactly_one_full_command() {
        let cmd = build(OPS, GROUPS, Some("repo"), Some("get"));
        let g = cmd.find_subcommand("raw").unwrap().find_subcommand("repo").unwrap();
        assert_eq!(g.get_subcommands().count(), 1);
        let leaf = g.find_subcommand("get").unwrap();
        // owner and repo, each positional + flag, plus --body-file and --dry-run.
        let ids: Vec<String> = leaf.get_arguments().map(|a| a.get_id().to_string()).collect();
        assert!(ids.contains(&path_pos_id("owner")));
        assert!(ids.contains(&path_flag_id("owner")));
        assert!(ids.contains(&ID_BODY_FILE.to_owned()));
        assert!(ids.contains(&ID_DRY_RUN.to_owned()));
    }

    /// An unknown group must still produce the stub list, so clap answers with its own
    /// did-you-mean instead of us inventing an error.
    #[test]
    fn an_unknown_group_falls_back_to_the_stub_list() {
        let cmd = build(OPS, GROUPS, Some("reop"), Some("get"));
        let raw = cmd.find_subcommand("raw").unwrap();
        assert_eq!(raw.get_subcommands().count(), GROUPS.len() + 1);
    }

    /// `gea raw search <words>` must exist whenever no group was named, or discoverability
    /// depends on already knowing the group.
    #[test]
    fn search_is_reachable_from_the_bare_raw_command() {
        let cmd = build(OPS, GROUPS, Some("search"), Some("pull"));
        assert!(
            cmd.clone().try_get_matches_from(["gea", "raw", "search", "pull", "request"]).is_ok()
        );
    }

    /// The golden test the plan asks for: every operation in the table renders help without
    /// panicking. A duplicate long name or a duplicate argument id is a clap *panic*, so this
    /// is what catches a table whose contents would crash the binary.
    #[test]
    fn every_op_renders_long_help_without_panicking() {
        for op in OPS {
            let mut cmd = build(OPS, GROUPS, Some(op.group), Some(op.command));
            let help = cmd.render_long_help().to_string();
            assert!(!help.is_empty());
            let mut leaf = cmd
                .find_subcommand_mut("raw")
                .unwrap()
                .find_subcommand_mut(op.group)
                .unwrap()
                .find_subcommand_mut(op.command)
                .unwrap()
                .clone();
            let leaf_help = leaf.render_long_help().to_string();
            assert!(leaf_help.contains("--body-file"), "{} lacks --body-file", op.op_id);
            assert!(leaf_help.contains("--dry-run"), "{} lacks --dry-run", op.op_id);
            assert!(leaf_help.contains(op.path), "{} lacks its HTTP path", op.op_id);
        }
    }

    #[test]
    fn a_nested_body_names_its_deep_fields_and_points_at_body_file() {
        let help = fixtures::leaf_help("repo", "create-pull-request");
        assert!(help.contains("nested too deeply"));
        assert!(help.contains("milestone_object"), "{help}");
        assert!(help.contains("--body-file"));
    }

    #[test]
    fn a_deprecated_op_says_so_in_help() {
        let help = fixtures::leaf_help("misc", "markdown-raw");
        assert!(help.contains("DEPRECATED"), "{help}");
        assert!(help.contains("use `misc markup`"), "{help}");
    }

    /// Our pinned spec is not the server's opinion: an enum value it never heard of must still
    /// reach the API, which is the only thing that can authoritatively reject it.
    #[test]
    fn enum_values_are_a_hint_and_do_not_reject_unknown_input() {
        let cmd = build(OPS, GROUPS, Some("issue"), Some("list"));
        let m = cmd.try_get_matches_from(["gea", "raw", "issue", "list", "--state", "bananas"]);
        assert!(m.is_ok(), "an unlisted enum value must be accepted: {:?}", m.err());
        let help = fixtures::leaf_help("issue", "list");
        assert!(help.contains("not enforced"), "{help}");
    }

    /// `--limit` (total items) and the API's per-page `limit` query parameter both want the
    /// same long name, and clap panics on a duplicate. The per-page one moves.
    #[test]
    fn pagination_limit_and_a_limit_query_param_coexist() {
        let cmd = build(OPS, GROUPS, Some("issue"), Some("list"));
        let leaf = cmd
            .find_subcommand("raw")
            .unwrap()
            .find_subcommand("issue")
            .unwrap()
            .find_subcommand("list")
            .unwrap();
        let longs: Vec<&str> = leaf.get_arguments().filter_map(|a| a.get_long()).collect();
        assert!(longs.contains(&"limit"), "{longs:?}");
        assert!(longs.contains(&"per-page"), "{longs:?}");
        assert!(longs.contains(&"paginate"), "{longs:?}");
    }

    #[test]
    fn value_name_splits_camel_case_so_placeholders_read_well() {
        assert_eq!(value_name("owner"), "OWNER");
        assert_eq!(value_name("diffType"), "DIFF_TYPE");
        assert_eq!(value_name("tree-path"), "TREE_PATH");
    }
}

/// The engine driven by the **real** generated tables rather than the fixtures.
///
/// The implementation deliberately takes `ops`/`groups` as parameters and never reaches for
/// these statics, so that it can be developed and reviewed independently of the emitter. The
/// tests, though, are the right place to close the loop: these are the checks that a table of
/// 506 real operations cannot make the engine panic — and a duplicate long name or duplicate
/// argument id is exactly a clap panic, i.e. a crash whose cause lives in generated data.
#[cfg(test)]
mod generated_table {
    use super::*;
    use gitea_client::meta::{GROUPS, OPS};
    use gitea_core::types::RepoSlug;

    /// M5's golden test: `--help` renders for every operation, without panicking.
    #[test]
    fn every_generated_op_builds_parses_and_renders_help() {
        let ctx = RepoSlug::new("owner-from-context", "repo-from-context");
        for op in OPS {
            let cmd = build(OPS, GROUPS, Some(op.group), Some(op.command));

            let mut leaf = cmd
                .clone()
                .find_subcommand_mut("raw")
                .and_then(|c| c.find_subcommand_mut(op.group))
                .and_then(|c| c.find_subcommand_mut(op.command))
                .unwrap_or_else(|| {
                    panic!("{} is unreachable as `{} {}`", op.op_id, op.group, op.command)
                })
                .clone();
            let help = leaf.render_long_help().to_string();
            assert!(help.contains("--body-file"), "{} lacks --body-file", op.op_id);
            assert!(help.contains("--dry-run"), "{} lacks --dry-run", op.op_id);

            // Every path parameter supplied as a flag, then bound: proves each of the 326 real
            // path templates renders, including the two dotted ones.
            let mut argv: Vec<String> =
                vec!["gea".into(), "raw".into(), op.group.into(), op.command.into()];
            for p in op.path_params() {
                argv.push(format!("--{}", p.flag));
                argv.push("v".into());
            }
            let m = cmd
                .try_get_matches_from(&argv)
                .unwrap_or_else(|e| panic!("{}: {argv:?} did not parse: {e}", op.op_id));
            let leaf_matches = m
                .subcommand_matches("raw")
                .and_then(|m| m.subcommand_matches(op.group))
                .and_then(|m| m.subcommand_matches(op.command))
                .expect("leaf matches");
            let plan = crate::bind::bind(op, leaf_matches, Some(&ctx), &mut std::io::empty())
                .unwrap_or_else(|e| panic!("{}: {e}", op.op_id));
            assert!(!plan.path.contains('{'), "{}: {}", op.op_id, plan.path);
        }
    }

    /// The laziness claim against the real table: the bare `raw` command holds one stub per
    /// group, not 506 commands with their ~3,000 arguments.
    #[test]
    fn the_bare_raw_command_holds_one_stub_per_group() {
        let cmd = build(OPS, GROUPS, None, None);
        let raw = cmd.find_subcommand("raw").unwrap();
        assert_eq!(raw.get_subcommands().count(), GROUPS.len() + 1);
        assert!(GROUPS.len() < 30, "a group count near 506 would defeat the purpose");
        assert_eq!(
            raw.get_subcommands().map(|s| s.get_arguments().count()).sum::<usize>(),
            1,
            "only `search` may own an argument at this depth"
        );
    }
}

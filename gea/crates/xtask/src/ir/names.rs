//! Name mangling. **The single source of truth for every generated name.**
//!
//! Everything downstream reads names from here: the clap command in layer 2, the SDK function
//! it dispatches to, the module both live in, and the `spec/name-lock.toml` entry that makes
//! the pair a versioned contract. Nothing else is allowed to do string casing — if two places
//! independently kebab-cased an `operationId`, they would agree today and disagree after the
//! first spec bump, and the symptom would be a layer-2 flag that no longer matches the
//! function parameter it fills.
//!
//! ## The algorithm
//!
//! 1. Normalize the `operationId` to a word list ([`words`]). This is what makes
//!    `repoCreatePullRequest` and `ListActionRuns` normalize identically — the spec mixes 490
//!    camelCase ids with 11 PascalCase ones, and downstream must not be able to tell.
//! 2. An `overrides.toml [op."<id>"]` entry wins outright.
//! 3. Else, if word 0 is a known group prefix, it is the group and the rest is the command.
//!    Gitea's ids are overwhelmingly `<group><Verb><Noun>`, so this covers most of the 482.
//! 4. Else the group comes from `tags[0]` through a fixed map.
//! 5. `command = kebab(rest)`, `fn_name = snake(rest)`, `module = snake(group)`.
//!
//! ## Collisions are fatal
//!
//! [`check_unique`] asserts 482 unique `(module, fn_name)` and 482 unique `(group, command)`.
//! A collision is a hard error naming both `operationId`s and the exact `overrides.toml` entry
//! to add. It is never auto-disambiguated: a generator that quietly appends a `_2` produces a
//! public command name nobody chose, and the next spec bump can move the `_2` to the other
//! operation.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use crate::Result;
use crate::overrides::Overrides;

/// A word 0 that identifies the group directly.
///
/// Kept as a fixed list rather than "any word that matches a tag" because it must be stable:
/// if a future Gitea added an `issue` tag, "derive prefixes from tags" would silently start
/// grouping operations differently.
pub const GROUP_PREFIXES: [&str; 12] = [
    "activitypub",
    "admin",
    "issue",
    "misc",
    "notify",
    "org",
    "package",
    "repo",
    "settings",
    "team",
    "topic",
    "user",
];

/// `tags[0]` → group, for the operations whose id carries no group prefix.
///
/// Gitea's tag vocabulary is the long form (`repository`, `organization`); the CLI's is the
/// short one, because that is what people type.
pub const TAG_GROUPS: [(&str, &str); 10] = [
    ("activitypub", "activitypub"),
    ("admin", "admin"),
    ("issue", "issue"),
    ("miscellaneous", "misc"),
    ("notification", "notification"),
    ("organization", "org"),
    ("package", "package"),
    ("repository", "repo"),
    ("settings", "settings"),
    ("user", "user"),
];

/// A Rust identifier known to be valid: raw-escaped for keywords, prefixed when it would
/// otherwise start with a digit.
///
/// Stored as a `String` rather than a `proc_macro2::Ident` because the IR must be `Ord`,
/// `Serialize`-able (for `--dump-ir` and `name-lock.toml`) and comparable in assertions, none
/// of which `Ident` supports — and because a `proc_macro2::Ident` carries a `Span`, which is
/// meaningless here. Emitters call [`Ident::to_ident`] at the last moment, which is also where
/// a malformed identifier would panic *during generation* rather than 40k lines later during
/// `cargo build`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Ident(String);

impl Ident {
    /// Wraps a string that is already a valid identifier. Panics otherwise, on purpose: an
    /// invalid identifier reaching the emitters is a generator bug, not a spec problem.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(is_valid_ident(&s), "not a valid Rust identifier: {s:?}");
        Ident(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when the identifier needs `r#` to be written in source.
    pub fn is_raw(&self) -> bool {
        RAW_ESCAPABLE_KEYWORDS.contains(&self.0.as_str())
    }

    /// The `proc_macro2::Ident` an emitter interpolates into `quote!`.
    pub fn to_ident(&self) -> proc_macro2::Ident {
        let span = proc_macro2::Span::call_site();
        if self.is_raw() {
            proc_macro2::Ident::new_raw(&self.0, span)
        } else {
            proc_macro2::Ident::new(&self.0, span)
        }
    }
}

impl fmt::Display for Ident {
    /// Displays with the `r#` prefix, which is how the identifier appears in source.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_raw() {
            f.write_str("r#")?;
        }
        f.write_str(&self.0)
    }
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Keywords that `r#` can escape. Gitea has fields named `type` and `ref`, so this is not
/// hypothetical.
const RAW_ESCAPABLE_KEYWORDS: [&str; 47] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let",
    "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "static", "struct", "trait", "true", "try", "type", "typeof", "unsafe", "unsized", "use",
    "virtual", "where", "while",
];

/// Keywords `r#` cannot escape. These get a trailing `_` instead.
const UNESCAPABLE_KEYWORDS: [&str; 4] = ["crate", "self", "Self", "super"];

/// Splits an identifier-ish string into lowercase words.
///
/// The rules, and what each one is for:
///
/// | rule | example | why |
/// | --- | --- | --- |
/// | lowercase a leading capital | `ListActionRuns` → `listActionRuns` | makes Pascal and camel ids indistinguishable |
/// | split lower→upper | `createPull` → `create`,`pull` | the common case |
/// | split digit→upper | `sha256Hash` → `sha256`,`hash` | keeps a digit with its word |
/// | split `_` and `-` | `user-id` → `user`,`id` | wire names use both |
/// | uppercase run is one word, ending one char before a following lowercase | `getGeneralAPISettings` → `get`,`general`,`api`,`settings` | acronyms stay whole |
///
/// Note what the last rule costs: `OAuth2Application` becomes `o`,`auth2`,`application`,
/// because no rule can know `OAuth` is a word. The five affected operations are corrected in
/// `overrides.toml` rather than by teaching this function an acronym dictionary — a splitter
/// with a dictionary is a splitter whose output nobody can predict from the input.
pub fn words(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();

    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c == '.' || c == ' ' || c == '/' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }

        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();

        let boundary = if c.is_ascii_uppercase() {
            match prev {
                // `createPull`, `sha256Hash`: a capital after a lowercase or a digit always
                // starts a word.
                Some(p) if p.is_lowercase() || p.is_ascii_digit() => true,
                // `APISettings`: the last capital of a run belongs to the following word.
                Some(p) if p.is_ascii_uppercase() => next.is_some_and(char::is_lowercase),
                _ => false,
            }
        } else if c.is_lowercase() {
            // `2application` after a digit-terminated word.
            prev.is_some_and(|p| p.is_ascii_digit())
        } else {
            false
        };

        if boundary && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn kebab(words: &[String]) -> String {
    words.join("-")
}

pub fn snake(words: &[String]) -> String {
    words.join("_")
}

pub fn pascal(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// A snake_case identifier, keyword- and digit-safe.
pub fn ident_snake(s: &str) -> Ident {
    sanitize(&snake(&words(s)))
}

/// A PascalCase type or variant identifier.
pub fn ident_pascal(s: &str) -> Ident {
    sanitize(&pascal(&words(s)))
}

/// Applies the two rules that turn a well-formed word joining into a legal identifier:
/// `r#` for escapable keywords, `n` for a leading digit.
///
/// A leading digit gets `n` rather than `_` because `_42` reads as "unused" to a Rust
/// programmer, while `n42` reads as "number 42".
fn sanitize(s: &str) -> Ident {
    let mut s = s.to_owned();
    if s.is_empty() {
        // Cannot happen for a real operationId, but an empty identifier reaching `quote!`
        // produces an incomprehensible error, so fail here with a name.
        panic!("empty identifier after mangling");
    }
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        s.insert(0, 'n');
    }
    if UNESCAPABLE_KEYWORDS.contains(&s.as_str()) {
        s.push('_');
    }
    s.retain(|c| c == '_' || c.is_ascii_alphanumeric());
    Ident::new(s)
}

/// The four names one operation contributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpName {
    /// Layer-2 group, as typed: `gea raw <group> <command>`.
    pub group: String,
    /// Layer-2 command, kebab-case.
    pub command: String,
    /// Module the generated client function lives in.
    pub module: Ident,
    /// The generated client function.
    pub fn_name: Ident,
    /// Whether `overrides.toml` decided this name.
    pub overridden: bool,
}

/// Derives the names for one operation.
pub fn op_name(op_id: &str, tags: &[String], overrides: &Overrides) -> Result<OpName> {
    if let Some(ov) = overrides.op.get(op_id) {
        let group_words = words(&ov.group);
        let command_words = words(&ov.command);
        if command_words.is_empty() {
            bail!("overrides.toml [op.{op_id}] has an empty command");
        }
        return Ok(OpName {
            group: kebab(&group_words),
            command: kebab(&command_words),
            module: sanitize(&snake(&group_words)),
            fn_name: sanitize(&snake(&command_words)),
            overridden: true,
        });
    }

    let w = words(op_id);
    if w.is_empty() {
        bail!("operationId {op_id:?} normalizes to no words");
    }

    let (group, rest): (String, &[String]) =
        if GROUP_PREFIXES.contains(&w[0].as_str()) && w.len() > 1 {
            (w[0].clone(), &w[1..])
        } else {
            let tag = tags.first().map(String::as_str).unwrap_or_default();
            let Some((_, group)) = TAG_GROUPS.iter().find(|(t, _)| *t == tag) else {
                bail!(
                    "operationId {op_id:?} has no group prefix and its tag {tag:?} is not in \
                 names::TAG_GROUPS.\n  Either add the tag to TAG_GROUPS (if Gitea grew a new \
                 route group) or pin the operation explicitly:\n\n    \
                 [op.{op_id}]\n    group = \"...\"\n    command = \"...\"\n"
                );
            };
            ((*group).to_owned(), &w[..])
        };

    if rest.is_empty() {
        bail!(
            "operationId {op_id:?} is nothing but a group prefix, so there is no command name \
             to derive. Pin it:\n\n    [op.{op_id}]\n    group = \"{group}\"\n    \
             command = \"...\"\n"
        );
    }

    Ok(OpName {
        module: sanitize(&snake(&words(&group))),
        group,
        command: kebab(rest),
        fn_name: sanitize(&snake(rest)),
        overridden: false,
    })
}

/// Asserts both uniqueness invariants over the whole operation set.
///
/// Reports *every* collision rather than the first, because collisions arrive in batches (a
/// group prefix that turns out to be ambiguous produces several at once) and because the error
/// message is the fix: it contains the `overrides.toml` entry to paste.
pub fn check_unique(names: &[(String, OpName)]) -> Result<()> {
    let mut by_fn: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    let mut by_command: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();

    for (op_id, n) in names {
        by_fn.entry((n.module.as_str(), n.fn_name.as_str())).or_default().push(op_id);
        by_command.entry((n.group.as_str(), n.command.as_str())).or_default().push(op_id);
    }

    let mut report = String::new();
    for ((module, fn_name), ids) in &by_fn {
        if ids.len() > 1 {
            report.push_str(&collision_note(
                &format!("client function {module}::{fn_name}"),
                ids,
                names,
            ));
        }
    }
    for ((group, command), ids) in &by_command {
        if ids.len() > 1 {
            report.push_str(&collision_note(
                &format!("command `gea raw {group} {command}`"),
                ids,
                names,
            ));
        }
    }

    if report.is_empty() {
        return Ok(());
    }
    bail!(
        "name collisions in the generated surface:\n{report}\n\
         Collisions are never auto-disambiguated. A generated `_2` suffix is a public command \
         name that nobody chose, and the next spec bump can move it to the other operation."
    );
}

fn collision_note(what: &str, ids: &[&str], names: &[(String, OpName)]) -> String {
    let mut s = format!("\n  {what} is claimed by {} operations:\n", ids.len());
    for id in ids {
        let n = names.iter().find(|(o, _)| o == id).map(|(_, n)| n);
        match n {
            Some(n) => s.push_str(&format!("    {id}  ->  {} {}\n", n.group, n.command)),
            None => s.push_str(&format!("    {id}\n")),
        }
    }
    s.push_str("  resolve by pinning all but one in crates/xtask/src/overrides.toml:\n");
    for id in ids {
        s.push_str(&format!("\n    [op.{id}]\n    group = \"...\"\n    command = \"...\"\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov() -> Overrides {
        Overrides::load().unwrap()
    }

    fn name(op_id: &str, tag: &str) -> OpName {
        op_name(op_id, &[tag.to_owned()], &ov()).unwrap()
    }

    #[test]
    fn pascal_and_camel_normalize_identically() {
        // The whole reason `words` lowercases a leading capital. If Pascal and camel ids
        // produced different word lists, the 11 PascalCase operations would need bespoke
        // handling everywhere downstream instead of in one override table.
        assert_eq!(words("listActionRuns"), words("ListActionRuns"));
        assert_eq!(words("repoCreatePullRequest"), ["repo", "create", "pull", "request"]);
    }

    #[test]
    fn uppercase_runs_stay_one_word() {
        assert_eq!(words("getGeneralAPISettings"), ["get", "general", "api", "settings"]);
        assert_eq!(words("repoGetRawFileOrLFS"), ["repo", "get", "raw", "file", "or", "lfs"]);
        assert_eq!(words("repoGetByID"), ["repo", "get", "by", "id"]);
        assert_eq!(words("userCurrentDeleteGPGKey"), ["user", "current", "delete", "gpg", "key"]);
    }

    #[test]
    fn digits_stay_attached_to_their_word() {
        assert_eq!(words("sha256Hash"), ["sha256", "hash"]);
        // Documents the known wart that `overrides.toml` exists to paper over.
        assert_eq!(words("OAuth2Application"), ["o", "auth2", "application"]);
    }

    #[test]
    fn wire_separators_split() {
        assert_eq!(words("user-id"), ["user", "id"]);
        assert_eq!(words("status_types"), ["status", "types"]);
    }

    /// Goldens for all 11 PascalCase operationIds. These are public command names; if one
    /// changes, `spec/name-lock.toml` must change too, and that is a reviewed breaking change.
    #[test]
    fn all_eleven_pascal_case_ids_land_where_intended() {
        let expected: [(&str, &str, &str, &str); 11] = [
            ("GetWorkflowRun", "run", "view", "view"),
            ("ListActionTasks", "task", "list", "list"),
            ("ActionsListRepositoryWorkflows", "workflow", "list", "list"),
            ("ActionsGetWorkflow", "workflow", "view", "view"),
            ("ActionsDispatchWorkflow", "workflow", "dispatch", "dispatch"),
            ("ActionsEnableWorkflow", "workflow", "enable", "enable"),
            ("ActionsDisableWorkflow", "workflow", "disable", "disable"),
            ("ActionsListWorkflowRuns", "workflow", "runs", "runs"),
            ("GetBlob", "git", "blob", "blob"),
            ("GetTree", "git", "tree", "tree"),
            ("GetAnnotatedTag", "git", "annotated-tag", "annotated_tag"),
        ];
        for (op_id, group, command, fn_name) in expected {
            let n = name(op_id, "repository");
            assert_eq!(n.group, group, "{op_id} group");
            assert_eq!(n.command, command, "{op_id} command");
            assert_eq!(n.fn_name.as_str(), fn_name, "{op_id} fn");
            assert_eq!(n.module.as_str(), group.replace('-', "_"), "{op_id} module");
            assert!(n.overridden, "{op_id} should be overridden");
        }
    }

    #[test]
    fn representative_camel_case_ids_use_the_group_prefix() {
        let n = name("repoCreatePullRequest", "repository");
        assert_eq!((n.group.as_str(), n.command.as_str()), ("repo", "create-pull-request"));
        assert_eq!(n.fn_name.as_str(), "create_pull_request");
        assert_eq!(n.module.as_str(), "repo");
        assert!(!n.overridden);

        let n = name("issueGetComment", "issue");
        assert_eq!((n.group.as_str(), n.command.as_str()), ("issue", "get-comment"));

        let n = name("notifyReadList", "notification");
        assert_eq!((n.group.as_str(), n.command.as_str()), ("notify", "read-list"));
    }

    #[test]
    fn ids_without_a_group_prefix_fall_back_to_the_tag() {
        // `getGeneralAPISettings` starts with `get`, not a group, so `tags[0]` decides.
        let n = name("getGeneralAPISettings", "settings");
        assert_eq!(n.group, "settings");
        assert_eq!(n.command, "get-general-api-settings");

        let n = name("renderMarkdown", "miscellaneous");
        assert_eq!(n.group, "misc", "the tag map must shorten `miscellaneous`");
    }

    #[test]
    fn an_unmapped_tag_is_an_error_that_says_what_to_add() {
        let err = op_name("somethingNew", &["quantum".to_owned()], &ov()).unwrap_err().to_string();
        assert!(err.contains("TAG_GROUPS"), "{err}");
        assert!(err.contains("[op.somethingNew]"), "{err}");
    }

    #[test]
    fn keywords_become_raw_identifiers() {
        // Gitea has fields named `type` and `ref`. Without `r#`, the models emitter produces
        // 42k lines that do not compile, and the first error points at a struct field.
        assert_eq!(ident_snake("type").to_string(), "r#type");
        assert_eq!(ident_snake("ref").to_string(), "r#ref");
        assert_eq!(ident_snake("ref").as_str(), "ref", "as_str is the bare name");
        // `crate` cannot be raw-escaped, so it gets a suffix instead of producing a syntax
        // error inside `quote!`.
        assert_eq!(ident_snake("crate").to_string(), "crate_");
    }

    #[test]
    fn leading_digits_get_a_letter_not_an_underscore() {
        // A leading digit is not a legal identifier start. `_2fa` reads as "unused" to a Rust
        // programmer, so the prefix is `n`. The `2_fa` in the middle is the documented digit
        // boundary doing its job — `2` and `fa` really are separate words.
        assert_eq!(ident_snake("2fa").as_str(), "n2_fa");
        assert_eq!(ident_snake("2").as_str(), "n2");
        // Digits that are part of a word keep it whole.
        assert_eq!(ident_snake("sha256").as_str(), "sha256");
    }

    #[test]
    fn to_ident_round_trips_raw_identifiers() {
        assert_eq!(ident_snake("type").to_ident().to_string(), "r#type");
        assert_eq!(ident_snake("owner").to_ident().to_string(), "owner");
    }

    #[test]
    fn collision_detector_names_both_operations_and_the_fix() {
        // Fed a synthetic collision: two operations that both want `gea raw repo get`. The
        // real spec has none, which is exactly why this has to be tested synthetically —
        // otherwise the detector is dead code that nobody has ever seen run.
        let dup = OpName {
            group: "repo".into(),
            command: "get".into(),
            module: Ident::new("repo"),
            fn_name: Ident::new("get"),
            overridden: false,
        };
        let names = vec![("repoGet".to_owned(), dup.clone()), ("repoFetch".to_owned(), dup)];
        let err = check_unique(&names).unwrap_err().to_string();

        assert!(err.contains("repoGet"), "must name the first offender: {err}");
        assert!(err.contains("repoFetch"), "must name the second offender: {err}");
        assert!(err.contains("[op.repoGet]"), "must paste the fix: {err}");
        assert!(err.contains("gea raw repo get"), "must name the colliding command: {err}");
        assert!(err.contains("repo::get"), "must name the colliding function: {err}");
    }

    #[test]
    fn check_unique_passes_on_distinct_names() {
        let a = OpName {
            group: "repo".into(),
            command: "get".into(),
            module: Ident::new("repo"),
            fn_name: Ident::new("get"),
            overridden: false,
        };
        let b = OpName {
            group: "repo".into(),
            command: "list".into(),
            module: Ident::new("repo"),
            fn_name: Ident::new("list"),
            overridden: false,
        };
        check_unique(&[("a".to_owned(), a), ("b".to_owned(), b)]).unwrap();
    }
}

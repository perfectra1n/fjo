//! `spec/name-lock.toml` — command names as a versioned contract.
//!
//! Every operation's `(group, command, fn)` triple is committed. Lowering compares against the
//! lock and **hard-errors on any change** unless a human passes `--accept-renames`.
//!
//! The failure this prevents is specific and expensive. `gea raw repo create-pull-request` is
//! a name users put in scripts and CI. Its spelling is derived mechanically from an
//! `operationId`, so a Gitea release that renames `repoCreatePullRequest` — or that adds an
//! operation whose name collides and forces a rename — would silently change the CLI's surface
//! at the next `cargo xtask update-spec`. The only symptom would be users' scripts failing with
//! "unrecognized subcommand", weeks later, with no changelog entry explaining it.
//!
//! With the lock, that becomes: codegen fails, a human sees a diff of exactly which commands
//! moved, and passing `--accept-renames` produces text that goes straight into the changelog.
//!
//! Asymmetry is deliberate:
//!
//! - a **new** `operationId` is appended silently — new API is not a breaking change;
//! - a **renamed** one needs `--accept-renames`;
//! - a **removed** one needs `--accept-removals`, because a command disappearing is the
//!   harshest thing we can do to a user and should never happen by accident.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ir::Ir;
use crate::spec;

#[derive(Debug, Clone, Copy)]
pub struct Mode {
    pub accept_renames: bool,
    pub accept_removals: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub group: String,
    pub command: String,
    /// The generated client function. Named `fn` on the wire because that is what it is; the
    /// field is renamed here only because `fn` is a Rust keyword.
    #[serde(rename = "fn")]
    pub fn_name: String,
}

impl Entry {
    fn invocation(&self) -> String {
        format!("gea raw {} {}", self.group, self.command)
    }
}

/// Compares the IR against the committed lock, writing it back when permitted.
pub fn reconcile(root: &Path, ir: &Ir, mode: Mode) -> Result<()> {
    let path = spec::name_lock_path(root);
    let current: BTreeMap<String, Entry> = ir
        .operations
        .iter()
        .map(|o| {
            (
                o.op_id.clone(),
                Entry {
                    group: o.group.clone(),
                    command: o.command.clone(),
                    fn_name: o.fn_name.as_str().to_owned(),
                },
            )
        })
        .collect();

    if !path.exists() {
        std::fs::create_dir_all(spec::spec_dir(root))?;
        std::fs::write(&path, render(&current, &ir.spec_version))?;
        eprintln!("created {} with {} entries", path.display(), current.len());
        return Ok(());
    }

    let text = std::fs::read_to_string(&path)?;
    let locked: BTreeMap<String, Entry> =
        toml::from_str(&text).map_err(|e| format!("{} is malformed: {e}", path.display()))?;

    let mut renamed: Vec<(&String, &Entry, &Entry)> = Vec::new();
    let mut added: Vec<&String> = Vec::new();
    for (op_id, now) in &current {
        match locked.get(op_id) {
            Some(before) if before != now => renamed.push((op_id, before, now)),
            Some(_) => {}
            None => added.push(op_id),
        }
    }
    let removed: Vec<&String> = locked.keys().filter(|id| !current.contains_key(*id)).collect();

    if !renamed.is_empty() && !mode.accept_renames {
        let mut msg = format!(
            "{} command name(s) would change, which would break users' scripts:\n",
            renamed.len()
        );
        for (op_id, before, now) in &renamed {
            msg.push_str(&format!(
                "\n  {op_id}\n    - {}\n    + {}\n",
                before.invocation(),
                now.invocation()
            ));
            if before.fn_name != now.fn_name {
                msg.push_str(&format!(
                    "      client fn: {}::{} -> {}::{}\n",
                    before.group, before.fn_name, now.group, now.fn_name
                ));
            }
        }
        msg.push_str(
            "\nFor intended renames, re-run with --accept-renames to update spec/name-lock.toml.\n\
             Otherwise, preserve the old names in crates/xtask/src/overrides.toml.",
        );
        bail!("{msg}");
    }

    if !removed.is_empty() && !mode.accept_removals {
        let mut msg = format!("{} operation(s) vanished from the spec:\n", removed.len());
        for op_id in &removed {
            let e = &locked[*op_id];
            msg.push_str(&format!("  {op_id}  ({})\n", e.invocation()));
        }
        msg.push_str(
            "\nConfirm these endpoints were removed upstream, then re-run with --accept-removals.\n\
             This removes their commands from the name lock.",
        );
        bail!("{msg}");
    }

    // Print the accepted changes in changelog form, then persist.
    if !renamed.is_empty() {
        println!("### Renamed commands");
        for (op_id, before, now) in &renamed {
            println!("- `{}` is now `{}` ({op_id})", before.invocation(), now.invocation());
        }
    }
    if !removed.is_empty() {
        println!("### Removed commands");
        for op_id in &removed {
            println!("- `{}` ({op_id})", locked[*op_id].invocation());
        }
    }
    if !added.is_empty() {
        println!("### New commands");
        for op_id in &added {
            println!("- `{}` ({op_id})", current[*op_id].invocation());
        }
    }

    let rendered = render(&current, &ir.spec_version);
    if rendered != text {
        std::fs::write(&path, &rendered)?;
        eprintln!(
            "updated {}: +{} renamed {} removed {}",
            path.display(),
            added.len(),
            renamed.len(),
            removed.len()
        );
    }
    Ok(())
}

/// One line per operation, sorted by `operationId`.
///
/// Inline tables rather than `[sections]` on purpose: a rename must be a **one-line diff**, so
/// that reviewing a spec bump means reading the lock's diff instead of 42k lines of generated
/// code.
fn render(entries: &BTreeMap<String, Entry>, spec_version: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "# @generated by `cargo xtask codegen`. Committed on purpose — this file is a contract.\n\
         #\n\
         # Every entry is a command name users can put in a script. Lowering hard-errors if a\n\
         # spec bump would change one, so a rename is always a reviewed, documented decision\n\
         # rather than something that happens to people. See crates/xtask/src/name_lock.rs.\n\
         #\n",
    );
    out.push_str(&format!("# spec: gitea v{spec_version}, {} operations\n\n", entries.len()));
    for (op_id, e) in entries {
        out.push_str(&format!(
            "{} = {{ group = {:?}, command = {:?}, fn = {:?} }}\n",
            toml_key(op_id),
            e.group,
            e.command,
            e.fn_name,
        ));
    }
    out
}

/// TOML bare keys allow `A-Za-z0-9_-`; every Gitea `operationId` qualifies, but quoting
/// anything unexpected is cheaper than producing a file that will not parse.
fn toml_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        k.to_owned()
    } else {
        format!("{k:?}")
    }
}

/// Operations in the lock but with no group at all — used only by tests today, but the shape a
/// future `codegen --stats` will report from.
pub fn groups_in(entries: &BTreeMap<String, Entry>) -> BTreeSet<&str> {
    entries.values().map(|e| e.group.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(group: &str, command: &str, fn_name: &str) -> Entry {
        Entry { group: group.to_owned(), command: command.to_owned(), fn_name: fn_name.to_owned() }
    }

    #[test]
    fn rendered_lock_round_trips() {
        let mut m = BTreeMap::new();
        m.insert(
            "repoCreatePullRequest".to_owned(),
            entry("repo", "create-pull-request", "create_pull_request"),
        );
        m.insert("ListActionRuns".to_owned(), entry("run", "list", "list"));
        let text = render(&m, "1.27.2");
        let back: BTreeMap<String, Entry> = toml::from_str(&text).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn one_operation_per_line_so_a_rename_is_a_one_line_diff() {
        // If this became a `[section]` per operation, a single rename would show up as a
        // four-line diff and reviewing a spec bump would get proportionally harder.
        let mut m = BTreeMap::new();
        m.insert("a".to_owned(), entry("g", "c", "c"));
        m.insert("b".to_owned(), entry("g", "d", "d"));
        let text = render(&m, "1.27.2");
        let body: Vec<&str> =
            text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).collect();
        assert_eq!(body.len(), 2, "{text}");
        assert!(body[0].starts_with("a = { group ="), "{:?}", body[0]);
    }

    #[test]
    fn the_lock_records_the_invocation_a_user_would_type() {
        // The error message has to be recognisable to someone whose script just broke.
        assert_eq!(
            entry("repo", "create-pull-request", "x").invocation(),
            "gea raw repo create-pull-request"
        );
    }
}

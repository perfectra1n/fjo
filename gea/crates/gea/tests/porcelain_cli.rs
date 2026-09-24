//! The layer-3 command tree, checked as clap sees it.
//!
//! `crate::raw` already has a smoke test that builds the 506-operation layer-2 tree, and its
//! comment explains why: **clap answers a duplicate flag name with a `panic!`**, so a collision is
//! a crash reachable from an ordinary command line rather than a compile error. Layer 3 had no
//! equivalent test, and it has exactly the same exposure — every global flag is
//! `.global(true)` and is therefore propagated into every subcommand, so a group that declares
//! `--template`, `--limit`, or `-q` panics the moment somebody runs it.
//!
//! That is not hypothetical. Writing this test found live crashes in the group this file was
//! added with:
//!
//! ```text
//! gea admin repo create   --template collides with the global --template
//! gea admin repo list     -q         collides with the global -q (--jq)
//! ```
//!
//! `Command::debug_assert` runs clap's own consistency checks over the whole tree, which is the
//! same thing that would fire at runtime — only here it fires in CI instead of in a user's
//! terminal.

use std::collections::BTreeSet;

use clap::{CommandFactory, Parser};

/// The derive root the binary builds, rebuilt here.
///
/// `main.rs`'s own `Cli` is private to the binary target and carries `api` and `raw` alongside the
/// porcelain groups. Only the porcelain half is under test, and it is the half where a collision
/// with a global is possible, so this reconstructs it from the public `gea::cmd::Porcelain`.
#[derive(Parser)]
#[command(name = "gea")]
struct Root {
    #[command(subcommand)]
    _command: gea::cmd::Porcelain,
}

fn tree() -> clap::Command {
    gea::global::augment(Root::command(), &BTreeSet::new())
}

/// Bug this prevents: `gea admin repo create --owner x widget` panicking inside clap with
/// "Long option names must be unique for each argument" — a crash, not an error message, on a
/// perfectly reasonable command line.
#[test]
fn the_whole_porcelain_tree_survives_claps_own_consistency_checks() {
    tree().debug_assert();
}

/// Bug this prevents: a group quietly redeclaring a flag that `GlobalOpts` already owns. clap only
/// panics on a *name* collision; a group that declared `--limit` with a different id and a
/// different meaning would parse, and would silently mean something else than it does everywhere
/// else in the tool. `docs/porcelain-conventions.md` forbids redeclaring these by name.
#[test]
fn no_group_redeclares_a_global_flag() {
    // The globals a group must never take for itself, from `docs/porcelain-conventions.md`.
    const RESERVED: &[&str] = &[
        "repo", "host", "login", "json", "jq", "template", "color", "sudo", "otp", "debug",
        "paginate", "limit", "output", "force",
    ];

    let root = tree();
    let mut offenders: Vec<String> = Vec::new();
    for group in root.get_subcommands() {
        walk(group, group.get_name(), RESERVED, &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "these arguments shadow a global flag:\n{}",
        offenders.join("\n")
    );
}

fn walk(cmd: &clap::Command, path: &str, reserved: &[&str], offenders: &mut Vec<String>) {
    for arg in cmd.get_arguments() {
        // A global's own propagated copy is identified by its `g:` id prefix; anything else with
        // one of these long names is a group's own declaration.
        let id = arg.get_id().as_str();
        if id.starts_with("g:") {
            continue;
        }
        if let Some(long) = arg.get_long()
            && reserved.contains(&long)
        {
            offenders.push(format!("  gea {path}: --{long} (argument id {id})"));
        }
    }
    for sub in cmd.get_subcommands() {
        walk(sub, &format!("{path} {}", sub.get_name()), reserved, offenders);
    }
}

/// Every group and subcommand needs an `about`, because `gea <group> --help` is how the tool is
/// discovered. A subcommand with no description is a dead end in the help output.
#[test]
fn every_command_in_the_tree_describes_itself() {
    let root = tree();
    let mut missing: Vec<String> = Vec::new();
    for group in root.get_subcommands() {
        describe(group, group.get_name(), &mut missing);
    }
    assert!(missing.is_empty(), "these commands have no description:\n{}", missing.join("\n"));
}

fn describe(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
    if cmd.get_about().is_none() {
        missing.push(format!("  gea {path}"));
    }
    for sub in cmd.get_subcommands() {
        describe(sub, &format!("{path} {}", sub.get_name()), missing);
    }
}

/// The groups this wave owns, spelled out so a rename shows up here rather than as a user's
/// "unrecognized subcommand". `deploy-key` in particular is the one command whose name differs
/// from its module name (`deploy_key`), which is exactly the kind of thing that gets typo'd.
#[test]
fn the_administration_and_plumbing_groups_are_reachable_under_their_command_names() {
    let root = tree();
    for (group, verbs) in [
        ("webhook", &["list", "create", "view", "edit", "delete", "test"][..]),
        ("deploy-key", &["list", "add", "view", "delete"][..]),
        ("topic", &["list", "add", "remove", "set"][..]),
        ("reaction", &["list", "add", "remove"][..]),
        ("block", &["list", "add", "remove"][..]),
        ("transfer", &["start", "accept", "reject", "status"][..]),
        ("admin", &["user", "org", "repo", "cron", "adopt", "runner", "email"][..]),
    ] {
        let cmd = root
            .get_subcommands()
            .find(|c| c.get_name() == group)
            .unwrap_or_else(|| panic!("gea {group} is missing"));
        for verb in verbs {
            assert!(
                cmd.get_subcommands().any(|c| c.get_name() == *verb),
                "gea {group} {verb} is missing"
            );
        }
    }
}

/// `gea reaction add +1` must not be readable as a flag.
///
/// Reaction names include `+1` and `-1`, and `-1` looks exactly like a short option to a parser.
/// Without `allow_hyphen_values` on the value, the most common reaction in the world is unusable.
#[test]
fn a_reaction_named_minus_one_parses_as_a_value_not_a_flag() {
    for argv in [
        &["gea", "reaction", "add", "-1", "--issue", "42"][..],
        &["gea", "reaction", "add", "--issue", "42", "-1"][..],
        &["gea", "reaction", "remove", "-1", "--comment", "918273"][..],
        // `+1` never looked like a flag, but it is the other half of the pair and worth pinning.
        &["gea", "reaction", "add", "+1", "--issue", "42"][..],
    ] {
        let m = tree().try_get_matches_from(argv);
        assert!(m.is_ok(), "{argv:?} must parse: {:?}", m.err().map(|e| e.to_string()));
    }
}

/// The scope flags on `webhook` are mutually exclusive, and clap has to enforce it — two scopes
/// would otherwise silently pick whichever the code checked first.
#[test]
fn webhook_scopes_are_mutually_exclusive() {
    let ok = tree().try_get_matches_from(["gea", "webhook", "list", "--global"]);
    assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e.to_string()));

    for pair in [["--org", "acme"], ["--user", "--global"], ["--global", "--org"]] {
        let mut argv = vec!["gea", "webhook", "list"];
        argv.extend(pair);
        argv.push("--user");
        assert!(
            tree().try_get_matches_from(argv.clone()).is_err(),
            "{argv:?} names more than one scope and must be refused"
        );
    }
}

/// `gea reaction list` with neither target must be a *parse* error naming both flags, not a
/// request that picks one. The two endpoints operate on different objects, and `42` is a valid
/// value for both.
#[test]
fn reaction_requires_exactly_one_target() {
    assert!(tree().try_get_matches_from(["gea", "reaction", "list"]).is_err());
    assert!(
        tree()
            .try_get_matches_from(["gea", "reaction", "list", "--issue", "1", "--comment", "2"])
            .is_err()
    );
    assert!(tree().try_get_matches_from(["gea", "reaction", "list", "--issue", "1"]).is_ok());
}

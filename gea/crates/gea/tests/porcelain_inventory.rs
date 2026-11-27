//! The porcelain leaf inventory, and the contract every leaf owes its user.
//!
//! `porcelain_cli.rs` checks the *shape* of the layer-3 tree — that clap can build it without
//! panicking and that no group steals a global's name. This file checks the *leaves*, one at a
//! time, and for the same reason: every global flag is `.global(true)`, so clap propagates it into
//! each subcommand while building, and **clap answers a duplicate long name with a `panic!`, not
//! an error**. `porcelain_cli.rs`'s module docs record the three live crashes that exposure
//! produced. `debug_assert` catches a name collision; it does not catch a global that silently
//! failed to reach a leaf, or a leaf that reached the user with no description.
//!
//! It also emits the inventory a coverage ratchet consumes. Set `GEA_COVERAGE_DIR` and every leaf
//! path lands in `$GEA_COVERAGE_DIR/porcelain-inventory.json` as a sorted JSON array. Unset — the
//! ordinary `mise run test` run — nothing is written, because a test that touches the filesystem
//! by default is a test that fails on a read-only checkout and races itself under `cargo nextest`.

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

/// Every leaf command, as the space-joined path a user types after `gea`.
///
/// A leaf is a subcommand with no subcommands of its own — `pr list`, `admin user create`. Groups
/// are not leaves: `gea pr` alone prints help and does nothing, so it has no behaviour to cover.
fn leaves() -> Vec<String> {
    let root = tree();
    let mut out = Vec::new();
    for group in root.get_subcommands() {
        collect(group, group.get_name().to_owned(), &mut out);
    }
    out.sort();
    out
}

fn collect(cmd: &clap::Command, path: String, out: &mut Vec<String>) {
    // clap synthesises a `help` subcommand under every parent. It is not a gea command, has no
    // implementation to cover, and would inflate the inventory by one entry per group.
    if cmd.get_name() == "help" {
        return;
    }
    let mut children = cmd.get_subcommands().filter(|c| c.get_name() != "help").peekable();
    if children.peek().is_none() {
        out.push(path);
        return;
    }
    for sub in children {
        collect(sub, format!("{path} {}", sub.get_name()), out);
    }
}

/// Renders `--help` for a leaf through the real root.
///
/// Building the leaf's own `Command` in isolation would show a tree the user never sees: clap
/// attaches globals during the *parent's* build, so a leaf lifted out of the tree has none of
/// them and every global assertion below would pass vacuously. `cli.rs` takes the same route for
/// the layer-2 operations.
fn render_leaf_help(leaf: &str) -> String {
    let mut argv = vec!["gea"];
    argv.extend(leaf.split(' '));
    argv.push("--help");

    let err = tree()
        .try_get_matches_from(&argv)
        .expect_err("clap reports --help as an error it wants printed");
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "gea {leaf} --help");
    err.render().to_string()
}

/// Bug this prevents: `gea admin repo create --help` panicking inside clap with "Long option names
/// must be unique for each argument" instead of printing usage — a crash on a command line whose
/// entire purpose is to ask what the command does. `debug_assert` catches the collisions clap
/// models as consistency errors; actually driving `--help` through every leaf is what proves the
/// user-facing path survives.
#[test]
fn every_leaf_command_renders_its_own_help_without_panicking() {
    for leaf in leaves() {
        let help = render_leaf_help(&leaf);
        assert!(!help.is_empty(), "gea {leaf} --help rendered nothing");
    }
}

/// Bug this prevents: a leaf losing repository, host, or output context because a group in its
/// ancestry took a global's name for itself. A global is only useful if it survives all the way
/// down — `gea pr list --json` that does not know `--json` is a global that exists on paper only.
#[test]
fn the_global_flags_reach_every_leaf_command() {
    for leaf in leaves() {
        let help = render_leaf_help(&leaf);
        assert!(help.contains("--host"), "gea {leaf} lost --host:\n{help}");
        // `--json` names a generated flag on no porcelain command, so it must survive everywhere.
        assert!(help.contains("--json"), "gea {leaf} lost --json:\n{help}");
        // `-R` is the one global that must survive even where a command owns `--repo` as its own
        // positional or parameter, or repository context becomes unreachable. It renders as
        // `-R, --repo <…>` normally and as `-R <…>` where the long name was surrendered.
        assert!(
            help.contains("-R, --repo") || help.contains("-R <"),
            "gea {leaf} lost -R:\n{help}"
        );
    }
}

/// Bug this prevents: shipping a command that `gea <group> --help` lists as a bare name with no
/// description. `porcelain_cli.rs` asserts the same over the whole tree; this is the leaf-scoped
/// half, and it fails with the leaf paths a user would type rather than a node somewhere inside
/// the tree.
#[test]
fn every_leaf_command_documents_itself() {
    let root = tree();
    let mut undocumented: Vec<String> = Vec::new();
    for group in root.get_subcommands() {
        check_about(group, group.get_name().to_owned(), &mut undocumented);
    }
    assert!(
        undocumented.is_empty(),
        "these leaf commands have no `about`:\n{}",
        undocumented.join("\n")
    );
}

fn check_about(cmd: &clap::Command, path: String, undocumented: &mut Vec<String>) {
    if cmd.get_name() == "help" {
        return;
    }
    let mut children = cmd.get_subcommands().filter(|c| c.get_name() != "help").peekable();
    if children.peek().is_none() {
        if cmd.get_about().is_none_or(|a| a.to_string().trim().is_empty()) {
            undocumented.push(format!("  gea {path}"));
        }
        return;
    }
    for sub in children {
        check_about(sub, format!("{path} {}", sub.get_name()), undocumented);
    }
}

/// A floor, deliberately not an equality.
///
/// Pinning the exact count turns every new command into a failing test whose author fixes it by
/// editing a number, which teaches people to edit the number instead of reading the assertion. A
/// floor still catches the failure that matters: a refactor that drops a whole group out of
/// `Porcelain` and takes its commands off the command line with it. The inventory emitted below
/// is the exhaustive half; this is only the tripwire.
#[test]
fn the_porcelain_tree_still_exposes_every_command_group_it_used_to() {
    // 231 leaves across 36 top-level groups at the time of writing.
    let leaves = leaves();
    assert!(
        leaves.len() >= 231,
        "expected at least 231 leaf commands, found {} — a group has fallen out of `Porcelain`",
        leaves.len()
    );

    let groups: BTreeSet<&str> = leaves.iter().filter_map(|leaf| leaf.split(' ').next()).collect();
    assert!(
        groups.len() >= 36,
        "expected at least 36 command groups, found {}: {groups:?}",
        groups.len()
    );
}

/// Emits the inventory the coverage ratchet diffs against, and only when asked to.
///
/// The path and the shape are a fixed contract with a component outside this crate, so neither is
/// derived from anything that could drift. With `GEA_COVERAGE_DIR` unset this writes nothing at
/// all: an unconditional write would make `cargo test` fail on a read-only checkout and would put
/// two test binaries in a race for the same file.
#[test]
fn the_leaf_inventory_is_emitted_for_the_coverage_ratchet() {
    let leaves = leaves();

    let Some(dir) = std::env::var_os("GEA_COVERAGE_DIR") else {
        return;
    };

    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));

    let path = dir.join("porcelain-inventory.json");
    let json = serde_json::to_string_pretty(&leaves).expect("a Vec<String> always serialises");
    std::fs::write(&path, json).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
}

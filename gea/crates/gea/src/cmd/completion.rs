//! `gea completion` — a shell completion script.
//!
//! # What is and is not covered, and why
//!
//! Layer 2 is 506 operations behind a two-phase parse: `main` never builds that tree unless the
//! first non-flag word is `raw`, because constructing thousands of `Arg`s costs five to twenty
//! milliseconds on a twenty-five millisecond startup budget. `clap_complete` generates from a
//! **fully built** `Command`, so a completion script covering every layer-2 leaf would require
//! building exactly the tree the two-phase parse exists to avoid — and the resulting bash script
//! would be well over a megabyte.
//!
//! So the generated script covers layer 1 (`gea api`), all of layer 3, and the `raw` **group**
//! names, stopping there. `--help` says so, out loud: a completion that silently omits half the
//! tool teaches users the omitted half does not exist. `gea raw search <words>` is the discovery
//! tool for the rest, and it is named in the help text for that reason.

use std::io::Write;

use clap::builder::PossibleValue;
use clap::{Args as ClapArgs, Command, Subcommand, ValueEnum};
use gitea_core::Result;

use crate::cmd::support;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    /// Shell to generate a script for
    #[arg(value_name = "SHELL", value_enum)]
    pub shell: Shell,
}

const LONG_HELP: &str = "\
Generate a shell completion script.

Completions include common commands, flags, and `gea raw` group names.
Individual raw operations are not included. Find them with `gea raw search <words>`
or `gea raw <group> --help`.

  gea completion bash > /etc/bash_completion.d/gea
  gea completion zsh  > \"${fpath[1]}/_gea\"
  gea completion fish > ~/.config/fish/completions/gea.fish";

/// The shells `clap_complete` ships generators for.
///
/// Spelled out locally rather than re-exporting `clap_complete::Shell` so that the value list in
/// `--help` is ours and cannot silently grow or shrink with a dependency bump — a completion for a
/// shell we have never tested is not a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl ValueEnum for Shell {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Bash, Self::Zsh, Self::Fish, Self::PowerShell, Self::Elvish]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::PowerShell => "powershell",
            Self::Elvish => "elvish",
        }))
    }
}

impl From<Shell> for clap_complete::Shell {
    fn from(s: Shell) -> Self {
        match s {
            Shell::Bash => Self::Bash,
            Shell::Zsh => Self::Zsh,
            Shell::Fish => Self::Fish,
            Shell::PowerShell => Self::PowerShell,
            Shell::Elvish => Self::Elvish,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let mut cmd = root();
    let mut out = support::writer(globals)?;
    clap_complete::generate(clap_complete::Shell::from(args.shell), &mut cmd, "gea", &mut out);
    out.flush()?;
    Ok(())
}

/// The command tree completions are generated from, and the same tree `alias set` checks a new
/// alias against.
///
/// It is rebuilt here rather than borrowed from `main.rs`: the derive root lives in the *binary*
/// crate, which the library cannot see, and moving it into the library would mean `main` no longer
/// owns its own `--help`. The three parts below are exactly what `main::dispatch` can reach —
/// `api`, the flattened layer-3 groups, and `raw` — so a group added to
/// [`crate::cmd::Porcelain`] appears here with no further edit.
pub fn root() -> Command {
    let cmd = Command::new("gea")
        .about(crate::ABOUT)
        .version(crate::version_string())
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .subcommand(crate::api::ApiArgs::augment_args(
            Command::new("api").about("Call any Gitea REST endpoint by path"),
        ))
        .subcommand(raw_group_stubs());
    let cmd = <super::Porcelain as Subcommand>::augment_subcommands(cmd);
    crate::global::augment(cmd, &std::collections::BTreeSet::new())
}

/// `gea raw <group>` with no leaves under it.
///
/// The group names come from the generated table, so they cannot drift from what `gea raw` will
/// actually accept. Leaves are deliberately absent — see the module comment.
fn raw_group_stubs() -> Command {
    let mut raw = Command::new("raw")
        .visible_alias("x")
        .about("Call any Gitea API operation directly")
        .subcommand(
            Command::new("search")
                .about("Find an operation by keyword")
                .arg(clap::Arg::new("terms").num_args(1..)),
        );
    for group in gitea_client::meta::GROUPS {
        raw = raw.subcommand(Command::new(group.name).about(group.about));
    }
    raw
}

/// Every top-level word `gea` itself answers to, for `alias set`'s shadow check.
pub fn command_names() -> Vec<String> {
    let root = root();
    let mut out = Vec::new();
    for sc in root.get_subcommands() {
        out.push(sc.get_name().to_owned());
        out.extend(sc.get_all_aliases().map(str::to_owned));
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: `root()` drifting from `main.rs`'s derive tree, so a completion script
    /// silently omits a whole group. The layer-3 list comes from `Porcelain` itself, which is the
    /// same enum `main` flattens, so the check is that the three top-level pieces are all present.
    #[test]
    fn the_completion_root_carries_all_three_layers() {
        let names = command_names();
        assert!(names.contains(&"api".to_owned()), "{names:?}");
        assert!(names.contains(&"raw".to_owned()), "{names:?}");
        assert!(names.contains(&"x".to_owned()), "the hidden raw alias: {names:?}");
        for group in ["auth", "config", "alias", "completion", "status", "pr", "issue", "repo"] {
            assert!(names.contains(&group.to_owned()), "{group} missing from {names:?}");
        }
    }

    /// Bug this prevents: building the layer-2 leaves here after all, which is the cost the
    /// two-phase parse exists to avoid and would produce a multi-megabyte script.
    #[test]
    fn raw_carries_group_names_but_no_leaves() {
        let root = root();
        let raw = root.get_subcommands().find(|c| c.get_name() == "raw").expect("raw");
        let groups: Vec<&str> = raw.get_subcommands().map(Command::get_name).collect();
        assert!(groups.contains(&"repo"), "{groups:?}");
        assert!(groups.contains(&"search"), "{groups:?}");
        let repo = raw.get_subcommands().find(|c| c.get_name() == "repo").expect("raw repo");
        assert_eq!(repo.get_subcommands().count(), 0, "layer-2 leaves must not be built");
    }

    /// Bug this prevents: a script that will not load. Generating into a buffer at least proves
    /// the generator ran over the whole tree without panicking on a duplicate name, which is how
    /// a clap tree usually breaks.
    #[test]
    fn every_shell_generates_something_that_mentions_the_groups() {
        for shell in <Shell as ValueEnum>::value_variants() {
            let mut buf = Vec::new();
            let mut cmd = root();
            clap_complete::generate(clap_complete::Shell::from(*shell), &mut cmd, "gea", &mut buf);
            let text = String::from_utf8(buf).expect("generated scripts are UTF-8");
            assert!(text.contains("auth"), "{shell:?} script has no auth");
            assert!(!text.is_empty());
        }
    }

    /// The help text has to *say* that layer 2's leaves are absent. A completion that quietly
    /// omits half the tool teaches users the omitted half does not exist.
    #[test]
    fn the_help_admits_what_it_does_not_cover() {
        assert!(LONG_HELP.contains("Individual raw operations are not included"), "{LONG_HELP}");
        assert!(LONG_HELP.contains("gea raw search"), "{LONG_HELP}");
    }
}

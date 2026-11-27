//! `gea pr files` — what a pull request touches, and by how much.
//!
//! `pr diff --name-only` gives the names; this gives the names *with* their status and their line
//! counts, which is the table you want when deciding whether a change is a one-line fix or a
//! rewrite. Filenames are sanitised the same way a diff's contents are — see
//! [`crate::cmd::pr::diff`] for why that is a security property and not a formatting one.

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_core::Result;
use gitea_model::ChangedFile;

use super::common;
use super::diff::sanitize;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::autocolor;
use crate::output::table::Table;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
List files changed by a pull request.

Filenames are escaped for safe terminal display.

  gea pr files
  gea pr files 42
  gea pr files 42 --json filename,additions,deletions
  gea pr files | cut -f1")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Maximum number of files. The long form is the global `--limit`
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Print escape sequences in filenames verbatim
    #[arg(long)]
    pub allow_escape_sequences: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_CHANGED_FILE)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;
        // A file list is not a 30-row resource: a large pull request touching 400 files should show
        // all of them, so the default cap is the pull request's own `changed_files` when we know it.
        let limit = args
            .limit
            .or(globals.limit)
            .unwrap_or_else(|| usize::try_from(found.pr.changed_files).unwrap_or(0).max(30));

        let query = gitea_client::query::RepoGetPullRequestFilesQuery::default();
        let mut stream = api
            .repo()
            .get_pull_request_files(&found.slug.owner, &found.slug.name, found.index(), &query)
            .take(limit);
        let mut files = Vec::new();
        while let Some(item) = stream.next().await {
            files.push(item?);
        }

        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&files)?)
            }
            _ => {
                if files.is_empty() {
                    support::empty_note(rt.term(), "changed files");
                }
                print!("{}", table(&files, rt.term(), args.allow_escape_sequences));
                Ok(())
            }
        }
    })
}

pub(crate) fn table(files: &[ChangedFile], term: &Term, allow_escapes: bool) -> String {
    let mut t = Table::new(term);
    t.headers(["FILE", "STATUS", "+", "-"]);
    for file in files {
        t.row([
            sanitize(&display_path(file), allow_escapes),
            autocolor(term, &file.status),
            format!("+{}", file.additions),
            format!("-{}", file.deletions),
        ]);
    }
    t.render_to_string()
}

/// `old => new` for a rename, so the move is visible instead of looking like an unrelated addition.
fn display_path(file: &ChangedFile) -> String {
    if file.previous_filename.is_empty() || file.previous_filename == file.filename {
        file.filename.clone()
    } else {
        format!("{} => {}", file.previous_filename, file.filename)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, status: &str, add: i64, del: i64) -> ChangedFile {
        ChangedFile {
            filename: name.to_owned(),
            status: status.to_owned(),
            additions: add,
            deletions: del,
            changes: add + del,
            ..ChangedFile::default()
        }
    }

    /// Bug this prevents: a rename shown as one deletion and one unrelated addition, which is how the
    /// API reports it and not how a reader wants to see it.
    #[test]
    fn a_rename_shows_both_names() {
        let mut renamed = file("src/new.rs", "renamed", 0, 0);
        renamed.previous_filename = "src/old.rs".to_owned();
        assert_eq!(display_path(&renamed), "src/old.rs => src/new.rs");
        assert_eq!(display_path(&file("src/a.rs", "modified", 1, 1)), "src/a.rs");
    }

    /// A filename is attacker-controlled text too — a contributor chooses it — and this is the one
    /// path that would print it straight to a terminal.
    #[test]
    fn filenames_are_sanitised_like_diff_contents() {
        let hostile = file("src/\x1b]0;owned\x07a.rs", "added", 1, 0);
        let out = table(std::slice::from_ref(&hostile), &Term::tty(80), false);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(out.contains("^["), "{out:?}");
    }

    #[test]
    fn file_table_snapshots_for_a_terminal_and_a_pipe() {
        let files = vec![
            file("src/lib.rs", "modified", 24, 3),
            file("README.md", "added", 10, 0),
            file("old.txt", "deleted", 0, 42),
        ];
        let mut report = String::from("== tty\n");
        report.push_str(&table(&files, &Term::tty(80), false));
        report.push_str("== piped\n");
        report.push_str(&table(&files, &Term::piped(), false));
        insta::assert_snapshot!(report);
    }
}

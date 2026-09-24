//! `gea pr diff` — the diff, with escape sequences defanged.
//!
//! # Why the sanitising is on by default
//!
//! A diff is *attacker-controlled text*. Anyone who can open a pull request chooses its contents,
//! and a terminal interprets what is written to it: an ESC sequence in a hunk can change the
//! window title, switch the character set, hide subsequent output, redefine what the cursor is
//! doing, or in some terminals stuff characters into the input queue. `gh` neutralises them for
//! this reason and it is a security convention rather than a formatting preference, so `gea`
//! matches it: control characters are rendered as caret notation (`^[`), and only
//! `--allow-escape-sequences` turns that off.
//!
//! Tabs, newlines and carriage returns are left alone — a diff of a file containing tabs must still
//! be a diff of a file containing tabs, and those three cannot reprogram a terminal.
//!
//! Note that `--color` is the **global** flag: it already means "when to colourise", it is
//! propagated into every command, and declaring a second `--color` here would be a duplicate long
//! name, which clap answers with a panic. So `gea pr diff --color never` works, and there is no
//! per-command flag to keep in step with it.

use std::io::Write;

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_core::Result;

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::output::color::{paint, style_by_name};
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a pull request diff.

Terminal escape sequences are displayed as caret notation.
--allow-escape-sequences disables this protection; use it only for trusted content.
Color is controlled by the global --color option.

  gea pr diff
  gea pr diff 42 --name-only
  gea pr diff --patch > change.patch
  gea pr diff --color always | less -R")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// List the changed file names instead of the diff
    #[arg(long, conflicts_with = "patch")]
    pub name_only: bool,

    /// Produce a `git am`-applicable patch rather than a diff
    #[arg(long)]
    pub patch: bool,

    /// Include binary file changes, so the diff applies with `git apply`
    #[arg(long)]
    pub binary: bool,

    /// Print escape sequences in the diff verbatim. See this command's help for why you would not
    #[arg(long)]
    pub allow_escape_sequences: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        if args.name_only {
            let query = gitea_client::query::RepoGetPullRequestFilesQuery::default();
            let mut stream = api.repo().get_pull_request_files(
                &found.slug.owner,
                &found.slug.name,
                found.index(),
                &query,
            );
            let mut out = std::io::stdout().lock();
            while let Some(item) = stream.next().await {
                let file = item?;
                // Sanitised as well: a *filename* can carry an escape sequence just as a hunk can,
                // and this is the path that would otherwise print it raw.
                writeln!(out, "{}", sanitize(&file.filename, args.allow_escape_sequences))?;
            }
            out.flush()?;
            return Ok(());
        }

        let mut query = gitea_client::query::RepoDownloadPullDiffOrPatchQuery::default();
        if args.binary {
            query = query.with_binary(true);
        }
        let kind = if args.patch { "patch" } else { "diff" };
        let text = api
            .repo()
            .download_pull_diff_or_patch(
                &found.slug.owner,
                &found.slug.name,
                found.index(),
                kind,
                &query,
            )
            .await?;

        let mut out = std::io::stdout().lock();
        out.write_all(render(&text, rt.term(), args.allow_escape_sequences).as_bytes())?;
        out.flush()?;
        Ok(())
    })
}

/// Sanitise, then colourise if colour is on.
///
/// In that order, deliberately: colourising first and sanitising afterwards would mangle the SGR
/// codes we had just added.
pub(crate) fn render(diff: &str, term: &Term, allow_escapes: bool) -> String {
    let safe = sanitize(diff, allow_escapes);
    if !term.color {
        return safe;
    }
    let add = style_by_name("green").unwrap_or_default();
    let remove = style_by_name("red").unwrap_or_default();
    let meta = style_by_name("cyan").unwrap_or_default();
    let mut out = String::with_capacity(safe.len() + safe.len() / 8);
    for line in safe.split_inclusive('\n') {
        // `+++`/`---` before `+`/`-`: a file header starts with the same character as an added
        // or removed line, and testing it second would colour every header as a hunk line.
        let meta_line = line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("@@")
            || line.starts_with("diff ")
            || line.starts_with("index ");
        let style = if meta_line {
            Some(meta)
        } else if line.starts_with('+') {
            Some(add)
        } else if line.starts_with('-') {
            Some(remove)
        } else {
            None
        };
        match style {
            // Painting the line *without* its newline, so the reset lands before the break rather
            // than after it — a coloured newline leaves the background set for the next line in
            // some terminals.
            Some(s) => {
                let (text, tail) = line.split_at(line.trim_end_matches('\n').len());
                out.push_str(&paint(term, s, text));
                out.push_str(tail);
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Replace control characters with caret notation.
///
/// `\t`, `\n` and `\r` survive: a diff of a file containing tabs has to remain one, and none of the
/// three can reprogram a terminal. Everything else in C0, plus DEL and the C1 range, becomes `^X` —
/// visible, harmless, and reversible by eye.
pub(crate) fn sanitize(text: &str, allow: bool) -> String {
    if allow || !text.chars().any(is_dangerous) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if !is_dangerous(c) {
            out.push(c);
            continue;
        }
        match u32::from(c) {
            // C0 and DEL have the classic caret spelling: ESC is `^[`, DEL is `^?`.
            n @ 0..=0x1f => {
                out.push('^');
                out.push(char::from(b'@' + u8::try_from(n).unwrap_or(0)));
            }
            0x7f => out.push_str("^?"),
            // C1 has no caret spelling; `\u{…}` is unambiguous and still obviously not text.
            n => out.push_str(&format!("\\u{{{n:x}}}")),
        }
    }
    out
}

fn is_dangerous(c: char) -> bool {
    match c {
        '\t' | '\n' | '\r' => false,
        c => c.is_control(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The security test.** A pull request's diff is text an untrusted contributor chose, and a
    /// terminal executes escape sequences written to it — window title, character set, hidden
    /// output. `gh` neutralises them; so must we, by default and without being asked.
    #[test]
    fn escape_sequences_are_neutralised_by_default() {
        let hostile = "+\x1b]0;you have been owned\x07\n+\x1b[2Jcleared\n";
        let safe = sanitize(hostile, false);
        assert!(!safe.contains('\x1b'), "{safe:?}");
        assert!(!safe.contains('\x07'), "{safe:?}");
        assert!(safe.contains("^["), "the sequence is still legible: {safe:?}");
        // The text itself is untouched, so the reader can still see what was attempted.
        assert!(safe.contains("you have been owned"), "{safe:?}");
    }

    /// The opt-out has to actually opt out, or somebody piping to a tool that wants the raw bytes
    /// has no way through.
    #[test]
    fn allow_escape_sequences_passes_them_through() {
        let hostile = "+\x1b[31mred\x1b[0m\n";
        assert_eq!(sanitize(hostile, true), hostile);
    }

    /// Bug this prevents: sanitising tabs, which turns every indented line of a diff into `^I` and
    /// makes the output useless for the ordinary case in order to defend against the rare one.
    #[test]
    fn tabs_newlines_and_carriage_returns_survive() {
        let ordinary = "+\tindented\r\n-\tremoved\n";
        assert_eq!(sanitize(ordinary, false), ordinary);
    }

    #[test]
    fn del_and_c1_have_a_spelling_too() {
        assert_eq!(sanitize("a\x7fb", false), "a^?b");
        assert_eq!(sanitize("a\u{9b}b", false), "a\\u{9b}b");
    }

    /// Bug this prevents: colourising before sanitising, which would strip the SGR codes we just
    /// added and produce a diff spelled `^[[32m+ added`.
    #[test]
    fn colour_is_applied_after_sanitising_not_before() {
        let term = Term { tty: true, width: 80, color: true, hyperlinks: false };
        let out = render("+added\n-removed\n", &term, false);
        assert!(out.contains("\x1b["), "colour was requested: {out:?}");
        assert!(!out.contains("^["), "our own escapes must not be caret-escaped: {out:?}");

        // ...and with colour off there are no escapes at all.
        let plain = render("+added\n", &Term::tty(80), false);
        assert_eq!(plain, "+added\n");
    }

    /// `--name-only` and `--patch` are two different outputs; accepting both would silently pick one.
    #[test]
    fn name_only_and_patch_are_mutually_exclusive() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        assert!(
            <Harness as clap::Parser>::try_parse_from(["gea", "--name-only", "--patch"]).is_err()
        );
    }
}

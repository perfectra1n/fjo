//! Where a title and a body come from: `-b`, `-F`, `-e`.
//!
//! # The first line is the title
//!
//! `docs/porcelain-conventions.md` fixes it and `gh` does the same. Getting it wrong is
//! **silent**: the whole message becomes the title, the issue or pull request is created, and
//! nobody notices until someone reads a list of 400-character titles. Hence its own function,
//! its own type, and its own tests.
//!
//! Two implementations of this existed. They disagreed in two places, and each one had a
//! property the other lacked:
//!
//! | | tuple version (`issue`) | struct version (`repo`) |
//! | --- | --- | --- |
//! | a UTF-8 BOM | kept, so it became part of the title | stripped |
//! | CRLF inside the body | normalised to LF | kept, so every body line ended `\r` |
//!
//! Both are things real editors produce on Windows — Notepad writes a BOM, and almost
//! everything there writes CRLF — so the version kept here does both, and returns the struct,
//! because `None` and `Some("")` are worth being able to tell apart.

use std::io::Read;
use std::path::Path;

use gitea_core::config::SystemEnv;
use gitea_core::error::Result;

use crate::runtime::Runtime;

use super::usage;

/// A title and a body, however they were supplied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleBody {
    pub title: Option<String>,
    pub body: Option<String>,
}

impl TitleBody {
    /// The pair as plain strings, with an absent half becoming `""`.
    ///
    /// For the call sites that go straight into a request body where the API treats an empty
    /// string and an absent field alike.
    pub fn parts(self) -> (String, String) {
        (self.title.unwrap_or_default(), self.body.unwrap_or_default())
    }
}

/// Split what an editor session produced: **first line is the title, the rest is the body.**
///
/// The blank line conventionally following a title is swallowed, so `Title\n\nBody` does not
/// produce a body that starts with a newline. An abandoned session — saved empty, or all
/// whitespace — yields [`TitleBody::default`] rather than an object titled `""`.
pub fn split_editor_text(text: &str) -> TitleBody {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    let title = lines.next().unwrap_or_default().trim();
    // `lines()` rather than `split_once('\n')`: it strips the `\r` of a CRLF file, so a body
    // written on Windows does not reach the server with a carriage return on every line.
    let rest = lines.collect::<Vec<_>>().join("\n");
    let body = rest.trim_matches('\n').trim_end();
    TitleBody {
        title: (!title.is_empty()).then(|| title.to_owned()),
        body: (!body.is_empty()).then(|| body.to_owned()),
    }
}

/// The template an editor session starts from.
pub fn editor_seed(title: &str, body: &str) -> String {
    format!("{title}\n\n{body}")
}

/// Read a value that may be in a file or on stdin. `-` means stdin.
///
/// The reader is a parameter so a test does not need a real stdin.
pub fn read_source(path: &Path, stdin: &mut dyn Read) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        stdin.read_to_string(&mut buf).map_err(|e| usage(format!("could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read_to_string(path)
        .map_err(|e| usage(format!("could not read {}: {e}", path.display())))
}

/// [`read_source`], with the flag that named the path in the message.
///
/// `-` support is not a nicety: `pr create -F - <<'EOF'` and `… | gea issue comment -F -` are
/// how bodies are supplied from a script, and a `-` read as a filename fails with
/// "No such file or directory: -".
pub fn read_flagged(flag: &str, path: &str, stdin: &mut dyn Read) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        stdin
            .read_to_string(&mut buf)
            .map_err(|e| usage(format!("{flag} -: could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).map_err(|e| usage(format!("{flag} {path}: {e}")))
}

/// `-b/--body`, `-F/--body-file`, `-e/--editor` — the three ways to supply prose.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct BodyOpts {
    /// Body text
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the body from a file; `-` reads stdin
    #[arg(short = 'F', long = "body-file", value_name = "FILE", conflicts_with = "body")]
    pub body_file: Option<String>,

    /// Open $EDITOR; the first line becomes the title and the rest the body
    #[arg(short = 'e', long, conflicts_with_all = ["body", "body_file"])]
    pub editor: bool,
}

impl BodyOpts {
    /// Resolve to a title/body pair. `template` seeds an editor session.
    ///
    /// Only `--editor` can produce a title, and it always may: that is the documented contract of
    /// the flag, and it is why this returns a pair rather than just a body.
    pub fn resolve(&self, rt: &Runtime, template: &str) -> Result<TitleBody> {
        if let Some(b) = &self.body {
            return Ok(TitleBody { title: None, body: Some(b.clone()) });
        }
        if let Some(path) = &self.body_file {
            let body = read_flagged("--body-file", path, &mut std::io::stdin())?;
            return Ok(TitleBody { title: None, body: Some(body) });
        }
        if self.editor {
            let text = edit(rt, template)?;
            return Ok(split_editor_text(&text));
        }
        Ok(TitleBody::default())
    }
}

/// Open the configured editor on `seed` and return what came back.
///
/// The editor is `gea config get editor` → `$VISUAL` → `$EDITOR` → `git config core.editor` →
/// `vi`. `core.editor` is consulted here rather than in `gitea_core::config` because it is the
/// one link in the chain that needs a `GitCtx`, and `Config` deliberately does not depend on git.
pub fn edit(rt: &Runtime, seed: &str) -> Result<String> {
    if !super::can_prompt(rt) {
        return Err(usage("--editor needs a terminal; pass --body or --body-file instead"));
    }
    let host = rt.host().as_str().to_owned();
    let editor = match rt.config().editor(Some(&host)) {
        Some(e) => e,
        None => match rt.git().config_get("core.editor")? {
            Some(e) if !e.trim().is_empty() => e,
            _ => rt.config().resolved_editor(Some(&host), &SystemEnv),
        },
    };
    // Bound to a local: `with_editor_command` borrows for the builder's lifetime.
    let command = std::ffi::OsString::from(editor);
    super::interact::prompted(
        "the body",
        inquire::Editor::new("Body")
            .with_editor_command(&command)
            .with_predefined_text(seed)
            // `.md` so the editor turns on markdown highlighting; a body is markdown everywhere
            // in Gitea, and `Fixes #123` linking is a markdown-adjacent habit.
            .with_file_extension(".md")
            .prompt(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: treating the whole editor buffer as the title, or as the body. The
    /// convention is fixed — first line is the title — and it is silent when broken.
    #[test]
    fn the_editors_first_line_is_the_title() {
        let tb = split_editor_text("Add the thing\n\nBecause the thing\nwas missing.\n");
        assert_eq!(tb.title.as_deref(), Some("Add the thing"));
        assert_eq!(tb.body.as_deref(), Some("Because the thing\nwas missing."));

        let tb = split_editor_text("Fix the thing\n\nIt is broken.\n\nBadly.\n");
        assert_eq!(tb.clone().parts().0, "Fix the thing");
        assert_eq!(tb.parts().1, "It is broken.\n\nBadly.");
    }

    #[test]
    fn a_one_line_editor_buffer_is_a_title_with_no_body() {
        let tb = split_editor_text("Just a title");
        assert_eq!(tb.title.as_deref(), Some("Just a title"));
        assert_eq!(tb.body, None);
        assert_eq!(split_editor_text("Just a title\n").parts(), ("Just a title".into(), "".into()));
    }

    /// An abandoned editor session (saved empty) must not produce an issue titled "".
    #[test]
    fn an_empty_editor_buffer_yields_nothing() {
        assert_eq!(split_editor_text("   \n\n  \n"), TitleBody::default());
        assert_eq!(split_editor_text(""), TitleBody::default());
    }

    /// A UTF-8 BOM is what several Windows editors write. Left in place it becomes part of the
    /// title, invisibly. One of the two implementations this replaced did exactly that.
    #[test]
    fn a_byte_order_mark_is_not_part_of_the_title() {
        let tb = split_editor_text("\u{feff}Title\n\nbody");
        assert_eq!(tb.title.as_deref(), Some("Title"));
    }

    /// The other half of the same story: a CRLF file must not leave a carriage return on the
    /// end of every body line, which is what the struct-returning implementation did.
    #[test]
    fn a_crlf_buffer_does_not_carry_carriage_returns_into_the_body() {
        let tb = split_editor_text("Title\r\n\r\nfirst\r\nsecond\r\n");
        assert_eq!(tb.title.as_deref(), Some("Title"));
        assert_eq!(tb.body.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn a_dash_reads_stdin_and_a_flag_names_itself_in_the_error() {
        let mut stdin: &[u8] = b"from a pipe";
        assert_eq!(read_flagged("-F/--body-file", "-", &mut stdin).unwrap(), "from a pipe");
        let e = read_flagged("-F/--body-file", "/nope/nothing", &mut std::io::empty()).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("-F/--body-file"), "{e}");

        let mut stdin: &[u8] = b"key material";
        assert_eq!(read_source(Path::new("-"), &mut stdin).unwrap(), "key material");
    }
}

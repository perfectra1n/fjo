//! The output system.
//!
//! ```text
//! Value ─> project (--json: keep requested top-level keys) ─> jq (jaq) ─> render
//!                                                                         ├─ --template
//!                                                                         ├─ JSON (pretty on TTY, compact piped)
//!                                                                         └─ Table / TSV
//! ```
//!
//! **Order matters, and it matches `gh`: projection happens before `--jq`.** So
//! `--json number,title --jq '.[].title'` filters first and then queries the *filtered*
//! document. Reversing them would make `--json` look like a no-op whenever `--jq` reshaped the
//! value, and a user's two flags would interact differently here than in `gh`.
//!
//! Module map:
//!
//! | module | responsibility |
//! | --- | --- |
//! | [`project`] | `--json` selection, projection, and field discovery |
//! | [`jq`] | the only place a `jaq` type is named |
//! | [`template`] | the hand-written Go-template subset |
//! | [`table`] | the one `Table`, shared by the human view and `tablerender` |
//! | [`tty`] | TTY / width / color / hyperlink detection |
//! | [`color`] | styling, and the ANSI-aware width primitives |
//! | [`pager`] | `GEA_PAGER` → `PAGER` → config |
//!
//! The **Table / TSV** branch is driven by the command layer rather than by [`Pipeline`]:
//! only the command knows which columns its resource has and what its `Showing N of M` banner
//! should say. It reaches the same [`table::Table`] type, so there is still one width
//! algorithm.

pub mod color;
pub mod jq;
pub mod pager;
pub mod project;
pub mod table;
pub mod template;
pub mod tty;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use gitea_core::{Error, ErrorKind};
use serde_json::Value;

pub use color::{display_width, strip_ansi, truncate_visible};
pub use jq::Filter;
pub use project::{FieldKind, FieldSpec, Selection};
pub use table::Table;
pub use template::Template;
pub use tty::{Env, MapEnv, SysEnv, Term};

/// What a command produced.
///
/// Mirrors the runtime's response shapes: the spec declares `text/plain`, `text/html`,
/// `application/zip`, `application/octet-stream`, `application/gzip`, and `application/ld+json`
/// alongside JSON, plus 204-no-content endpoints.
pub enum Payload {
    Json(Value),
    /// `text/plain` and `text/html` responses (raw file contents, a rendered markdown blob).
    /// Written through verbatim: adding a trailing newline would corrupt a file fetched with
    /// `gea api repos/o/r/raw/README`.
    Text(String),
    /// A streamed non-JSON body: release assets, action artifacts, archives.
    ///
    /// A `Read` rather than an async stream so that rendering stays synchronous and testable.
    /// The command layer bridges the runtime's async `ByteStream` (with
    /// `tokio_util::io::SyncIoBridge`, or by buffering when the body is known to be small).
    Bytes {
        mime: String,
        body: Box<dyn Read + Send>,
    },
    /// A 204, or a command whose only output is its exit code.
    Empty,
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(v) => f.debug_tuple("Json").field(v).finish(),
            Self::Text(t) => f.debug_tuple("Text").field(t).finish(),
            Self::Bytes { mime, .. } => {
                f.debug_struct("Bytes").field("mime", mime).finish_non_exhaustive()
            }
            Self::Empty => f.write_str("Empty"),
        }
    }
}

/// Where a payload goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    Stdout,
    /// `--output <file>`.
    File(PathBuf),
}

/// The `--json` / `--jq` / `--template` transform chain.
///
/// Borrows its filter and template so one `Pipeline` can be reused across every page of a
/// `--paginate` run without recompiling either.
#[derive(Debug, Default, Clone, Copy)]
pub struct Pipeline<'a> {
    pub fields: Option<&'a [String]>,
    pub jq: Option<&'a Filter>,
    pub template: Option<&'a Template>,
}

impl<'a> Pipeline<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn fields(mut self, fields: Option<&'a [String]>) -> Self {
        self.fields = fields;
        self
    }

    #[must_use]
    pub fn jq(mut self, filter: Option<&'a Filter>) -> Self {
        self.jq = filter;
        self
    }

    #[must_use]
    pub fn template(mut self, template: Option<&'a Template>) -> Self {
        self.template = template;
        self
    }

    /// True when the pipeline asks for machine-readable output, which is how the command layer
    /// decides to skip its human table (and its `Showing N of M` banner).
    pub fn is_explicit(&self) -> bool {
        self.fields.is_some() || self.jq.is_some() || self.template.is_some()
    }

    /// Project, then run `--jq`. Returns every result the filter produced.
    ///
    /// Without `--jq` this is a single-element vector, so callers do not need two code paths.
    pub fn transform(&self, value: Value) -> Result<Vec<Value>, Error> {
        let projected = match self.fields {
            Some(fields) => project::project(value, fields)?,
            None => value,
        };
        match self.jq {
            Some(filter) => filter.run(&projected),
            None => Ok(vec![projected]),
        }
    }

    /// Transform and render one JSON document.
    pub fn render(&self, value: Value, term: &Term, out: &mut impl Write) -> Result<(), Error> {
        let results = self.transform(value)?;
        if let Some(template) = self.template {
            for value in &results {
                out.write_all(template.render(value, term)?.as_bytes())?;
            }
            return Ok(());
        }
        if self.jq.is_some() {
            // jq's own output rules: strings raw, everything else compact JSON, one per line.
            jq::write_results(&results, out)?;
            return Ok(());
        }
        for value in &results {
            write_json(value, term, out)?;
        }
        Ok(())
    }
}

/// Serialize a JSON document: **pretty on a TTY, compact when piped.**
///
/// A human reading a 40-field pull request needs the indentation; a program reading it wants
/// one line per document so `while read -r line` works, and pretty-printing costs bytes for no
/// benefit. `gh` makes the same split, and scripts rely on it.
pub fn write_json(value: &Value, term: &Term, out: &mut impl Write) -> io::Result<()> {
    if term.tty {
        serde_json::to_writer_pretty(&mut *out, value).map_err(io::Error::other)?;
    } else {
        serde_json::to_writer(&mut *out, value).map_err(io::Error::other)?;
    }
    out.write_all(b"\n")
}

/// Mime types that are safe to print to a terminal.
///
/// Everything else is treated as binary. The allowlist direction matters: a mime type we have
/// never seen should default to "do not dump this to the user's terminal", because the cost of
/// being wrong is a wedged terminal, not a missing byte.
fn is_text_mime(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    m.starts_with("text/")
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || matches!(
            m.as_str(),
            "application/json" | "application/ld+json" | "application/xml" | "application/x-yaml"
        )
}

/// Refuse to write binary data to a terminal.
///
/// This is `curl`'s guard, and it exists because a few kilobytes of a zip file interpreted as
/// terminal input will change the character set, disable line wrapping, or leave the terminal
/// in a state that needs `reset`. The remedy is named in the message, because "binary output
/// can mess up your terminal" without a next step is just an obstacle.
pub fn guard_binary(mime: &str, term: &Term, dest: &Dest, force: bool) -> Result<(), Error> {
    if force || matches!(dest, Dest::File(_)) || !term.tty || is_text_mime(mime) {
        return Ok(());
    }
    Err(Error::new(ErrorKind::Usage(format!(
        "refusing to write {mime} to the terminal; it would likely corrupt your session.\n\
         redirect it with `--output <file>`, pipe it somewhere, or pass --force to override"
    ))))
}

/// Stream a `Bytes` payload to its destination, returning the byte count.
///
/// Streamed rather than buffered so a 2 GB release asset never lands in RAM.
pub fn write_bytes(
    mime: &str,
    body: &mut dyn Read,
    dest: &Dest,
    term: &Term,
    force: bool,
) -> Result<u64, Error> {
    guard_binary(mime, term, dest, force)?;
    match dest {
        Dest::File(path) => {
            let mut file = std::fs::File::create(path)?;
            let n = io::copy(body, &mut file)?;
            file.flush()?;
            Ok(n)
        }
        Dest::Stdout => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            let n = io::copy(body, &mut lock)?;
            lock.flush()?;
            Ok(n)
        }
    }
}

/// The `--output <file>` destination for a text or JSON payload.
pub fn open_dest(dest: &Dest) -> io::Result<Box<dyn Write>> {
    match dest {
        Dest::Stdout => Ok(Box::new(io::stdout())),
        Dest::File(path) => Ok(Box::new(std::fs::File::create(path)?)),
    }
}

/// Path helper for the common "did the user ask for a file?" check.
pub fn dest_for(output: Option<&Path>) -> Dest {
    match output {
        Some(p) => Dest::File(p.to_path_buf()),
        None => Dest::Stdout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIELDS: &[FieldSpec] = &[
        FieldSpec { name: "number", kind: FieldKind::Int, doc: "index" },
        FieldSpec { name: "title", kind: FieldKind::Str, doc: "title" },
        FieldSpec { name: "state", kind: FieldKind::Str, doc: "state" },
    ];

    fn prs() -> Value {
        json!([
            {"number": 1, "title": "add a thing", "state": "open", "body": "long body"},
            {"number": 22, "title": "fix a thing", "state": "closed", "body": "another"}
        ])
    }

    fn render(p: &Pipeline, term: &Term) -> String {
        let mut buf = Vec::new();
        p.render(prs(), term, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Bug this prevents: running `--jq` before projection. With the wrong order,
    /// `--json number --jq '.[0]'` would return the full object because the filter saw the
    /// unprojected document, and `gea` would disagree with `gh` on a flag combination users
    /// actually type.
    #[test]
    fn projection_happens_before_jq() {
        let filter = Filter::compile(".[0]").unwrap();
        let fields = vec!["number".to_string()];
        let p = Pipeline::new().fields(Some(&fields)).jq(Some(&filter));
        assert_eq!(render(&p, &Term::piped()), "{\"number\":1}\n");
    }

    /// Bug this prevents: pretty-printing into a pipe (or compacting on a terminal), either of
    /// which breaks a habit `gh` established.
    #[test]
    fn json_is_pretty_on_a_tty_and_compact_when_piped() {
        let fields = vec!["number".to_string(), "title".to_string()];
        let p = Pipeline::new().fields(Some(&fields));
        let piped = render(&p, &Term::piped());
        assert_eq!(piped.lines().count(), 1, "{piped}");
        let tty = render(&p, &Term::tty(80));
        assert!(tty.lines().count() > 1, "{tty}");
        // Same data, so the compact form must be recoverable from the pretty one.
        let a: Value = serde_json::from_str(&piped).unwrap();
        let b: Value = serde_json::from_str(&tty).unwrap();
        assert_eq!(a, b);
    }

    /// The full triad on one document, both output modes. This is the golden that would catch a
    /// stage being reordered, skipped, or double-applied.
    #[test]
    fn pipeline_goldens() {
        let filter = Filter::compile(".[] | .title").unwrap();
        let template = Template::parse("{{range .}}{{tablerow .number .state}}{{end}}").unwrap();
        let fields = vec!["number".to_string(), "state".to_string()];
        let mut report = String::new();
        for (label, p) in [
            ("bare", Pipeline::new()),
            ("--json", Pipeline::new().fields(Some(&fields))),
            ("--jq", Pipeline::new().jq(Some(&filter))),
            ("--template", Pipeline::new().template(Some(&template))),
            (
                "--json + --template",
                Pipeline::new().fields(Some(&fields)).template(Some(&template)),
            ),
        ] {
            report.push_str(&format!("== {label} / tty\n{}", render(&p, &Term::tty(80))));
            report.push_str(&format!("== {label} / piped\n{}", render(&p, &Term::piped())));
        }
        insta::assert_snapshot!(report);
    }

    /// Bug this prevents: `is_explicit` returning false when only `--template` was given, so
    /// the command layer prints its human table *and* the template output.
    #[test]
    fn is_explicit_covers_all_three_flags() {
        let filter = Filter::compile(".").unwrap();
        let template = Template::parse("x").unwrap();
        let fields = vec!["number".to_string()];
        assert!(!Pipeline::new().is_explicit());
        assert!(Pipeline::new().fields(Some(&fields)).is_explicit());
        assert!(Pipeline::new().jq(Some(&filter)).is_explicit());
        assert!(Pipeline::new().template(Some(&template)).is_explicit());
    }

    /// Bug this prevents: dumping a zip file to the user's terminal and wedging it. Also
    /// prevents the over-correction: refusing when the user *did* redirect, piped, or forced.
    #[test]
    fn binary_to_a_tty_is_refused_unless_forced() {
        let tty = Term::tty(80);
        let zip = "application/zip";
        assert!(guard_binary(zip, &tty, &Dest::Stdout, false).is_err());
        assert!(guard_binary(zip, &tty, &Dest::Stdout, true).is_ok());
        assert!(guard_binary(zip, &tty, &Dest::File("/tmp/x".into()), false).is_ok());
        assert!(guard_binary(zip, &Term::piped(), &Dest::Stdout, false).is_ok());
        // Text is always fine.
        for mime in [
            "text/plain",
            "text/plain; charset=utf-8",
            "text/html",
            "application/json",
            "application/ld+json",
        ] {
            assert!(guard_binary(mime, &tty, &Dest::Stdout, false).is_ok(), "{mime}");
        }
        // An unknown mime defaults to binary, because a wedged terminal is worse.
        assert!(guard_binary("application/x-gitea-blob", &tty, &Dest::Stdout, false).is_err());
    }

    /// Bug this prevents: the refusal message being a dead end. Every local error owes the user
    /// a next step, and this one has three.
    #[test]
    fn the_refusal_names_a_remedy() {
        let err =
            guard_binary("application/zip", &Term::tty(80), &Dest::Stdout, false).unwrap_err();
        let ErrorKind::Usage(message) = &*err.kind else { panic!() };
        assert!(message.contains("--output"), "{message}");
        assert!(message.contains("--force"), "{message}");
    }

    /// Bug this prevents: `write_bytes` buffering the body, so a large artifact is read into
    /// memory before being written.
    #[test]
    fn bytes_stream_to_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.zip");
        let mut body: &[u8] = b"PK\x03\x04binary";
        let n = write_bytes(
            "application/zip",
            &mut body,
            &Dest::File(path.clone()),
            &Term::tty(80),
            false,
        )
        .unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&path).unwrap(), b"PK\x03\x04binary");
    }

    /// The bare-`--json` path, end to end, in both modes — this is the divergence from `gh`
    /// (stdout, exit 0) and the reason it exists.
    #[test]
    fn bare_json_discovery_end_to_end() {
        assert_eq!(project::resolve("", FIELDS).unwrap(), Selection::Discover);
        let mut piped = Vec::new();
        project::write_field_list(FIELDS, &Term::piped(), &mut piped).unwrap();
        assert_eq!(String::from_utf8(piped).unwrap(), "number\ntitle\nstate\n");

        let mut tty = Vec::new();
        project::write_field_list(FIELDS, &Term::tty(80), &mut tty).unwrap();
        let tty = String::from_utf8(tty).unwrap();
        assert!(tty.contains("number  int     index"), "{tty}");

        // `--json a,b` on the same table selects rather than discovers.
        assert_eq!(
            project::resolve("number,title", FIELDS).unwrap(),
            Selection::Fields(vec!["number".into(), "title".into()])
        );
    }
}

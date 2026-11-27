//! Shared plumbing for the Gitea-native groups (`times`, `stopwatch`, `wiki`, `mirror`,
//! `package`, `nodeinfo`).
//!
//! It lives under `times` for a mechanical reason that has since expired: `cmd/mod.rs` was frozen
//! while several waves of work ran in parallel, so a neutral `cmd/support.rs` could not be
//! declared. [`crate::cmd::support`] now exists and owns the genuinely common half — the field
//! adapter, the prompting gate, the `--output` writer, the test scaffolding — which this module
//! delegates to. What is left is this wave's own `Fields`/`Machine`/`discovery` facade and its
//! rendering helpers; folding those into `support` would mean merging a fourth output facade
//! into the two that are there, which is a change of behaviour rather than of location.
//!
//! # What is actually shared
//!
//! Three decisions that every layer-3 command has to make identically, or the tool stops
//! feeling like one tool:
//!
//! 1. **Bare `--json` is answered before any request.** `gea wiki list --json` prints the
//!    selectable field names and exits 0 — see `docs/output.md`, divergence 2 — and that has to
//!    work with no token, no network, and no configured host. So [`discovery`] is called from a
//!    group's synchronous `run`, *outside* `runtime::block_on`.
//! 2. **`--json`/`--jq`/`--template` suppress the human view entirely**, and are compiled once
//!    rather than per page.
//! 3. **A destructive action confirms on a terminal and demands `--yes` otherwise.** Never
//!    prompt when stdin is not a terminal: a command that blocks forever inside a CI job is
//!    worse than one that fails.

use std::io::Write;

use gitea_client::fields::FieldSpec as ClientFieldSpec;
use gitea_core::config::Config;
use gitea_core::error::{Error, Result};
use serde_json::Value;

use crate::global::GlobalOpts;
use crate::output::{self, Filter, Pipeline, Selection, Table, Template, Term, project};
use crate::runtime::Runtime;

/// The default for `-L/--limit`, from `docs/porcelain-conventions.md`.
/// Everything general this module used to define now lives in [`crate::cmd::support`]; these
/// re-exports keep the seven groups' call sites reading the same while there is one
/// implementation behind each name.
pub use crate::cmd::support::{
    banner, item_cap as item_limit, note, to_value as json_of, usage, writer,
};

// -------------------------------------------------------------------------------- field tables

/// Translate a generated field table into the output layer's own.
///
/// One line, because the adapter itself lives in [`crate::cmd::support::fields`]. There were six
/// copies of it and they had diverged over how an array of strings describes itself; see that
/// module.
pub fn specs(fields: &'static [ClientFieldSpec]) -> Vec<project::FieldSpec> {
    crate::cmd::support::fields::for_table(fields)
}

/// A field table for a document `gea` computes rather than fetches — `times list --total`.
///
/// `docs/porcelain-conventions.md` says never to invent field names, and this is the one
/// exception the rule allows for: the object is not the API's, so there are no API names to be
/// faithful to. Anything that *is* an API document uses the generated table instead.
pub const fn local(name: &'static str, kind: project::FieldKind, doc: &'static str) -> LocalField {
    LocalField { name, kind, doc }
}

/// One entry of a locally-defined field table. See [`local`].
#[derive(Debug, Clone, Copy)]
pub struct LocalField {
    pub name: &'static str,
    pub kind: project::FieldKind,
    pub doc: &'static str,
}

impl LocalField {
    fn to_spec(self) -> project::FieldSpec {
        project::FieldSpec { name: self.name, kind: self.kind, doc: self.doc }
    }
}

/// Which field table a subcommand's output follows.
///
/// `Generated` is the normal case. `Local` is for a computed document, and `None` is for a
/// command that produces no JSON document at all (`wiki view` emits markdown), where bare
/// `--json` must say so rather than print an empty list.
#[derive(Debug, Clone, Copy)]
pub enum Fields {
    Generated(&'static [ClientFieldSpec]),
    Local(&'static [LocalField]),
    None(&'static str),
}

impl Fields {
    fn resolve(self) -> std::result::Result<Vec<project::FieldSpec>, Error> {
        match self {
            Self::Generated(f) => Ok(specs(f)),
            Self::Local(f) => Ok(f.iter().copied().map(LocalField::to_spec).collect()),
            Self::None(why) => Err(usage(format!(
                "{why}, so there are no --json fields to select; drop --json to see it"
            ))),
        }
    }
}

// ------------------------------------------------------------------------------- output plans

/// Answer a bare `--json`, and validate a named field list, **before any request**.
///
/// Returns `true` when the field listing was printed and the command is done. Call this from
/// the group's synchronous `run`, before [`crate::runtime::block_on`]: field discovery needs
/// neither a credential nor a network, and answering it with "no Gitea host is set up yet"
/// would send a user to fix the wrong thing.
pub fn discovery(globals: &GlobalOpts, fields: Fields) -> Result<bool> {
    let Some(raw) = globals.json.as_deref() else { return Ok(false) };
    let table = fields.resolve()?;
    match project::resolve(raw, &table)? {
        Selection::Discover => {
            let term = Term::detect();
            let mut out = std::io::stdout().lock();
            project::write_field_list(&table, &term, &mut out)?;
            out.flush()?;
            Ok(true)
        }
        // Validated here so a mistyped field name is reported before a request is sent, with
        // the "did you mean" suggestion the taxonomy carries.
        Selection::Fields(_) => Ok(false),
    }
}

/// The compiled `--json`/`--jq`/`--template` triad.
///
/// Owned separately from the borrowed [`Pipeline`] so one compiled filter and one parsed
/// template are reused across pages instead of rebuilt per page.
#[derive(Debug)]
pub struct Machine {
    fields: Option<Vec<String>>,
    filter: Option<Filter>,
    template: Option<Template>,
}

impl Machine {
    /// `None` when the user asked for none of the three, which is the signal to render the
    /// command's own human view.
    pub fn compile(globals: &GlobalOpts, fields: Fields) -> Result<Option<Self>> {
        if !globals.wants_machine_output() {
            return Ok(None);
        }
        let selected = match globals.json.as_deref() {
            None => None,
            Some(raw) => match project::resolve(raw, &fields.resolve()?)? {
                // `discovery` already handled and printed this; reaching here means a caller
                // forgot to call it, so behave as if no fields were named rather than
                // printing the listing a second time from inside the async body.
                Selection::Discover => None,
                Selection::Fields(f) => Some(f),
            },
        };
        Ok(Some(Self {
            fields: selected,
            filter: globals.jq.as_deref().map(Filter::compile).transpose()?,
            template: globals.template.as_deref().map(Template::parse).transpose()?,
        }))
    }

    fn pipeline(&self) -> Pipeline<'_> {
        Pipeline::new()
            .fields(self.fields.as_deref())
            .jq(self.filter.as_ref())
            .template(self.template.as_ref())
    }

    /// Transform and write one JSON document.
    pub fn write(&self, globals: &GlobalOpts, term: &Term, value: Value) -> Result<()> {
        let mut out = writer(globals)?;
        out.write_all(self.render_to_string(term, value)?.as_bytes())?;
        out.flush()?;
        Ok(())
    }

    /// The same bytes, as a `String`.
    ///
    /// Exists so a test can snapshot `--json` output without a process, a socket, or a
    /// temporary file — which is what makes the machine-readable half of the output contract
    /// checkable at all.
    pub fn render_to_string(&self, term: &Term, value: Value) -> Result<String> {
        let mut buf: Vec<u8> = Vec::new();
        self.pipeline().render(value, term, &mut buf)?;
        // The pipeline writes JSON and template output, both of which are UTF-8 by construction.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

// ------------------------------------------------------------------------------ human output

/// A table pre-loaded with the terminal's shape.
pub fn table(term: &Term) -> Table {
    Table::new(term)
}

/// A table plus its banner, as a `String`, so it can be snapshotted.
pub fn rendered_table(term: &Term, mut t: Table, noun: &str, total: Option<u64>) -> String {
    if term.tty && !t.is_empty() {
        t.banner(banner(t.len(), total, noun));
    }
    t.render_to_string()
}

/// Write already-rendered human output to `--output` or stdout.
pub fn print(globals: &GlobalOpts, text: &str) -> Result<()> {
    let mut out = writer(globals)?;
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// The empty-list contract: exit 0, an empty table (or `[]`), and a note on a terminal.
///
/// Emptiness is not an error. `if gea mirror list; then` must test reachability, not whether
/// any mirror happens to exist.
pub fn empty(
    globals: &GlobalOpts,
    term: &Term,
    machine: Option<&Machine>,
    note_text: &str,
) -> Result<()> {
    match machine {
        Some(m) => m.write(globals, term, Value::Array(Vec::new()))?,
        None => note(term, note_text),
    }
    Ok(())
}

/// `-` for an absent value, so a column never collapses.
pub fn dash(s: &str) -> String {
    if s.trim().is_empty() { "-".to_owned() } else { s.to_owned() }
}

/// A timestamp as `2026-09-12 14:03` in the reader's own zone, or `-` for Gitea's "unset"
/// sentinel.
///
/// Local rather than UTC because the reader is a person deciding whether something happened this
/// morning, and `git log` sets that expectation.
pub fn when(t: Option<gitea_core::types::Timestamp>) -> String {
    when_in(t, &jiff::tz::TimeZone::system())
}

/// [`when`], with the zone named.
///
/// Split out so the formatting is testable: a test that went through [`when`] would assert a
/// different string on a developer's machine than in CI, which is how a snapshot becomes something
/// people re-accept without reading.
pub fn when_in(t: Option<gitea_core::types::Timestamp>, tz: &jiff::tz::TimeZone) -> String {
    match t {
        Some(ts) if !ts.is_unset() => {
            let z = ts.as_jiff().to_zoned(tz.clone());
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                z.year(),
                z.month(),
                z.day(),
                z.hour(),
                z.minute()
            )
        }
        _ => "-".to_owned(),
    }
}

// -------------------------------------------------------------------------------- interaction

/// True when prompting is allowed: **both** streams are terminals and nothing disabled it.
///
/// stdin as well as stdout, deliberately. `gea wiki delete x < /dev/null` on a terminal has
/// nobody to answer the question, and a prompt there hangs until the user works out why.
pub fn may_prompt(config: &Config, host: Option<&str>) -> bool {
    crate::cmd::support::interact::may_prompt_for(config, host)
}

/// Gate a destructive action: confirm on a terminal, require `--yes` otherwise.
pub fn confirm(rt: &Runtime, question: &str, yes: bool) -> Result<()> {
    crate::cmd::support::confirm_question(crate::cmd::support::can_prompt(rt), yes, question)
}

/// Ask for a secret, echoing nothing.
pub fn ask_secret(rt: &Runtime, question: &str, flag: &str) -> Result<String> {
    if !may_prompt(rt.config(), Some(rt.host().as_str())) {
        return Err(usage(format!("{question}; pass {flag}")));
    }
    inquire::Password::new(question)
        .without_confirmation()
        .with_display_mode(inquire::PasswordDisplayMode::Masked)
        .prompt()
        .map_err(|e| usage(format!("{question}: {e}")))
}

// --------------------------------------------------------------------------------- pagination

// ------------------------------------------------------------------------------- body sources

/// Where a body came from, resolved once so `-b`, `-F` and `-e` cannot disagree.
///
/// The flag meanings are fixed by `docs/porcelain-conventions.md` and copied exactly:
/// `-b/--body` is inline, `-F/--body-file` reads a file with `-` meaning stdin, and
/// `-e/--editor` opens `$EDITOR`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct BodyOpts {
    /// Body text, inline
    #[arg(short = 'b', long, value_name = "TEXT")]
    pub body: Option<String>,

    /// Read the body from a file; `-` reads stdin
    #[arg(short = 'F', long = "body-file", value_name = "PATH", conflicts_with = "body")]
    pub body_file: Option<String>,

    /// Compose the body in $EDITOR
    #[arg(short = 'e', long, conflicts_with_all = ["body", "body_file"])]
    pub editor: bool,
}

impl BodyOpts {
    pub fn given(&self) -> bool {
        self.body.is_some() || self.body_file.is_some() || self.editor
    }

    /// Read the body, or `None` when no source was named.
    ///
    /// `initial` seeds the editor buffer, which is what makes `wiki edit -e` an edit rather
    /// than a retype.
    pub fn read(&self, rt: &Runtime, initial: &str, ext: &str) -> Result<Option<String>> {
        if let Some(b) = &self.body {
            return Ok(Some(b.clone()));
        }
        if let Some(path) = &self.body_file {
            return Ok(Some(read_file(path)?));
        }
        if self.editor {
            return Ok(Some(edit(rt, initial, ext)?));
        }
        Ok(None)
    }
}

/// `-` is stdin; anything else is a path.
pub fn read_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| usage(format!("--body-file -: could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).map_err(|e| usage(format!("--body-file {path}: {e}")))
}

/// Open `$EDITOR` on a temporary file seeded with `initial`, and return what came back.
///
/// The suffix matters: editors pick their syntax highlighting and their wrapping from it, and
/// a wiki page edited as `.md` gets markdown mode.
pub fn edit(rt: &Runtime, initial: &str, ext: &str) -> Result<String> {
    let argv = editor_argv(rt)?;
    let dir = std::env::temp_dir();
    let path = dir.join(format!("gea-{}-{}.{ext}", std::process::id(), nonce()));
    std::fs::write(&path, initial)?;

    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .arg(&path)
        .status()
        .map_err(|e| usage(format!("could not run the editor {:?}: {e}", argv[0])));
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    let out = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if !status.success() {
        // An editor that exited non-zero usually means the user aborted (`:cq`), and using
        // whatever happened to be in the buffer would publish something they rejected.
        return Err(usage(format!(
            "the editor {:?} exited with {status}; nothing was sent",
            argv[0]
        )));
    }
    Ok(out?)
}

/// `GEA_EDITOR` → `gea config get editor` → `VISUAL` → `EDITOR` → `vi`.
///
/// Same precedence as `gh`, and the same fallback: an unset `$EDITOR` is the normal state on a
/// fresh machine, and refusing there would make `-e` unusable for exactly the people most
/// likely to reach for it.
fn editor_argv(rt: &Runtime) -> Result<Vec<String>> {
    let from_env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let spec = from_env("GEA_EDITOR")
        .or_else(|| rt.config().editor(Some(rt.host().as_str())))
        .or_else(|| from_env("VISUAL"))
        .or_else(|| from_env("EDITOR"))
        .unwrap_or_else(|| default_editor().to_owned());
    let argv = output::pager::split_command(&spec);
    if argv.is_empty() {
        return Err(usage(
            "the configured editor is empty; set $EDITOR or `gea config set editor`",
        ));
    }
    Ok(argv)
}

#[cfg(windows)]
const fn default_editor() -> &'static str {
    "notepad"
}

#[cfg(not(windows))]
const fn default_editor() -> &'static str {
    "vi"
}

/// A per-invocation suffix for the temporary file name.
///
/// Not security-relevant — the file lives in a directory the user already owns — but two `-e`
/// invocations in the same second must not collide.
fn nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------- @me

/// Resolve `@me` to the authenticated user, leaving any other name alone.
pub async fn resolve_user(rt: &Runtime, name: &str) -> Result<String> {
    if name != "@me" {
        return Ok(name.to_owned());
    }
    me(rt).await
}

/// The authenticated user's login, by asking the server.
///
/// One `GET /user` every time, on purpose. The login recorded in `hosts.toml` is *gea's name for
/// a credential*, not a verified identity: a rotated token, or a `$GITEA_TOKEN` exported against
/// a host whose entry names somebody else, and `@me` would quietly resolve to the wrong person.
/// `times add --user @me` writes a timesheet entry, so that is a wrong row under a real name
/// rather than a cosmetic slip. The request is also the only thing that proves the token still
/// works before a mutation is attempted.
pub async fn me(rt: &Runtime) -> Result<String> {
    let api = gitea_client::Api::new(rt.client().clone());
    Ok(api.user().get_current().await?.login)
}

/// Test scaffolding shared by the seven groups' unit tests.
///
/// A `#[cfg(test)]` module rather than a `tests/` file, because everything worth testing here is
/// crate-private: the renderers take `&Term` and the fetchers take `&Api`, and an integration test
/// would only be able to reach them by widening their visibility for no other reason.
#[cfg(test)]
pub mod testing {
    use super::*;

    pub use crate::cmd::support::testing::{json_globals, method, one_page, term};

    /// An `Api` over a fake transport, at the base this wave's snapshots name.
    pub fn api(
        fake: std::sync::Arc<gitea_core::http::transport::FakeTransport>,
    ) -> gitea_client::Api {
        crate::cmd::support::testing::api_at(crate::cmd::support::testing::EXAMPLE, fake)
    }

    /// Render a value the way `--json` would, piped (so: compact, one line).
    pub fn as_json(fields: Fields, select: &str, value: Value) -> String {
        let globals = json_globals(select);
        Machine::compile(&globals, fields)
            .expect("the field list is valid")
            .expect("--json was set")
            .render_to_string(&Term::piped(), value)
            .expect("rendering a JSON document cannot fail")
    }
}

#[cfg(test)]
mod tests {
    use gitea_core::error::ErrorKind;

    use super::*;

    /// Bug this prevents: bare `--json` on a command with no JSON document printing an empty
    /// listing, which reads as "this resource has no fields" rather than "wrong flag".
    #[test]
    fn a_command_with_no_json_document_says_so() {
        let g = GlobalOpts { json: Some(String::new()), ..GlobalOpts::default() };
        let e = discovery(&g, Fields::None("`wiki view` prints the page's markdown")).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("markdown"), "{e}");
    }

    /// Bug this prevents: an unknown `--json` field reaching the server, so the user pays a
    /// round trip (and possibly a write) to learn they made a typo.
    #[test]
    fn an_unknown_json_field_is_rejected_before_any_request() {
        let g = GlobalOpts { json: Some("issue_idx".to_owned()), ..GlobalOpts::default() };
        let e = discovery(&g, Fields::Generated(gitea_client::fields::FIELDS_TRACKED_TIME))
            .unwrap_err();
        assert_eq!(e.exit_code(), 2);
        // The taxonomy carries the suggestion; this asserts we route through it rather than
        // building our own string.
        assert!(matches!(&*e.kind, ErrorKind::UnknownJsonField { .. }), "{e:?}");
    }

    #[test]
    fn an_absent_value_becomes_a_dash_so_a_column_never_collapses() {
        assert_eq!(dash(""), "-");
        assert_eq!(dash("   "), "-");
        assert_eq!(dash("main"), "main");
    }

    /// Bug this prevents: rendering Gitea's "unset" sentinel — Go's zero time, or the unix
    /// epoch — as `0001-01-01` or `1970-01-01` in a column.
    #[test]
    fn an_unset_timestamp_renders_as_a_dash() {
        assert_eq!(when(None), "-");
        assert_eq!(when(Some(gitea_core::types::Timestamp::default())), "-");
        // Go's zero time, which is what Gitea actually sends for an unset field.
        let go_zero = gitea_core::types::Timestamp::from_jiff(
            "0001-01-01T00:00:00Z".parse().expect("a valid instant"),
        );
        assert_eq!(when(Some(go_zero)), "-");
    }

    /// Pinned in UTC, because the point is the *format* and going through the system zone would
    /// make the assertion depend on where it runs.
    #[test]
    fn a_real_timestamp_is_rendered_to_the_minute() {
        let ts = gitea_core::types::Timestamp::from_jiff(
            "2026-09-10T09:15:42Z".parse().expect("a valid instant"),
        );
        assert_eq!(when_in(Some(ts), &jiff::tz::TimeZone::UTC), "2026-09-10 09:15");
    }
}

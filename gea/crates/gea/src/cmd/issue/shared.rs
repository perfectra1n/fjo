//! Porcelain plumbing shared by the `issue`, `label`, `milestone` and `release` groups.
//!
//! What is left here is the part that is **about issues, labels, milestones and releases**: the
//! sparse patch bodies, the newtyped API wrappers, and the name-to-id resolvers. The general
//! plumbing this file used to carry — the stderr note, `usage`, the editor split, the limit
//! chain, `@me` — moved to [`crate::cmd::support`] once `cmd/mod.rs` could be edited again. There
//! is one `@me` resolver, which is what stops `-a @me` from coming to mean two things.
//!
//! Three decisions here are worth reading before using any of it.
//!
//! # 1. Sparse patch bodies, not the generated `Edit*Option` models
//!
//! The Gitea Swagger document does not record which properties are Go pointers, so the
//! generated `Edit*Option` structs have plain `String`/`bool`/`Vec` fields with no
//! `skip_serializing_if`. Every one of them therefore serialises **every** field, and Gitea
//! reads a present-but-empty field as an instruction:
//!
//! | sent | Gitea does |
//! | --- | --- |
//! | `"body": ""` | clears the issue body |
//! | `"state": ""` | `"" != "closed"`, so it **reopens a closed issue** |
//! | `"milestone": 0` | unsets the milestone |
//! | `"assignees": []` | unassigns everybody |
//! | `"draft": false` | **publishes a draft release** |
//!
//! So `gea issue close 42` written the obvious way would close the issue and wipe its body,
//! and `gea release edit v1 -n notes` would publish a draft. The bodies in this module are
//! `Option`-per-field and skip `None`, so an unmentioned field is genuinely absent from the
//! JSON.
//!
//! **The emitter has since been fixed** — a generated `Edit*Option` now skips the fields nobody
//! set, which `the_generated_edit_option_sends_nothing_it_was_not_given` below pins. These
//! structs stay because they are also how this module says "clear this field" as distinct from
//! "leave it alone", which the generated models still cannot express.
//!
//! # 2. Field discovery runs before the runtime exists
//!
//! [`Out::prepare`] is called at the top of a synchronous `run`, before `block_on`, so bare
//! `--json` needs no host, no token, and no network. That is a contract; see `docs/output.md`.
//!
//! Human output goes to stdout, notes and progress to stderr:
//!
//! `Out` owns the destination so that a command never reaches for `println!`, and so that
//! tests can capture what a command wrote without a subprocess.
//!
//! # 3. `IssueIndex` never becomes an `i64` outside this module
//!
//! The generated client takes `index: i64` and `id: i64` — it cannot tell them apart. The
//! wrappers at the bottom of this file take the newtypes, so every call site in layers above
//! is type-checked, and `.get()` appears in exactly one place per operation.

use std::io::{self, Write};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use gitea_client::Api;
use gitea_client::gitea_core::config::{Prompt, SystemEnv};
use gitea_client::meta_types::FieldSpec as GenSpec;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::Request;
use gitea_core::types::ids::{
    AttachmentId, CommentId, IssueIndex, LabelId, MilestoneId, ReleaseId,
};
use gitea_core::types::{RepoSlug, Timestamp};
use gitea_model::{Comment, Issue, Label, Milestone, Release};
use serde::Serialize;
use serde_json::Value;

use crate::cmd::support::{self, machine::Triad};
use crate::global::GlobalOpts;
use crate::output::template::funcs::timeago_between;
use crate::output::{self, Dest, Selection, Table, Term, project};
use crate::runtime::Runtime;

// ------------------------------------------------------------------------------- output

/// Where a command's primary output goes.
///
/// `Buffer` exists so that a snapshot test can assert on what a command *wrote* rather than on
/// what it returned. Without it every rendering test would need a subprocess, and subprocess
/// tests cannot inject a `FakeTransport`.
#[derive(Clone)]
enum Target {
    Dest(Dest),
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

#[cfg(test)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("output buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The `--json` / `--jq` / `--template` triad, resolved once, plus the destination.
pub struct Out {
    triad: Triad,
    target: Target,
}

impl Out {
    /// Resolve the output flags against a resource's generated field table.
    ///
    /// `Ok(None)` means bare `--json` already printed the field list and the command is done —
    /// on **stdout**, with exit **0**, which is `gea`'s deliberate divergence from `gh`.
    pub fn prepare(globals: &GlobalOpts, table: &'static [GenSpec]) -> Result<Option<Self>> {
        let specs = support::fields::for_table(table);
        let fields = match globals.json.as_deref() {
            None => None,
            Some(raw) => match project::resolve(raw, &specs)? {
                Selection::Discover => {
                    let term = Term::detect();
                    let mut out = io::stdout().lock();
                    project::write_field_list(&specs, &term, &mut out)?;
                    out.flush()?;
                    return Ok(None);
                }
                Selection::Fields(f) => Some(f),
            },
        };
        Ok(Some(Self {
            triad: Triad::compile(globals, fields)?,
            target: Target::Dest(output::dest_for(globals.output.as_deref())),
        }))
    }

    /// An `Out` that renders into `buf`. Tests only; `prepare` is the real entry point.
    #[cfg(test)]
    pub fn to_buffer(
        globals: &GlobalOpts,
        table: &'static [GenSpec],
        buf: &Arc<Mutex<Vec<u8>>>,
    ) -> Self {
        let mut out = Self::prepare(globals, table)
            .expect("test output flags parse")
            .expect("test does not use bare --json");
        out.target = Target::Buffer(Arc::clone(buf));
        out
    }

    /// True when the user asked for machine-readable output, so the human renderer must not run.
    pub fn is_machine(&self) -> bool {
        self.triad.is_explicit()
    }

    fn writer(&self) -> Result<Box<dyn Write + '_>> {
        Ok(match &self.target {
            Target::Dest(d) => output::open_dest(d)?,
            #[cfg(test)]
            Target::Buffer(b) => Box::new(SharedBuf(Arc::clone(b))),
        })
    }

    /// Render a value through `--json`/`--jq`/`--template`.
    pub fn machine(&self, value: Value, term: &Term) -> Result<()> {
        let mut w = self.writer()?;
        self.triad.render(value, term, &mut w)
    }

    /// Render a table (padded on a terminal, TSV when piped).
    pub fn table(&self, table: &Table) -> Result<()> {
        let mut w = self.writer()?;
        table.render(&mut w)?;
        w.flush()?;
        Ok(())
    }

    /// Render pre-formatted human text — a detail view, a created object's URL.
    pub fn text(&self, text: &str) -> Result<()> {
        let mut w = self.writer()?;
        w.write_all(text.as_bytes())?;
        w.flush()?;
        Ok(())
    }
}

/// Everything a porcelain command in these four groups needs.
pub struct Cx {
    pub api: Api,
    pub term: Term,
    pub out: Out,
    /// The instance's web root, for `-w/--web` and for `html_url` fallbacks.
    pub web: String,
    repo: Option<RepoSlug>,
    prompt: bool,
    editor: String,
    browser: Option<String>,
    debug: bool,
}

impl Cx {
    /// For a command that needs a repository. Resolution happens here, once.
    pub fn in_repo(rt: &Runtime, globals: &GlobalOpts, out: Out) -> Result<Self> {
        let slug = rt.repo(globals)?.slug.clone();
        Ok(Self { repo: Some(slug), ..Self::global(rt, globals, out) })
    }

    /// For a command that works without one — `gea label list --org acme`.
    pub fn global(rt: &Runtime, _globals: &GlobalOpts, out: Out) -> Self {
        let env = &SystemEnv;
        let host = Some(rt.host().as_str());
        Self {
            api: Api::new(rt.client().clone()),
            term: *rt.term(),
            out,
            web: rt.client().web_base().to_owned(),
            repo: None,
            prompt: rt.config().prompt(host) == Prompt::Enabled && !prompt_disabled(env),
            editor: rt.config().resolved_editor(host, env),
            browser: rt.config().resolved_browser(host, env),
            debug: rt.is_debug(),
        }
    }

    /// A context wired to a fake transport, for unit tests.
    #[cfg(test)]
    pub fn for_test(api: Api, slug: Option<RepoSlug>, term: Term, out: Out) -> Self {
        Self {
            web: "https://git.example.org".to_owned(),
            api,
            term,
            out,
            repo: slug,
            prompt: false,
            editor: "true".to_owned(),
            browser: None,
            debug: false,
        }
    }

    pub fn repo(&self) -> Result<&RepoSlug> {
        self.repo.as_ref().ok_or_else(|| {
            support::usage(
                "this command needs a repository; pass -R owner/name or run inside a clone",
            )
        })
    }

    pub fn owner(&self) -> Result<&str> {
        Ok(self.repo()?.owner.as_str())
    }

    pub fn name(&self) -> Result<&str> {
        Ok(self.repo()?.name.as_str())
    }

    /// Whether an interactive prompt is allowed: **both** streams are terminals, and prompting
    /// has not been switched off. Stdout matters as much as stdin — a prompt written into a
    /// pipe is invisible, and the command then looks hung.
    pub fn can_prompt(&self) -> bool {
        self.prompt && support::interact::streams_are_terminals()
    }

    pub fn trace(&self, message: &str) {
        if self.debug {
            eprintln!("debug: {message}");
        }
    }

    /// Open a URL, announcing it on stderr first.
    ///
    /// The announcement is not decoration: on a headless box `open` may silently do nothing,
    /// and the printed URL is then the whole output the user needs.
    pub fn browse(&self, url: &str) -> Result<()> {
        eprintln!("Opening {url} in your browser.");
        let result = match &self.browser {
            Some(b) => open::with(url, b),
            None => open::that(url),
        };
        result.map_err(|e| {
            support::usage(format!("could not open a browser for {url}: {e}; open it yourself, or set `gea config set browser <cmd>`"))
        })
    }

    /// `$EDITOR` on a temporary file, returning what came back.
    ///
    /// The suffix is `.md` so an editor applies markdown highlighting and wrapping, which is
    /// what the text actually is.
    pub fn edit_text(&self, initial: &str) -> Result<String> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "gea-{}-{}.md",
            std::process::id(),
            Timestamp::now().as_jiff().as_nanosecond()
        ));
        std::fs::write(&path, initial)?;

        let mut parts = crate::output::pager::split_command(&self.editor);
        if parts.is_empty() {
            return Err(support::usage(
                "no editor is configured; set $EDITOR or `gea config set editor <cmd>`",
            ));
        }
        let program = parts.remove(0);
        let status =
            std::process::Command::new(&program).args(&parts).arg(&path).status().map_err(|e| {
                support::usage(format!("could not run the editor {program:?}: {e}"))
            })?;
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            return Err(support::usage(format!(
                "the editor {program:?} exited with {status}; nothing was sent"
            )));
        }
        let text = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        Ok(text)
    }

    /// A destructive action's confirmation. `--yes` skips it; a non-terminal requires it.
    pub fn confirm(&self, what: &str, yes: bool) -> Result<()> {
        support::confirm_action(self.can_prompt(), yes, what)
    }

    /// Ask for one line of text. Callers must check [`Cx::can_prompt`] first.
    pub fn ask(&self, message: &str) -> Result<String> {
        support::interact::ask(message, None)
    }

    /// Resolve `@me` against the authenticated user, for any user-valued flag.
    pub async fn resolve_user(&self, who: &str) -> Result<String> {
        if who != "@me" {
            return Ok(who.to_owned());
        }
        support::me(&self.api).await
    }

    pub async fn resolve_users(&self, who: &[String]) -> Result<Vec<String>> {
        support::resolve_me(&self.api, who).await
    }
}

fn prompt_disabled(env: &dyn gitea_core::config::Env) -> bool {
    env.get("GEA_PROMPT_DISABLED").is_some_and(|v| !v.is_empty())
}

/// Where a title and a body came from on the command line.
///
/// Held as one struct so that the precedence rules — and the "first line is the title" rule —
/// live in one place instead of being re-derived by `issue create`, `issue comment` and
/// `release create`.
#[derive(Debug, Default, Clone)]
pub struct BodyFlags<'a> {
    pub body: Option<&'a str>,
    pub body_file: Option<&'a str>,
    pub editor: bool,
}

impl BodyFlags<'_> {
    /// The body as written on the command line, with no editor involved.
    ///
    /// `-F -` reads stdin, which is why this takes the reader: a test must not need a real one.
    pub fn read(&self, stdin: &mut dyn io::Read) -> Result<Option<String>> {
        match (self.body, self.body_file) {
            (Some(_), Some(_)) => Err(support::usage(
                "-b/--body and -F/--body-file both set the body; use one of them",
            )),
            (Some(b), None) => Ok(Some(b.to_owned())),
            (None, Some("-")) => {
                let mut buf = String::new();
                stdin.read_to_string(&mut buf)?;
                Ok(Some(buf))
            }
            (None, Some(path)) => Ok(Some(
                std::fs::read_to_string(path)
                    .map_err(|e| support::usage(format!("-F/--body-file {path}: {e}")))?,
            )),
            (None, None) => Ok(None),
        }
    }
}

/// `3 minutes ago`, `2 days ago`, `—` for an unset timestamp.
///
/// The doc comment this replaces said a second implementation "would disagree in the first
/// week", and it did. The hand-rolled buckets here said `about 3 minutes ago` where the
/// documented helper says `3 minutes ago`, `just now` where it says `less than a minute ago`,
/// and had no future case at all — so a Gitea whose clock ran a minute ahead of the client's
/// rendered as `just now` in a column and `in 1 minute` under
/// `--template '{{timeago .updated_at}}'`, for the same field of the same response.
///
/// [`timeago_between`] is the one implementation, so this delegates to it rather than matching
/// its vocabulary by hand — matching by hand is what failed. Only the unset case stays local:
/// the template helper renders an absent timestamp as the empty string, which is right in a
/// template and wrong in a table, where a blank cell is indistinguishable from a missing column.
pub fn timeago(ts: Option<Timestamp>) -> String {
    match ts.filter(|t| !t.is_unset()) {
        Some(t) => timeago_between(jiff::Timestamp::now(), t.as_jiff()),
        None => "—".to_owned(),
    }
}

/// `2026-09-12` for a date-only column, `—` when unset.
pub fn date(ts: Option<Timestamp>) -> String {
    match ts.filter(|t| !t.is_unset()) {
        Some(t) => t.as_jiff().to_zoned(jiff::tz::TimeZone::UTC).strftime("%Y-%m-%d").to_string(),
        None => "—".to_owned(),
    }
}

/// A label's name, painted in the label's own colour on a colour terminal.
///
/// Gitea stores the colour as `RRGGBB` (sometimes with a leading `#`). It is applied as a
/// **foreground** colour: as a background it would be unreadable against half the themes in
/// use, since Gitea does not record whether the label was designed for a light or dark one.
pub fn label_chip(term: &Term, label: &Label) -> String {
    match (term.color, rgb(&label.color)) {
        (true, Some((r, g, b))) => {
            let style = anstyle::Style::new()
                .fg_color(Some(anstyle::Color::Rgb(anstyle::RgbColor(r, g, b))));
            output::color::paint(term, style, &label.name)
        }
        _ => label.name.clone(),
    }
}

/// `#e11d21` or `e11d21` into components. `None` for anything else, so a server that starts
/// sending names instead of hex degrades to plain text rather than to a panic.
fn rgb(color: &str) -> Option<(u8, u8, u8)> {
    let hex = color.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// A very small markdown renderer for terminal detail views.
///
/// No markdown crate: adding a dependency is out of scope for this change, and the useful
/// subset is small. Headings become bold, list bullets become `•`, fenced code is indented and
/// dimmed, and inline emphasis markers are dropped. Everything else is passed through
/// verbatim, because mangling text nobody asked to be reformatted is worse than leaving a `*`
/// on screen. When output is not a terminal the text is returned unchanged — a piped body must
/// stay byte-identical to what the server holds.
pub fn markdown(term: &Term, text: &str) -> String {
    if !term.tty {
        return text.to_owned();
    }
    let bold = anstyle::Style::new().bold();
    let dim = anstyle::Style::new().dimmed();
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_code = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str(&output::color::paint(term, dim, &format!("    {trimmed}")));
        } else if let Some(rest) = trimmed.trim_start().strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            out.push_str(&output::color::paint(term, bold, heading));
        } else if let Some(rest) = bullet(trimmed) {
            out.push_str(&format!("  • {}", inline(rest)));
        } else {
            out.push_str(&inline(trimmed));
        }
        out.push('\n');
    }
    out
}

fn bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m))
}

/// Drop **paired** `**` and `__` emphasis markers. Single `*`/`_` are left alone, and so are
/// backticks: in a terminal a backtick is the only signal that `--json` is a literal rather
/// than prose, and unlike emphasis, removing it destroys the one thing the span was carrying.
///
/// # Why the pairing test
///
/// This used to replace unconditionally, which ate any line with an ODD number of markers:
///
/// | input | was | is |
/// | --- | --- | --- |
/// | `capacity is 2 ** 10 bytes` | `capacity is 2  10 bytes` | unchanged |
/// | `**********` (a rule line) | (empty line) | unchanged |
/// | `the separator is ***` | `the separator is *` | unchanged |
/// | `GLOB is a__b` | `GLOB is ab` | unchanged |
///
/// The first turns a true statement false, and the second silently deletes a line that was
/// carrying meaning. Both reach `gea issue view` and `gea release view`, on a TTY only.
///
/// The rule is `cmd/repo/view.rs::inline`'s, arrived at independently there and correct; this
/// copy had drifted. It is a heuristic, not a parser, and it is still wrong for an even number
/// of markers that were never emphasis — `if __name__ == "__main__":` renders as
/// `if name == "main":` here and in `repo`'s copy alike. A real fix is a markdown parser, which
/// is a larger decision than either renderer should make on its own.
fn inline(line: &str) -> String {
    let mut out = line.to_owned();
    for marker in ["**", "__"] {
        let n = out.matches(marker).count();
        if n >= 2 && n.is_multiple_of(2) {
            out = out.replace(marker, "");
        }
    }
    out
}

// --------------------------------------------------------------- limits and state values

/// `open` | `closed` | `all`, defaulting to `open` for list commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum State {
    #[default]
    Open,
    Closed,
    All,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

// ------------------------------------------------------------------ sparse patch bodies

/// A partial `PATCH /repos/{o}/{r}/issues/{index}` body. See the module docs for why this is
/// not `gitea_model::EditIssueOption`.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct IssuePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Gitea replaces the whole set, so a caller that means "add one" must read first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unset_due_date: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
}

impl IssuePatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A partial `PATCH /repos/{o}/{r}/milestones/{id}` body.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct MilestonePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_on: Option<Timestamp>,
}

impl MilestonePatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A partial `PATCH` body for a repository or organization label.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct LabelPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_archived: Option<bool>,
}

impl LabelPatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A partial `PATCH /repos/{o}/{r}/releases/{id}` body.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct ReleasePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<bool>,
}

impl ReleasePatch {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

// ------------------------------------------------------------------ newtyped API wrappers
//
// The generated client takes `index: i64` and `id: i64`. Everything above these functions
// speaks in newtypes, so `IssueIndex` and `CommentId` cannot be swapped by accident. `.get()`
// appears once per operation, here, where the surrounding line names the route it builds.

pub async fn get_issue(cx: &Cx, index: IssueIndex) -> Result<Issue> {
    // GET /repos/{owner}/{repo}/issues/{index} — the number users type as `#42`.
    cx.api.issue().get_issue(cx.owner()?, cx.name()?, index.get()).await
}

pub async fn patch_issue(cx: &Cx, index: IssueIndex, patch: &IssuePatch) -> Result<Issue> {
    let path = format!(
        "/repos/{}/{}/issues/{}",
        gitea_core::http::encode::seg(cx.owner()?),
        gitea_core::http::encode::seg(cx.name()?),
        index
    );
    cx.api.client().json(Request::patch(path).json_body(patch)?).await
}

pub async fn patch_milestone(
    cx: &Cx,
    id: MilestoneId,
    patch: &MilestonePatch,
) -> Result<Milestone> {
    let path = format!(
        "/repos/{}/{}/milestones/{}",
        gitea_core::http::encode::seg(cx.owner()?),
        gitea_core::http::encode::seg(cx.name()?),
        id
    );
    cx.api.client().json(Request::patch(path).json_body(patch)?).await
}

pub async fn patch_release(cx: &Cx, id: ReleaseId, patch: &ReleasePatch) -> Result<Release> {
    let path = format!(
        "/repos/{}/{}/releases/{}",
        gitea_core::http::encode::seg(cx.owner()?),
        gitea_core::http::encode::seg(cx.name()?),
        id
    );
    cx.api.client().json(Request::patch(path).json_body(patch)?).await
}

/// `PATCH /repos/{o}/{r}/labels/{id}` or `PATCH /orgs/{org}/labels/{id}`.
pub async fn patch_label(
    cx: &Cx,
    scope: &LabelScope,
    id: LabelId,
    patch: &LabelPatch,
) -> Result<Label> {
    let path = match scope {
        LabelScope::Org(org) => {
            format!("/orgs/{}/labels/{}", gitea_core::http::encode::seg(org), id)
        }
        LabelScope::Repo(slug) => format!(
            "/repos/{}/{}/labels/{}",
            gitea_core::http::encode::seg(&slug.owner),
            gitea_core::http::encode::seg(&slug.name),
            id
        ),
    };
    cx.api.client().json(Request::patch(path).json_body(patch)?).await
}

/// Whether a label lives on a repository or on an organization.
///
/// Gitea has both, and they are different objects with different ids on different routes —
/// a fact `gh` has no equivalent for, and the reason `--org` exists on every `label`
/// subcommand.
#[derive(Debug, Clone)]
pub enum LabelScope {
    Repo(RepoSlug),
    Org(String),
}

impl std::fmt::Display for LabelScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repo(slug) => write!(f, "{slug}"),
            Self::Org(org) => write!(f, "organization {org}"),
        }
    }
}

/// Every label in a scope, following pagination to the end.
pub async fn all_labels(api: &Api, scope: &LabelScope) -> Result<Vec<Label>> {
    let mut out = Vec::new();
    match scope {
        LabelScope::Repo(slug) => {
            let q = gitea_client::query::IssueListLabelsQuery::default();
            let mut stream = api.issue().list_labels(&slug.owner, &slug.name, &q);
            while let Some(item) = stream.next().await {
                out.push(item?);
            }
        }
        LabelScope::Org(org) => {
            let q = gitea_client::query::OrgListLabelsQuery::default();
            let mut stream = api.org().list_labels(org, &q);
            while let Some(item) = stream.next().await {
                out.push(item?);
            }
        }
    }
    Ok(out)
}

/// Resolve label **names** to the ids `CreateIssueOption.labels` and
/// `CreatePullRequestOption.labels` want.
///
/// The create endpoint takes ids only, so a porcelain `-l bug` has to look them up; without
/// this, `gea issue create -l bug` would be `gea issue create -l 4`, which nobody can type
/// from memory. Repository labels are searched first, then the owning organization's, because
/// that is the order Gitea's own UI offers them in. The match is case-insensitive: label
/// names are display strings, and `Bug` vs `bug` is not a distinction worth a 404.
///
/// `gea pr` reaches for this too, which is why it takes an `Api` and a slug rather than a
/// [`Cx`]. Gitea models a pull request *as* an issue — same index space, same labels, same
/// `/issues/{index}` route — so `pr` depending on the issue group's label resolver is the real
/// shape of the data, not a leftover. The copy `pr` used to carry searched only the repository's
/// own labels, so `gea pr create -l <an org label>` failed with "has no label called" for a
/// label the server would have accepted.
pub async fn label_ids(api: &Api, slug: &RepoSlug, names: &[String]) -> Result<Vec<i64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut known = all_labels(api, &LabelScope::Repo(slug.clone())).await?;
    // Only pay for the organization's labels if a name is still unaccounted for: on a
    // user-owned repository the `/orgs/{owner}` call is a guaranteed 404.
    if names.iter().any(|n| !known.iter().any(|l| l.name.eq_ignore_ascii_case(n)))
        && let Ok(org) = all_labels(api, &LabelScope::Org(slug.owner.clone())).await
    {
        known.extend(org);
    }

    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let hit = known
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| label_missing(name, &known, slug))?;
        ids.push(hit.id.get());
    }
    Ok(ids)
}

/// "there is no label called that, and here are the ones there are".
///
/// A `Usage` error rather than `ErrorKind::Validation`: nothing was sent, so a headline of "the
/// server rejected the values in this request (HTTP 422)" would be a lie about where the problem
/// is. The taxonomy has no variant for "a name you typed does not exist locally, and here is the
/// list" — reported alongside this change; `ResourceNotFound` is the right shape but carries no
/// room for the list, which is the part that makes the message useful.
fn label_missing(name: &str, known: &[Label], slug: &RepoSlug) -> Error {
    let mut available: Vec<&str> = known.iter().map(|l| l.name.as_str()).collect();
    available.sort_unstable();
    support::usage(format!(
        "{slug} has no label named {name:?}; it has: {}",
        if available.is_empty() { "none".to_owned() } else { available.join(", ") }
    ))
}

/// Resolve a milestone **title** to its id.
///
/// `-m/--milestone` is by title in every porcelain command, by the conventions, because a
/// milestone id is not something a human has. Both open and closed milestones are searched:
/// closing a milestone must not stop `gea issue list -m 1.0` from working.
///
/// **Two milestones may share a title.** Gitea does not enforce uniqueness — creating `1.0`
/// twice succeeds and yields two ids — so an ambiguous title is refused rather than resolved to
/// whichever one the server listed first. Silently picking one is the same class of bug as
/// confusing an `IssueIndex` with an `IssueId`: it succeeds, and it operates on the wrong object.
pub async fn milestone_by_title(cx: &Cx, title: &str) -> Result<Milestone> {
    let q = gitea_client::query::IssueGetMilestonesListQuery::default().with_state("all");
    let mut stream = cx.api.issue().get_milestones_list(cx.owner()?, cx.name()?, &q);
    let mut hits: Vec<Milestone> = Vec::new();
    let mut titles: Vec<String> = Vec::new();
    while let Some(item) = stream.next().await {
        let m = item?;
        if m.title.eq_ignore_ascii_case(title) {
            hits.push(m);
        } else {
            titles.push(m.title);
        }
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => {
            titles.sort();
            // `Usage`, for the reason given on `label_missing`: this is a name the user typed
            // that does not exist, discovered locally, and the list of what does exist is the
            // useful part.
            Err(support::usage(format!(
                "{} has no milestone titled {title:?}; it has: {}",
                cx.repo()?,
                if titles.is_empty() { "none".to_owned() } else { titles.join(", ") }
            )))
        }
        n => Err(support::usage(format!(
            "{} has {n} milestones titled {title:?} (ids {}); Gitea does not require titles to \
             be unique, so rename or delete one — gea will not guess which you meant",
            cx.repo()?,
            hits.iter().map(|m| m.id.to_string()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// A release by tag, with a message that says which tag when there is none.
pub async fn release_by_tag(cx: &Cx, tag: &str) -> Result<Release> {
    cx.api
        .repo()
        .get_release_by_tag(cx.owner()?, cx.name()?, tag)
        .await
        .map_err(|e| retarget_404(e, "release", tag, cx.repo().ok()))
}

/// An asset of a release, by name.
pub fn asset_by_name<'a>(release: &'a Release, name: &str) -> Option<&'a gitea_model::Attachment> {
    release.assets.iter().find(|a| a.name == name)
}

pub fn attachment_id(asset: &gitea_model::Attachment) -> AttachmentId {
    asset.id
}

pub fn comment_id(comment: &Comment) -> CommentId {
    comment.id
}

/// Re-point a bare 404 at the thing the user actually named.
///
/// The client's classifier only knows the path; it cannot know that `/releases/tags/v9` was a
/// tag the user typed. Never swallows the server's own message — a non-404 is returned as-is.
fn retarget_404(e: Error, kind: &'static str, id: &str, slug: Option<&RepoSlug>) -> Error {
    match &*e.kind {
        // The server's own words are carried across, not dropped: Gitea answers some 404s with
        // a body that is the entire diagnosis, and re-pointing the error at the tag the user
        // typed must not cost that sentence.
        ErrorKind::ResourceNotFound { server_message, .. } => {
            Error::new(ErrorKind::ResourceNotFound {
                kind,
                id: id.to_owned(),
                slug: slug.map(RepoSlug::to_string),
                server_message: server_message.clone(),
            })
        }
        ErrorKind::RouteNotFound { .. } => Error::new(ErrorKind::ResourceNotFound {
            kind,
            id: id.to_owned(),
            slug: slug.map(RepoSlug::to_string),
            server_message: None,
        }),
        _ => e,
    }
}

#[cfg(test)]
mod tests {

    /// Found by diffing this against `cmd/repo/view.rs::inline` on the same inputs, after the
    /// two were noticed to disagree. Every row here was wrong before the pairing test.
    #[test]
    fn an_odd_run_of_markers_is_not_emphasis_and_must_survive() {
        for line in [
            "capacity is 2 ** 10 bytes",
            "the separator is ***",
            "**********",
            "**Note: this is unclosed",
            "GLOB is a__b",
        ] {
            assert_eq!(inline(line), line, "an odd run of markers was eaten: {line:?}");
        }
    }

    /// The pairing test must not stop it doing its job.
    #[test]
    fn paired_emphasis_is_still_dropped_and_backticks_are_still_kept() {
        assert_eq!(inline("a **bold** word"), "a bold word");
        assert_eq!(inline("an __underlined__ word"), "an underlined word");
        // A backtick is the only mark that survives, deliberately: without it `--json` reads
        // as prose rather than as something to type.
        assert_eq!(inline("pass `--json` to it"), "pass `--json` to it");
        // Single markers were never touched, despite what the old doc comment claimed.
        assert_eq!(inline("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(inline("snake_case_name"), "snake_case_name");
    }
    use super::*;

    /// Bug this prevents: a sparse patch that serialises its unset fields. `{"state":""}` on
    /// `PATCH /issues/{index}` **reopens a closed issue**, and `{"body":""}` erases the body.
    #[test]
    fn a_patch_body_contains_only_the_fields_that_were_set() {
        let patch = IssuePatch { title: Some("new".to_owned()), ..IssuePatch::default() };
        assert_eq!(serde_json::to_string(&patch).unwrap(), r#"{"title":"new"}"#);

        let close = IssuePatch { state: Some("closed".to_owned()), ..IssuePatch::default() };
        assert_eq!(serde_json::to_string(&close).unwrap(), r#"{"state":"closed"}"#);

        // The same property for the other three resources, because the failure mode differs:
        // a release's `draft: false` publishes it.
        assert_eq!(
            serde_json::to_string(&ReleasePatch {
                body: Some("notes".to_owned()),
                ..ReleasePatch::default()
            })
            .unwrap(),
            r#"{"body":"notes"}"#
        );
        assert_eq!(
            serde_json::to_string(&LabelPatch {
                color: Some("e11d21".to_owned()),
                ..LabelPatch::default()
            })
            .unwrap(),
            r#"{"color":"e11d21"}"#
        );
        assert_eq!(
            serde_json::to_string(&MilestonePatch {
                state: Some("closed".to_owned()),
                ..MilestonePatch::default()
            })
            .unwrap(),
            r#"{"state":"closed"}"#
        );
    }

    /// The hazard this module was written around, now fixed in the emitter: an unset
    /// request-body field is omitted rather than sent as its zero value, so a default
    /// `EditIssueOption` is an empty JSON object and not an instruction to clear the body,
    /// unset the milestone, unassign everybody and reopen the issue.
    ///
    /// Kept as a regression test. The patch structs above stay for now because they are also
    /// how this module distinguishes "clear this field" from "leave it alone".
    #[test]
    fn the_generated_edit_option_sends_nothing_it_was_not_given() {
        let sent = serde_json::to_string(&gitea_model::EditIssueOption::default()).unwrap();
        assert_eq!(sent, "{}", "an unset request-body field must not reach the wire");

        let one = serde_json::to_string(&gitea_model::EditIssueOption {
            state: Some("closed".to_owned()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(one, r#"{"state":"closed"}"#);
    }

    #[test]
    fn a_body_file_of_dash_reads_stdin() {
        let flags = BodyFlags { body_file: Some("-"), ..BodyFlags::default() };
        let mut stdin: &[u8] = b"from a pipe";
        assert_eq!(flags.read(&mut stdin).unwrap().as_deref(), Some("from a pipe"));
    }

    #[test]
    fn body_and_body_file_together_are_a_usage_error() {
        let flags = BodyFlags { body: Some("x"), body_file: Some("y"), editor: false };
        let e = flags.read(&mut io::empty()).unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn label_colours_are_parsed_with_or_without_a_hash_and_never_panic() {
        assert_eq!(rgb("#e11d21"), Some((0xe1, 0x1d, 0x21)));
        assert_eq!(rgb("e11d21"), Some((0xe1, 0x1d, 0x21)));
        assert_eq!(rgb("red"), None);
        assert_eq!(rgb(""), None);
        assert_eq!(rgb("#12345"), None);
    }

    /// A piped body must be byte-identical to what the server holds: a script doing
    /// `gea issue view 1 --json body --jq .body > file` and one doing `gea issue view 1`
    /// should not disagree about the text.
    #[test]
    fn markdown_is_untouched_when_output_is_not_a_terminal() {
        let src = "# Title\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n";
        assert_eq!(markdown(&Term::piped(), src), src);
        let rendered = markdown(&Term::tty(80), src);
        assert!(rendered.contains("Title"), "{rendered}");
        assert!(rendered.contains("• one"), "{rendered}");
        assert!(!rendered.contains("```"), "{rendered}");
    }

    #[test]
    fn timeago_and_date_report_an_unset_timestamp_as_a_dash() {
        assert_eq!(timeago(None), "—");
        assert_eq!(timeago(Some(Timestamp::default())), "—");
        assert_eq!(date(None), "—");
    }

    /// Bug this prevents: a column and `--template '{{timeago .updated_at}}'` printing
    /// different words for the same field of the same response.
    ///
    /// This file used to carry its own buckets, and they drifted exactly as the doc comment
    /// predicted: `about 3 minutes ago` against `3 minutes ago`, `just now` against `less than
    /// a minute ago`, and no future case at all, so a server clock a minute ahead rendered as
    /// `just now` here and `in 1 minute` there. The two agreed only on the hour buckets — which
    /// is the single range the `issue` snapshots happen to exercise, which is why nothing caught
    /// it. The offsets below are chosen to sit in the ranges those snapshots do not reach.
    #[test]
    fn the_column_and_the_template_helper_render_the_same_words() {
        for offset_secs in [0_i64, 30, 60, 180, 2_700, 5_400, 90_000, 3_000_000, -60, -3_600] {
            let then = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(offset_secs);
            let column = timeago(Some(Timestamp::from_jiff(then)));
            let template = crate::output::template::funcs::timeago(&then.to_string());
            assert_eq!(column, template, "offset {offset_secs}s");
        }
    }
}

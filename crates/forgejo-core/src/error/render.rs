//! The three-part diagnostic renderer.
//!
//! Every error this tool prints has the same shape, and the shape is the contract:
//!
//! 1. **What happened** — one plain line, lowercase, no jargon.
//! 2. **The relevant facts** — indented `key: value`, only facts that bear on *this* failure.
//! 3. **What to do** — imperative bullets, at least one of which is a literal runnable command.
//!
//! Part 3 is the one that matters and the one every CLI skips. An error that says "403 Forbidden"
//! is a transcription of the wire; an error that says "Forgejo token scopes are fixed at creation
//! — create a NEW token that includes `write:issue` at the instance's token page, then
//! `fjo auth login`" is the answer. A test at the bottom of this file iterates **every**
//! [`ErrorKind`] variant and asserts its rendering contains a "what to do" section with a
//! runnable command, so a new variant physically cannot ship without a remedy.
//!
//! Two rules keep the voice consistent:
//!
//! - **Never blame the user and never blame "the request".** Name the thing that is wrong.
//! - **Never invent certainty.** A 404 is genuinely ambiguous and the message says so, listing
//!   all three real causes, because confidently sending someone to debug the wrong one costs more
//!   than admitting the ambiguity.

use std::fmt::Write as _;

use anstyle::{AnsiColor, Style};

use super::{
    AgitRemedy, Attempt, CallbackFailure, CredentialKind, Error, ErrorKind, FailedCheck,
    FieldError, KeyringCause, Phase, RemoteCandidate, RequestCtx, TokenSource,
};

/// Whether to emit ANSI styling.
///
/// TTY detection is deliberately **not** done here: `std::io::IsTerminal` on stderr is the
/// binary's business, the renderer is called from tests and from `--json` error paths where the
/// answer differs, and a library that reaches for the terminal state cannot be tested for both
/// outcomes. Callers pass the answer in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Never,
    Always,
}

impl Color {
    /// The usual policy: colour only on a terminal, and never when `NO_COLOR` is set.
    /// `CLICOLOR_FORCE` overrides both, per <https://bixense.com/clicolors/>.
    pub fn from_tty(tty: bool) -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
        if set("CLICOLOR_FORCE") {
            return Color::Always;
        }
        if !tty || set("NO_COLOR") {
            return Color::Never;
        }
        Color::Always
    }

    fn paint(self, s: &str, style: Style) -> String {
        match self {
            Color::Never => s.to_owned(),
            Color::Always => format!("{}{s}{}", style.render(), style.render_reset()),
        }
    }
}

fn bold_red() -> Style {
    Style::new().bold().fg_color(Some(AnsiColor::Red.into()))
}

fn bold() -> Style {
    Style::new().bold()
}

fn command_style() -> Style {
    Style::new().fg_color(Some(AnsiColor::Cyan.into()))
}

fn url_style() -> Style {
    Style::new().underline()
}

/// One piece of an advice line, so a command inside a sentence can be styled without the caller
/// hand-assembling escape codes (and without a markup mini-language that would have to be
/// escaped).
enum Frag {
    Plain(String),
    /// A literal runnable command. Rendered cyan.
    Cmd(String),
    /// Rendered underlined.
    Url(String),
}

/// One line of the "what to do" section.
struct Line {
    frags: Vec<Frag>,
    /// Extra indent, for continuation lines under a numbered bullet.
    hang: usize,
}

impl Line {
    fn text(s: impl Into<String>) -> Self {
        Self { frags: vec![Frag::Plain(s.into())], hang: 0 }
    }

    /// A numbered bullet whose body is a command: `1. fjo auth login --host x`.
    fn step(n: usize, cmd: impl Into<String>) -> Self {
        Self { frags: vec![Frag::Plain(format!("{n}. ")), Frag::Cmd(cmd.into())], hang: 0 }
    }

    fn note(s: impl Into<String>) -> Self {
        Self { frags: vec![Frag::Plain(format!("note: {}", s.into()))], hang: 6 }
    }

    fn and_text(mut self, s: impl Into<String>) -> Self {
        self.frags.push(Frag::Plain(s.into()));
        self
    }

    fn and_cmd(mut self, s: impl Into<String>) -> Self {
        self.frags.push(Frag::Cmd(s.into()));
        self
    }

    fn and_url(mut self, s: impl Into<String>) -> Self {
        self.frags.push(Frag::Url(s.into()));
        self
    }

    fn hang(mut self, n: usize) -> Self {
        self.hang = n;
        self
    }

    /// Whether this line contains a literal runnable command. The all-variants test asserts at
    /// least one line in every rendering does; nothing in the rendering path needs to ask.
    #[cfg(test)]
    fn has_command(&self) -> bool {
        self.frags.iter().any(|f| matches!(f, Frag::Cmd(_)))
    }

    fn render(&self, color: Color) -> String {
        let mut out = String::new();
        for frag in &self.frags {
            match frag {
                Frag::Plain(s) => out.push_str(s),
                Frag::Cmd(s) => out.push_str(&color.paint(s, command_style())),
                Frag::Url(s) => out.push_str(&color.paint(s, url_style())),
            }
        }
        out
    }

    /// The physical lines this logical line occupies.
    ///
    /// Prose is soft-wrapped at [`WRAP`]; anything containing a command or a URL is not.
    /// Wrapping a styled fragment would have to count escape bytes as zero-width, and wrapping a
    /// command would break something the reader is meant to copy and paste — so lines that carry
    /// one are written to fit by hand instead.
    fn lines(&self, color: Color) -> Vec<String> {
        let prose = self.frags.iter().all(|f| matches!(f, Frag::Plain(_)));
        let text = self.render(color);
        if !prose {
            return text.split('\n').map(str::to_owned).collect();
        }
        let width = WRAP.saturating_sub(2 + self.hang);
        text.split('\n').flat_map(|part| wrap(part, width)).collect()
    }
}

/// Total line width, including the two-space indent.
///
/// 80 is the width every terminal is at least as wide as. Prose is wrapped here rather than left
/// to the terminal because a terminal's own wrap ignores the hanging indent, so a long note
/// reflows into the left margin and stops looking like a note.
const WRAP: usize = 80;

/// Greedy word wrap. A word longer than `width` (a URL, a long path) gets its own line rather
/// than being broken, because breaking it would make it unusable.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if s.chars().count() <= width {
        return vec![s.to_owned()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// The facts and the remedy for one error.
struct Advice {
    facts: Vec<(String, String)>,
    todo: Vec<Line>,
}

impl Advice {
    fn new() -> Self {
        Self { facts: Vec::new(), todo: Vec::new() }
    }

    fn fact(mut self, key: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.trim().is_empty() {
            self.facts.push((key.to_owned(), value));
        }
        self
    }

    /// A fact with no key, aligned under the previous one. Used for lists (repeated `tried:`
    /// lines, a caret under an expression).
    fn cont(mut self, value: impl Into<String>) -> Self {
        self.facts.push((String::new(), value.into()));
        self
    }

    fn todo(mut self, line: Line) -> Self {
        self.todo.push(line);
        self
    }
}

/// The one-line summary: lowercase, no jargon, no punctuation at the end.
///
/// Used as `Display for Error` and as the first line of [`render`]. Kept separate so that
/// `--json` error output and log lines can have the summary without the essay.
pub fn headline(kind: &ErrorKind) -> String {
    use ErrorKind::*;
    match kind {
        Dns { host } => format!("cannot resolve hostname {host}"),
        Connect { host, .. } => format!("cannot connect to {host}"),
        Tls { host, .. } => format!("TLS connection to {host} failed"),
        Timeout { host, .. } => format!("timed out waiting for {host}"),
        Proxy { proxy, .. } => format!("proxy {proxy} rejected the request"),

        NoHostConfigured => "no Forgejo host configured".to_owned(),
        UnknownHost { given, .. } => format!("host {given} is not configured"),
        NotAuthenticated { host } => format!("you are not logged in to {host}"),
        WebSessionMissing { host } => format!("no web session for {host}"),
        WebSessionExpired { host } => format!("your web session for {host} has expired"),
        WebLoginFailed { host, .. } => format!("{host} rejected the sign-in"),
        WebAuthnRequired { host } => {
            format!("the account on {host} requires WebAuthn, which needs a browser")
        }
        TokenRejected { host, .. } => format!("{host} refused your token (HTTP 401)"),
        InsufficientScope { .. } => "token is missing a required scope (HTTP 403)".to_owned(),
        TwoFactorRequired { host } => format!("{host} requires a two-factor code"),
        KeyringUnavailable { .. } => "OS keyring is unavailable".to_owned(),
        CredFilePermissions { .. } => "your credentials file can be read by other users".to_owned(),

        OauthNotSupported { host, .. } => format!("{host} does not offer OAuth login"),
        OauthAuthorizationDenied { host, .. } => format!("{host} did not authorize fjo"),
        OauthStateMismatch { host } => {
            format!("the OAuth reply from {host} did not match the request")
        }
        OauthCallbackUnavailable { reason, .. } => match reason {
            CallbackFailure::Timeout(secs) => {
                format!("nothing came back from your browser within {secs}s")
            }
            CallbackFailure::Bind(_) => "fjo could not listen for the OAuth reply".to_owned(),
        },
        OauthTokenExchangeFailed { host, .. } => format!("{host} refused the OAuth code"),
        OauthRefreshFailed { host, .. } => format!("your OAuth session for {host} has expired"),
        OauthEntropyUnavailable { .. } => {
            "could not generate a secure random value for the login".to_owned()
        }

        NotAGitRepo => "not in a Git checkout; specify a repository with -R".to_owned(),
        RepoNotResolved { .. } => "could not determine the repository".to_owned(),
        AmbiguousRemote { .. } => "multiple Git remotes match; select a repository".to_owned(),
        RemoteHostUnknown { remote, host } => {
            format!("remote {remote} uses unconfigured host {host}")
        }

        // Two different claims, and only one of them is ever supported by evidence. `probed`
        // means `GET /repos/{owner}/{repo}` was run and 404'd, so the repository is genuinely
        // unreachable. Without it nobody asked, and saying the repository could not be found
        // would be inventing the result of a request that was never sent — visibly so once
        // `server says:` prints a sentence about a branch two lines below.
        RepoNotFound { slug, host, probed: true, .. } => {
            format!("could not find the repository {slug} on {host} (HTTP 404)")
        }
        RepoNotFound { slug, host, .. } => {
            format!("repository or resource not found under {slug} on {host} (HTTP 404)")
        }
        // An empty id is the *collection* — `POST /repos/o/r/pulls`. Nothing in the path was
        // named, so nothing in the path can be missing; what 404'd is something the request
        // referred to, and only the server's message knows which. Saying "that pull request does
        // not exist" here would be describing the object the user was trying to create.
        ResourceNotFound { kind, id, slug, .. } => {
            if id.is_empty() {
                match slug {
                    Some(s) => {
                        format!("referenced resource not found in {s} (HTTP 404)")
                    }
                    None => "referenced resource not found (HTTP 404)".to_owned(),
                }
            } else {
                format!("there is no {kind} {id} (HTTP 404)")
            }
        }
        RouteNotFound { .. } => "API endpoint not found (HTTP 404)".to_owned(),
        Conflict { .. } => "request conflicts with the current state (HTTP 409)".to_owned(),
        Validation { .. } => "the server rejected the values in this request (HTTP 422)".to_owned(),
        QuotaExceeded { .. } => "the storage quota is full (HTTP 413)".to_owned(),
        Archived { slug, .. } => {
            format!("{slug} is archived and read-only (HTTP 423)")
        }
        StateConflict { resource, state, .. } => {
            let what = resource.clone().unwrap_or_else(|| "that resource".to_owned());
            match state.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(s) => format!("operation unavailable: {what} is {s}"),
                None => format!("operation unavailable in the current state of {what}"),
            }
        }
        ChecksPending { pr, .. } => {
            format!("checks are pending on pull request {pr}")
        }
        ChecksFailed { pr, failed, .. } => match failed.len() {
            1 => format!("a check on pull request {pr} failed"),
            0 => format!("the checks on pull request {pr} did not pass"),
            n => format!("{n} checks on pull request {pr} failed"),
        },
        RunFailed { run, conclusion, .. } => match conclusion.trim() {
            "" => format!("workflow run {run} did not succeed"),
            c => format!("workflow run {run} ended in {c}"),
        },
        PaginationDidNotTerminate { pages, .. } => {
            format!("pagination stopped after {pages} pages without reaching the end")
        }
        RateLimited { host, .. } => format!("rate limit reached on {host} (HTTP 429)"),
        Forbidden { .. } => "permission denied (HTTP 403)".to_owned(),
        ServerError { status, .. } => format!("server error (HTTP {status})"),
        UnexpectedStatus { status, .. } => {
            format!("unexpected HTTP status {status}")
        }

        Decode { .. } => "unexpected API response format".to_owned(),

        Usage(msg) => msg.clone(),
        UnknownJsonField { given, .. } => {
            format!("unknown --json field: {given}")
        }
        JqCompile { .. } => "invalid --jq expression".to_owned(),
        Template { .. } => "invalid --template syntax".to_owned(),
        PathNotFound { path, what } => {
            format!("there is no {what} at {}", path.display())
        }
        GitFailed { command, .. } => {
            format!("Git command failed: {}", first_words(command))
        }
        AgitRefused { refspec, .. } => format!("the AGit push to {refspec} was refused"),
        Io(e) => format!("a local file operation failed: {e}"),
        Cancelled => "cancelled".to_owned(),
    }
}

/// The first few words of a command, so a headline naming it stays one readable line. The whole
/// command is in the facts block directly underneath, so nothing is lost by clipping here.
fn first_words(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().take(3).collect();
    if command.split_whitespace().count() > 3 {
        format!("{} …", words.join(" "))
    } else {
        words.join(" ")
    }
}

/// The full three-part diagnostic.
pub fn render(err: &Error, color: Color) -> String {
    let advice = advise(&err.kind, &err.ctx);

    let mut facts: Vec<(String, String)> = Vec::new();
    if let Some(request) = request_line(&err.ctx) {
        facts.push(("request".to_owned(), request));
    }
    facts.extend(advice.facts);
    if let Some(source) = token_source_line(&err.ctx) {
        // Placed last, because it is context for the *credential* rather than for the failure,
        // and only shown for failures where the credential is implicated.
        if credential_relevant(&err.kind) {
            facts.push(("token from".to_owned(), source));
        }
    }

    let mut out = String::new();
    let _ = write!(out, "{} {}", color.paint("error:", bold_red()), headline(&err.kind));

    if !facts.is_empty() {
        out.push_str("\n\n");
        // Align values one column past the longest key, so the block scans vertically.
        let width = facts.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 1;
        for (key, value) in &facts {
            let label = if key.is_empty() { String::new() } else { format!("{key}:") };
            let _ = writeln!(out, "  {label:<width$} {value}");
        }
        // Trim the trailing newline; the "what to do" block adds its own separator.
        while out.ends_with('\n') {
            out.pop();
        }
    }

    if !advice.todo.is_empty() {
        out.push_str("\n\n");
        let _ = writeln!(out, "{}", color.paint("what to do:", bold()));
        for line in &advice.todo {
            let pad = " ".repeat(2 + line.hang);
            for (i, part) in line.lines(color).into_iter().enumerate() {
                if i == 0 {
                    let _ = writeln!(out, "  {part}");
                } else {
                    let _ = writeln!(out, "{pad}{part}");
                }
            }
        }
        while out.ends_with('\n') {
            out.pop();
        }
    }

    out
}

/// `POST /api/v1/repos/perf3ct/fjo/issues`
/// The server's own words for an OAuth failure, preferring `error_description` over the bare
/// `error` code.
///
/// Forgejo's OAuth handlers return both, and the description is the one written for a human —
/// "PKCE is required for public clients" says what to fix, where `invalid_request` does not.
/// Neither is ever paraphrased: `docs/porcelain-conventions.md` is explicit that a server
/// message is never swallowed, and a guess about someone else's error is worse than a quote.
fn oauth_reason(error: &str, description: &Option<String>) -> String {
    match description {
        Some(d) if !d.trim().is_empty() => d.clone(),
        _ => error.to_owned(),
    }
}

fn request_line(ctx: &RequestCtx) -> Option<String> {
    let path = ctx.path.as_deref()?;
    Some(match &ctx.method {
        Some(m) => format!("{m} {path}"),
        None => path.to_owned(),
    })
}

/// Where the credential came from. This is the difference between "your token was rejected" and
/// "the token in your keyring under `fjo:git.example.org` was rejected" — the second tells the
/// user which of their three credentials to go fix.
fn token_source_line(ctx: &RequestCtx) -> Option<String> {
    Some(match ctx.token_source.as_ref()? {
        TokenSource::Keyring { entry } => format!("the OS keyring, entry {entry}"),
        TokenSource::File { path } => format!("the file {}", path.display()),
        TokenSource::Env { var } => format!("the environment variable {var}"),
        TokenSource::Flag => "a command-line flag".to_owned(),
    })
}

fn credential_relevant(kind: &ErrorKind) -> bool {
    use ErrorKind::*;
    matches!(
        kind,
        TokenRejected { .. }
            | InsufficientScope { .. }
            | NotAuthenticated { .. }
            | TwoFactorRequired { .. }
            | Forbidden { .. }
            | RepoNotFound { .. }
            | OauthStateMismatch { .. }
            | OauthCallbackUnavailable { .. }
            | OauthTokenExchangeFailed { .. }
            | OauthRefreshFailed { .. }
    )
}

/// The `fjo api …` command equivalent to the request that failed. Runnable, and useful in its
/// own right: layer 1 always works even when a generated model or a porcelain command does not.
fn api_command(ctx: &RequestCtx) -> String {
    let Some(path) = ctx.path.as_deref() else {
        return "fjo auth status".to_owned();
    };
    let rel = path.split_once("/api/v1/").map_or(path, |(_, r)| r).trim_start_matches('/');
    match ctx.method.as_deref() {
        Some("GET") | None => format!("fjo api {rel}"),
        Some(m) => format!("fjo api -X {m} {rel}"),
    }
}

/// A best-effort web URL for this instance, for "do it in the UI" advice.
fn web_base(ctx: &RequestCtx) -> String {
    match ctx.host.as_deref() {
        Some(h) => format!("https://{h}"),
        None => "https://your-instance".to_owned(),
    }
}

fn settings_hint(ctx: &RequestCtx) -> String {
    format!("{}/user/settings/applications", web_base(ctx))
}

fn quoted(server_message: &str) -> String {
    if server_message.trim().is_empty() {
        "(the server sent no message)".to_owned()
    } else {
        server_message.trim().to_owned()
    }
}

fn join_scopes(v: &[String]) -> String {
    v.join(", ")
}

/// The scope list, or an honest placeholder.
///
/// `classify` fills this from the server's own message, then from `OpMeta::scope`, then from
/// inference, so in practice it is always populated — but a caller that constructs
/// `InsufficientScope` by hand may not, and "create a NEW token that includes  at" is worse than
/// saying we do not know.
fn scopes_or_unknown(v: &[String]) -> String {
    if v.is_empty() {
        "the scope for this route (fjo could not determine it)".to_owned()
    } else {
        join_scopes(v)
    }
}

fn attempts(mut a: Advice, tried: &[Attempt]) -> Advice {
    for (i, attempt) in tried.iter().enumerate() {
        let text = format!("{} — {}", attempt.what, attempt.outcome);
        a = if i == 0 { a.fact("tried", text) } else { a.cont(text) };
    }
    a
}

fn fields_facts(mut a: Advice, fields: &[FieldError]) -> Advice {
    for f in fields {
        a = match &f.field {
            Some(name) => a.fact(name, &f.message),
            None => a.fact("problem", &f.message),
        };
    }
    a
}

/// A porcelain command that lists things of this kind, for a `ResourceNotFound` remedy.
fn list_command(kind: &str, slug: Option<&str>) -> String {
    let scope = slug.map(|s| format!(" -R {s}")).unwrap_or_default();
    match kind {
        "pull request" => format!("fjo pr list --state all{scope}"),
        "issue" => format!("fjo issue list --state all{scope}"),
        "release" => format!("fjo release list{scope}"),
        "label" => format!("fjo label list{scope}"),
        "milestone" => format!("fjo milestone list{scope}"),
        "workflow run" => format!("fjo run list{scope}"),
        "team" => "fjo team list".to_owned(),
        "user" => "fjo api users/search --jq '.data[].login'".to_owned(),
        "organization" => "fjo org list".to_owned(),
        _ => match slug {
            Some(s) => format!("fjo api repos/{s}"),
            None => "fjo auth status".to_owned(),
        },
    }
}

/// The per-variant facts and remedy. Exhaustive over [`ErrorKind`] on purpose — see the test.
fn advise(kind: &ErrorKind, ctx: &RequestCtx) -> Advice {
    use ErrorKind::*;
    let a = Advice::new();
    match kind {
        // ------------------------------------------------------------------- transport
        Dns { host } => a
            .fact("host", host)
            .todo(Line::text("check the hostname and your DNS connection."))
            .todo(Line::text("1. check the hostname and port"))
            .todo(Line::step(2, "fjo auth status").and_text("  (configured hosts)"))
            .todo(
                Line::text(
                    "3. if the instance is only reachable on a private network, connect to it \
                     first",
                )
                .hang(3),
            ),

        Connect { host, port, cause, looks_like_plaintext, scheme } => {
            let a = a
                .fact("host", host)
                .fact("port", if *port == 0 { String::new() } else { port.to_string() })
                .fact("cause", cause);
            let with_port =
                if *port == 0 { host.clone() } else { format!("{host}:{port}") };
            if *looks_like_plaintext {
                // The port answered — it just answered in HTTP. Telling the reader to check the
                // port here would send them to change the one thing that is already correct.
                a.todo(Line::text(
                    "the server replied with HTTP, but fjo used HTTPS.",
                ))
                .todo(Line::text("1. use an explicit HTTP address:").hang(3))
                .todo(Line::text("   ").and_cmd(format!("fjo auth login --host http://{with_port}")))
                .todo(Line::text("2. check the server response:").hang(3))
                .todo(Line::text("   ").and_cmd(format!(
                    "curl -sS -o /dev/null -w '%{{http_code}}\\n' http://{with_port}/api/v1/version"
                )))
                .todo(Line::note(
                    "non-loopback hosts default to HTTPS. Use http:// only if the server requires it.",
                ))
            } else {
                a.todo(Line::text(
                    "the hostname resolved, but the connection failed.",
                ))
                // Both commands repeat the scheme the request used rather than asserting
                // https. Asserting it was wrong twice over for a plain-HTTP instance: the curl
                // went to a URL the server does not serve, and the login suggestion dropped the
                // scheme entirely — and a bare host makes `scheme_for` guess https for anything
                // non-loopback, so taking the advice moved the reader from this branch into the
                // plaintext one above.
                // Step 1 curls the address fjo actually used, port and all, rather than a
                // guessed default. It answers the question the reader has — "is it really
                // refused, or is this fjo?" — with an independent tool, and a definitive
                // answer about the right address beats a hint about a different one.
                .todo(Line::text("1. test the same address with curl:").hang(3))
                .todo(Line::text("   ").and_cmd(format!(
                    "curl -sS -o /dev/null -w '%{{http_code}}\\n' {scheme}://{with_port}/api/v1/version"
                )))
                .todo(
                    Line::text("2. if the port is wrong, log in with the correct port (example: 3000):")
                        .hang(3),
                )
                .todo(
                    Line::text("   ")
                        .and_cmd(format!("fjo auth login --host {scheme}://{host}:3000")),
                )
                .todo(Line::note("check whether a firewall or VPN is blocking the connection."))
            }
        }

        Tls { host, cause, looks_like_private_ca } => {
            let a = a.fact("host", host).fact("cause", cause);
            if *looks_like_private_ca {
                a.todo(Line::text(
                    "this machine does not trust the server's certificate authority (CA).",
                ))
                .todo(Line::text("1. add the CA certificate to the system trust store, then retry"))
                .todo(
                    Line::text("2. or set the CA file for one command: ")
                        .and_cmd("SSL_CERT_FILE=/path/to/ca.pem fjo auth status")
                        .hang(3),
                )
                .todo(Line::note(
                    "use a trusted CA certificate. Disabling verification leaves the connection unverified.",
                ))
            } else {
                a.todo(Line::text(
                    "check for an expired certificate or a hostname mismatch.",
                ))
                .todo(Line::text("1. inspect the server certificate:").hang(3))
                .todo(Line::text("   ").and_cmd(format!(
                    "openssl s_client -connect {host}:443 -servername {host} </dev/null | head -20"
                )))
                .todo(
                    Line::text(
                        "2. ask the server administrator to renew or correct the certificate",
                    )
                    .hang(3),
                )
            }
        }

        Timeout { host, after, phase } => a
            .fact("host", host)
            .fact(
                "phase",
                match phase {
                    Phase::Connect => "opening the connection",
                    Phase::Headers => "waiting for a response",
                    Phase::Body => "reading the response body",
                },
            )
            .fact("waited", if after.is_zero() { String::new() } else { format!("{:.1?}", after) })
            .todo(Line::text("the request timed out during the phase shown above."))
            .todo(Line::step(1, "fjo api version").and_text("  (check server availability)"))
            .todo(
                Line::text(
                    "2. check the server status before retrying a long-running operation",
                )
                .hang(3),
            )
            .todo(Line::note(match phase {
                Phase::Body => "the response had started. Check the result before retrying a write.",
                _ => "a write may have completed despite the timeout. Read-only requests are safe to retry.",
            })),

        Proxy { proxy, cause } => a
            .fact("proxy", proxy)
            .fact("cause", cause)
            .todo(Line::text("check the proxy settings and access requirements."))
            .todo(Line::step(1, "env | grep -i proxy").and_text("  (proxy settings)"))
            .todo(
                Line::text("2. exclude this host from the proxy: ")
                    .and_cmd(format!("NO_PROXY={} fjo auth status", ctx.host.as_deref().unwrap_or("git.example.org")))
                    .hang(3),
            ),

        // -------------------------------------------------------------- config and auth
        NoHostConfigured => a
            .todo(Line::text("log in to your Forgejo server:"))
            .todo(Line::step(1, "fjo auth login --host git.example.org"))
            .todo(Line::note(
                "use the server address without /api/v1.",
            )),

        UnknownHost { given, known } => a
            .fact("asked for", given)
            .fact("configured", if known.is_empty() { "(none)".to_owned() } else { known.join(", ") })
            .todo(Line::step(1, format!("fjo auth login --host {given}")).and_text("  (add it)"))
            .todo(Line::step(2, "fjo auth status").and_text("  (list configured hosts)"))
            .todo(Line::note(
                "git.example.org and www.git.example.org are separate hosts.",
            )),

        NotAuthenticated { host } => a
            .fact("host", host)
            .todo(Line::text("this endpoint requires authentication."))
            .todo(
                Line::text("1. create a token at ")
                    .and_url(settings_hint(ctx))
                    .hang(3),
            )
            .todo(Line::step(2, format!("fjo auth login --host {host}")))
            .todo(Line::note(
                "in CI, set FJO_TOKEN or FORGEJO_TOKEN instead of logging in.",
            )),

        // ------------------------------------------------------------------ web session
        //
        // Deliberately not folded into NotAuthenticated. That error's whole remedy is a token,
        // and a token cannot reach these routes at all — Forgejo answers one with the same 303
        // to /user/login that it gives an anonymous request. Sending someone to create a token
        // would be sending them to do work that cannot possibly help.
        WebSessionMissing { host } => a
            .fact("host", host)
            .todo(Line::text(
                "this is a web-only route, which needs a signed-in session rather than a token.",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host} --with-password")))
            .todo(Line::note(
                "in CI, set FJO_WEB_SESSION to a session document instead of logging in.",
            )),

        WebSessionExpired { host } => a
            .fact("host", host)
            .todo(Line::text(
                "the sign-in that renews it has lapsed, been revoked, or followed a password \
                 change.",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host} --with-password")))
            .todo(Line::note(
                "web sessions are renewed automatically; this means the remember token itself \
                 is gone.",
            )),

        WebLoginFailed { host, reason } => {
            let a = a.fact("host", host);
            // The server's own words when it gave any, because Forgejo distinguishes a wrong
            // password from a disabled account from a login source that forbids passwords, and
            // re-deriving that here would be guessing at which one happened.
            let a = match reason {
                Some(r) => a.fact("server said", r),
                None => a,
            };
            a.todo(Line::text("check the username and password, then try again."))
                .todo(Line::step(1, format!("fjo auth login --host {host} --with-password")))
                .todo(Line::note(
                    "a token is not a password here; this route wants the one you type into the \
                     web UI.",
                ))
        }

        WebAuthnRequired { host } => a
            .fact("host", host)
            .todo(Line::text(
                "WebAuthn cannot be completed without a browser, so this account cannot sign in \
                 from the command line. Enrol TOTP alongside it, then:",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host} --with-password --otp 123456")))
            .todo(Line::note(
                "or use a dedicated account for automation, with TOTP or no second factor.",
            ))
            .todo(Line::note(format!("both are configured at {host}/user/settings/security"))),

        // The same 401, from the same place, with two different remedies. Which one depends on
        // what was actually presented, which is why `RequestCtx` carries the credential kind.
        // Sending someone whose OAuth session lapsed to the token settings page is sending them
        // to the wrong screen, and they will do what it says before discovering that.
        TokenRejected { host, login, settings_url } => {
            let a = a.fact("host", host).fact("login", login.clone().unwrap_or_default());
            match ctx.credential_kind {
                Some(CredentialKind::Oauth2) => a
                    .todo(Line::text(
                        "your OAuth session has expired or been revoked.",
                    ))
                    .todo(Line::step(1, format!("fjo auth login --host {host} --web")))
                    .todo(Line::step(2, format!("fjo auth login --host {host}")))
                    .todo(Line::note(
                        "OAuth sessions lapse after about 30 days; a token made in the web UI does not.",
                    )),
                _ => a
                    .todo(Line::text(
                        "the token may be expired, revoked, or from another server.",
                    ))
                    .todo(
                        Line::text("1. create a new token at ")
                            .and_url(settings_url.clone())
                            .hang(3),
                    )
                    .todo(Line::step(2, format!("fjo auth login --host {host}")))
                    .todo(Line::note(
                        "replacing a token requires creating a new one.",
                    )),
            }
        }

        OauthNotSupported { host, tried } => a
            .fact("host", host)
            .fact("tried", tried.join(", "))
            .todo(Line::text(
                "this instance answered nothing at its OAuth2 endpoints.",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host}")))
            .todo(Line::step(2, "fjo api version"))
            .todo(Line::note(
                "an administrator can switch the built-in OAuth applications off with [oauth2] DEFAULT_APPLICATIONS.",
            )),

        OauthAuthorizationDenied { host, error, description } => a
            .fact("reason", oauth_reason(error, description))
            .fact("code", error)
            .todo(Line::text("the browser came back without authorizing fjo."))
            .todo(Line::step(1, format!("fjo auth login --host {host} --web")))
            .todo(Line::step(2, format!("fjo auth login --host {host}")))
            .todo(Line::note(
                "access_denied usually means the Authorize button was not clicked.",
            )),

        OauthStateMismatch { host } => a
            .fact("host", host)
            .todo(Line::text(
                "the reply did not carry the value fjo sent, so no code was exchanged.",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host} --web")))
            .todo(Line::note(
                "this can mean two logins were running at once; run one at a time.",
            )),

        OauthCallbackUnavailable { host, port, reason } => {
            let a = match reason {
                CallbackFailure::Timeout(_) => a
                    .fact(
                        "listening on",
                        port.map_or_else(
                            || "127.0.0.1".to_owned(),
                            |p| format!("http://127.0.0.1:{p}"),
                        ),
                    )
                    .todo(Line::text("the browser never reached fjo.")),
                CallbackFailure::Bind(cause) => {
                    a.fact("cause", cause).todo(Line::text(
                        "fjo could not open a socket on 127.0.0.1 to receive the reply.",
                    ))
                }
            };
            a.todo(Line::step(
                1,
                format!("fjo auth login --host {host} --web --no-browser"),
            ))
            .todo(Line::step(2, format!("fjo auth login --host {host} --with-token")))
            .todo(Line::note(
                "over SSH, forward the port with ssh -L, or use --no-browser and paste the reply back.",
            ))
        }

        OauthTokenExchangeFailed { host, error, description } => a
            .fact("reason", oauth_reason(error, description))
            .fact("code", error)
            .todo(Line::text("the authorization code was refused."))
            .todo(Line::step(1, format!("fjo auth login --host {host} --web")))
            .todo(Line::step(2, format!("fjo auth login --host {host}")))
            .todo(Line::note(
                "if this instance registers its own application, pass its --client-id.",
            )),

        OauthRefreshFailed { host, login, reason } => a
            .fact("login", login)
            .fact(
                "reason",
                reason.clone().unwrap_or_else(|| "the refresh token was not accepted".to_owned()),
            )
            .todo(Line::text(
                "OAuth sessions last about 30 days and cannot be renewed once they lapse.",
            ))
            .todo(Line::step(1, format!("fjo auth login --host {host} --web")))
            .todo(Line::step(2, format!("fjo auth login --host {host}")))
            .todo(Line::note(
                "a token created in the web UI never expires, which is what CI should use.",
            )),

        OauthEntropyUnavailable { cause } => a
            .fact("cause", cause)
            .todo(Line::text(
                "a browser login needs unguessable values, and the OS random source refused.",
            ))
            .todo(Line::step(1, "fjo auth login --with-token"))
            .todo(Line::note(
                "this usually means a restricted sandbox or a seccomp filter blocking getrandom.",
            )),

        // Reproduces the worked example in the plan, because the wording is the design.
        InsufficientScope { host, needed, have, settings_url } => a
            .fact("needs", scopes_or_unknown(needed))
            .fact(
                "token has",
                match have {
                    Some(h) if !h.is_empty() => join_scopes(h),
                    // Honest beats guessing: Forgejo genuinely does not report a token's scopes.
                    _ => "unknown (Forgejo does not report a token's scopes)".to_owned(),
                },
            )
            .todo(Line::text(
                "create a token with the required scopes; existing token scopes cannot be changed.",
            ))
            .todo(
                Line::text(format!(
                    "1. create a token with {} at",
                    scopes_or_unknown(needed)
                ))
                .hang(3),
            )
            .todo(Line::text("   ").and_url(settings_url.clone()))
            .todo(Line::step(2, format!("fjo auth login --host {host}")))
            .todo(Line::note(
                "Forgejo scopes use read:<area> and write:<area>.",
            )),

        TwoFactorRequired { host } => a
            .fact("host", host)
            .todo(Line::text(
                "password authentication requires a two-factor code for this account.",
            ))
            .todo(Line::step(1, "fjo --otp 123456 <command>").and_text("  (the current code)"))
            .todo(Line::step(2, format!("fjo auth login --host {host}")).and_text("  (store a token instead)"))
            .todo(Line::note("API tokens do not require a two-factor code.")),

        KeyringUnavailable { cause } => a
            .fact(
                "cause",
                match cause {
                    KeyringCause::NoBackend => {
                        "no credential backend answered (on Linux, usually no D-Bus session)".to_owned()
                    }
                    KeyringCause::Locked => "the keyring is locked".to_owned(),
                    KeyringCause::Denied => "access was denied".to_owned(),
                    KeyringCause::Timeout => "the keyring did not answer in time".to_owned(),
                    KeyringCause::Other(m) => m.clone(),
                },
            )
            .todo(Line::text(
                "unlock or enable the keyring, or use one of these alternatives:",
            ))
            .todo(
                Line::step(1, "fjo auth login --host git.example.org --insecure-storage")
                    .and_text("  (a 0600 file)")
                    .hang(3),
            )
            .todo(Line::step(2, "FJO_TOKEN=<token> fjo auth status").and_text("  (environment only)"))
            .todo(Line::note(
                "set FJO_CREDENTIAL_STORE to env, file, or keyring to select a storage backend.",
            )),

        CredFilePermissions { path, mode } => a
            .fact("file", path.display().to_string())
            .fact("mode", format!("{mode:04o}"))
            .todo(Line::text("restrict the token file to your account:"))
            .todo(Line::step(1, format!("chmod 600 {}", path.display())))
            .todo(Line::note("fjo will not read the file until its permissions are restricted.")),

        // ------------------------------------------------------------------ git context
        NotAGitRepo => a
            .todo(Line::text(
                "specify a repository or run the command from a Git checkout.",
            ))
            .todo(Line::step(1, "fjo <command> -R owner/name").and_text("  (name it explicitly)"))
            .todo(Line::text("2. or cd into a clone and try again"))
            .todo(Line::note("FJO_REPO=owner/name works too, for a shell session or a CI job.")),

        RepoNotResolved { tried } => attempts(a, tried)
            .todo(Line::text("specify a repository or save a default for this checkout."))
            .todo(Line::step(1, "fjo <command> -R owner/name"))
            .todo(Line::step(2, "fjo repo set-default").and_text("  (remember it for this checkout)"))
            .todo(Line::note("-R also accepts host/owner/name and a full URL.")),

        AmbiguousRemote { candidates } => {
            let mut a = a;
            for (i, RemoteCandidate { remote, host, slug }) in candidates.iter().enumerate() {
                let text = format!("{remote} → {host}/{slug}");
                a = if i == 0 { a.fact("remotes", text) } else { a.cont(text) };
            }
            a.todo(Line::text(
                "choose which remote repository to use.",
            ))
            .todo(Line::step(1, "fjo repo set-default").and_text("  (save the choice in Git config)"))
            .todo(Line::step(2, "fjo <command> -R owner/name").and_text("  (choose for one command)"))
        }

        RemoteHostUnknown { remote, host } => a
            .fact("remote", remote)
            .fact("host", host)
            .todo(Line::text("log in to the remote host, or select another repository."))
            .todo(Line::step(1, format!("fjo auth login --host {host}")))
            .todo(Line::step(2, "fjo repo set-default").and_text("  (select a repository explicitly)"))
            .todo(Line::note(
                "SSH Host aliases are not supported. Use fjo repo set-default instead.",
            )),

        // --------------------------------------------------------------- API semantics
        // The probe was never run, so the one thing this rendering must not do is list the three
        // causes of a missing repository as though the repository had been ruled out. The
        // remedy is the check itself: `fjo api repos/{slug}` is literally the request
        // `probe_404` would have made, and running it collapses the ambiguity the same way.
        RepoNotFound { slug, host, login, probed: false, server_message } => a
            .fact("repository", format!("{slug}  (not checked)"))
            .fact("host", host)
            .fact("logged in as", login.clone().unwrap_or_else(|| "(not logged in)".to_owned()))
            .fact("server says", server_message.clone().unwrap_or_default())
            .todo(Line::text(
                "the repository check was disabled or failed. The repository or a referenced resource may be missing.",
            ))
            .todo(Line::step(1, format!("fjo api repos/{slug}")).and_text("  (check repository access)"))
            .todo(Line::text("2. another 404 may mean the repository is missing, private with a token").hang(3))
            .todo(Line::text("   that lacks read:repository, or on another server. Check: ").and_cmd("fjo auth status"))
            .todo(Line::text("3. if it succeeds, check the resources referenced by the request:").hang(3))
            .todo(Line::text("   branch, tag, or user names")),

        // A 404 really is ambiguous. Naming one cause would send most people down the wrong path.
        RepoNotFound { slug, host, login, server_message, .. } => a
            .fact("repository", slug)
            .fact("host", host)
            .fact("logged in as", login.clone().unwrap_or_else(|| "(not logged in)".to_owned()))
            // Present only when the probe was skipped or failed and the body said more than
            // "not found" — but when it is there it usually outranks all three guesses below.
            .fact("server says", server_message.clone().unwrap_or_default())
            .todo(Line::text("check these possible causes:"))
            .todo(
                Line::text("1. check the owner and repository name; the repository may have been")
                    .hang(3),
            )
            .todo(Line::text("   renamed, transferred, or deleted"))
            .todo(Line::text("2. for a private repository, check account access and read:repository.").hang(3))
            .todo(Line::text("   if the scope is missing, create a new token at"))
            .todo(Line::text("   ").and_url(settings_hint(ctx)))
            .todo(Line::text("3. check the configured server: ").and_cmd("fjo auth status").hang(3))
            .todo(Line::text("check repository access: ").and_cmd(format!("fjo api repos/{slug}"))),

        // The collection case: the path ended on `…/pulls`, with no identifier after it. See
        // `headline` above — and note that neither remedy below is the other's with a word
        // changed. "Go and list the pull requests" is the answer when an identifier is wrong and
        // exactly the wrong answer when the user was creating one.
        ResourceNotFound { kind, slug, server_message, id } if id.is_empty() => {
            let a = a
                .fact(
                    "repository",
                    slug.clone().map(|s| format!("{s}  (found)")).unwrap_or_default(),
                )
                .fact("server says", server_message.clone().unwrap_or_default());
            let a = a.todo(Line::text(format!(
                "this request has no {kind} identifier. Check referenced branch, tag, or user names.",
            )));
            let a = match server_message {
                Some(_) => a.todo(
                    Line::text(
                        "1. correct the value named in the server message, then retry",
                    )
                    .hang(3),
                ),
                None => a.todo(
                    Line::text(
                        "1. check branch, tag, and user names in the request body",
                    )
                    .hang(3),
                ),
            };
            match slug {
                Some(s) => a
                    .todo(Line::step(2, format!("fjo api repos/{s}/branches --jq '.[].name'")))
                    .todo(Line::step(3, format!("fjo api repos/{s}/tags --jq '.[].name'")))
                    .todo(Line::note(
                        "when creating a pull request, check --head and --base.",
                    )),
                None => a.todo(
                    Line::step(2, "fjo auth status")
                        .and_text("  (check the selected server)"),
                ),
            }
        }

        ResourceNotFound { kind, id, slug, server_message } => {
            let a = a
                .fact("repository", slug.clone().map(|s| format!("{s}  (found)")).unwrap_or_default())
                .fact(kind, id)
                .fact("server says", server_message.clone().unwrap_or_default());
            a.todo(Line::text(format!(
                "check the {kind} identifier and list the available resources:"
            )))
            .todo(Line::step(1, list_command(kind, slug.as_deref())))
            .todo(Line::note(
                "most issue and pull request paths use the number shown in the web UI, not the database ID.",
            ))
        }

        RouteNotFound { method, path, instance } => a
            // Only when it is not already the `request:` line above; repeating it verbatim makes
            // the reader look for a difference that is not there.
            .fact(
                "endpoint",
                match request_line(ctx) {
                    Some(r) if r == format!("{method} {path}") => String::new(),
                    _ => format!("{method} {path}"),
                },
            )
            .fact(
                "instance",
                instance.clone().unwrap_or_else(|| "unknown (/nodeinfo did not answer)".to_owned()),
            )
            .todo(Line::text(
                "the server does not support this endpoint or HTTP method.",
            ))
            .todo(Line::step(1, "fjo api settings/api").and_text("  (server API settings)"))
            .todo(Line::text("2. try the web interface at ").and_url(web_base(ctx)).hang(3))
            .todo(Line::note(
                "check whether the server version supports this operation.",
            )),

        Conflict { server_message } => a
            .fact("server says", quoted(server_message))
            .todo(Line::text(
                "resolve the conflict described in the server message before retrying.",
            ))
            .todo(Line::step(1, api_command(ctx)).and_text("  (inspect the request response)"))
            .todo(Line::text("2. correct the reported conflict, then retry")),

        Validation { fields, server_message } => {
            let a = fields_facts(a, fields);
            let a = match server_message {
                Some(m) => a.fact("server says", quoted(m)),
                None => a,
            };
            a.todo(Line::text("correct the rejected values listed above."))
                .todo(Line::text("1. correct the values and retry"))
                .todo(Line::step(2, "fjo raw search issue").and_text("  (find the endpoint and its flags)"))
                .todo(Line::note(
                    "the server may require a field that the API specification marks optional.",
                ))
        }

        QuotaExceeded { server_message, uploading } => a
            .fact("uploading", uploading.clone().unwrap_or_default())
            .fact("server says", quoted(server_message))
            .todo(Line::text(
                "check storage usage, including LFS objects, packages, and release assets.",
            ))
            .todo(Line::step(1, "fjo quota").and_text("  (what is using the space)"))
            .todo(
                Line::text(
                    "2. delete old release assets or package versions, or ask an admin to raise \
                     the quota",
                )
                .hang(3),
            )
            .todo(Line::note("check which quota rule applies before removing data.")),

        Archived { slug, host } => a
            .fact("repository", slug)
            .todo(Line::text("archived repositories are read-only."))
            .todo(
                Line::text("1. unarchive it at ")
                    .and_url(format!("https://{host}/{slug}/settings"))
                    .hang(3),
            )
            .todo(Line::text("2. then run the command again"))
            .todo(Line::text("reads still work: ").and_cmd(format!("fjo issue list -R {slug}"))),

        // Forgejo answers a refused operation with 405 as readily as with 409, so the status is
        // not the signal here — the message is, and it is the whole point of the variant.
        StateConflict { resource, state, server_message } => a
            .fact("resource", resource.clone().unwrap_or_default())
            .fact("state", state.clone().unwrap_or_default())
            .fact("server says", quoted(server_message))
            .todo(Line::text(
                "the current state prevents this operation. Check the server message above.",
            ))
            .todo(Line::step(1, api_command(ctx)).and_text("  (read the current state)"))
            .todo(
                Line::text(
                    "2. resolve the reported state conflict, then retry",
                )
                .hang(3),
            )
            .todo(Line::note(
                "changing token scopes does not resolve a state conflict.",
            )),

        ChecksPending { slug, pr, pending } => {
            let scope = slug.as_deref().map(|s| format!(" -R {s}")).unwrap_or_default();
            let a = a
                .fact("pull request", pr)
                .fact("repository", slug.clone().unwrap_or_default())
                .fact(
                    "still running",
                    if pending.is_empty() { String::new() } else { pending.join(", ") },
                );
            a.todo(Line::text(
                "wait for the pending checks to finish.",
            ))
            .todo(Line::step(1, format!("fjo pr checks {pr}{scope}")).and_text("  (run it again in a minute)"))
            .todo(
                Line::text("2. or enable automatic merge: ")
                    .and_cmd(format!("fjo pr merge {pr}{scope} --auto"))
                    .hang(3),
            )
            .todo(Line::note(
                "pending checks return exit code 8. Rate limits also use this code.",
            ))
        }

        // No invented remedy. The checks table printed directly above already names the check
        // that failed, and fjo does not know why it failed — a status check is a name, a state
        // and a URL. So the advice is the URL, and the reason there is no `fjo run view` here.
        ChecksFailed { slug, pr, failed } => {
            let scope = slug.as_deref().map(|s| format!(" -R {s}")).unwrap_or_default();
            let mut a = a.fact("pull request", pr).fact("repository", slug.clone().unwrap_or_default());
            for (i, FailedCheck { name, url }) in failed.iter().enumerate() {
                let text = match url.as_deref() {
                    Some(u) => format!("{name} — {u}"),
                    None => name.clone(),
                };
                a = if i == 0 { a.fact("failed", text) } else { a.cont(text) };
            }
            let published_a_url = failed.iter().any(|c| c.url.is_some());
            let a = a.todo(Line::text(
                "inspect the failed checks listed above.",
            ));
            let a = if published_a_url {
                a.todo(
                    Line::text(
                        "1. open each failed check's URL for details",
                    )
                    .hang(3),
                )
            } else {
                a.todo(
                    Line::text(
                        "1. no URL was published; check the logs in the service that runs these checks",
                    )
                    .hang(3),
                )
            };
            a.todo(
                Line::step(2, format!("fjo pr checks {pr}{scope} --web"))
                    .and_text("  (the same checks in a browser)"),
            )
            .todo(Line::note(
                "checks may come from external services. For Forgejo Actions, use fjo run list.",
            ))
        }

        RunFailed { slug, run, conclusion, failed_jobs, url } => {
            let scope = slug.as_deref().map(|s| format!(" -R {s}")).unwrap_or_default();
            let a = a
                .fact("run", run)
                .fact("ended in", conclusion)
                .fact("repository", slug.clone().unwrap_or_default())
                .fact(
                    "failed jobs",
                    if failed_jobs.is_empty() { String::new() } else { failed_jobs.join(", ") },
                )
                .fact("web", url.clone().unwrap_or_default());
            a.todo(Line::text(
                "inspect the workflow logs to find the cause.",
            ))
            .todo(
                Line::step(1, format!("fjo run view {run}{scope} --log-failed"))
                    .and_text("  (only the steps that failed)"),
            )
            .todo(
                Line::step(2, format!("fjo run logs {run}{scope}"))
                    .and_text("  (full logs)"),
            )
            .todo(Line::note(
                "cancelled or timed-out runs may not have started. Check runner availability with fjo run runners.",
            ))
        }

        // The content of this advice is load-bearing: `http::paginate`'s rule-(f) test asserts
        // that the rendering names the page count, a `?page`-ignoring instance, a
        // header-stripping proxy, and something actionable. That is the contract, not decoration.
        PaginationDidNotTerminate { pages, items } => a
            .fact("pages fetched", pages.to_string())
            .fact("items so far", items.to_string())
            .todo(Line::text(
                "the page limit was reached before the server indicated the end of the results.",
            ))
            .todo(Line::text(
                "check whether the server ignores ?page or a proxy strips the Link and X-Total-Count headers.",
            ))
            .todo(
                Line::text("1. inspect the response headers: ")
                    .and_cmd(format!("{} -i", api_command(ctx)))
                    .hang(3),
            )
            .todo(
                Line::text("2. request a limited number of items: ")
                    .and_cmd(format!("{} --limit 500", api_command(ctx)))
                    .hang(3),
            )
            .todo(Line::text("3. report unexpected pagination behavior to the server or proxy administrator"))
            .todo(Line::note(
                "the request failed because the result may be incomplete.",
            )),

        RateLimited { host, retry_after } => a
            .fact("host", host)
            .fact(
                "retry after",
                match retry_after {
                    Some(d) => format!("{}s (reported by the server)", d.as_secs()),
                    None => "not stated".to_owned(),
                },
            )
            .todo(Line::text("wait before sending more requests."))
            .todo(Line::step(1, format!("sleep {}", retry_after.map_or(60, |d| d.as_secs().max(1)))).and_text("  then run it again"))
            .todo(
                Line::text(
                    "2. reduce the request rate in scripts; server limits may be shared",
                )
                .hang(3),
            ),

        Forbidden { server_message } => a
            .fact("server says", quoted(server_message))
            .todo(Line::text(
                "the server denied this action. Check the message and your account permissions.",
            ))
            .todo(Line::step(1, "fjo api user --jq .login").and_text("  (confirm who you are)"))
            .todo(
                Line::text("2. ask for the access you need, or, as an instance admin, act as")
                    .hang(3),
            )
            .todo(Line::text("   someone who has it with ").and_cmd("--sudo <user>")),

        ServerError { status, server_message } => a
            .fact("status", status.to_string())
            .fact("server says", quoted(server_message))
            .todo(Line::text("the server returned an error. Check its availability and logs."))
            .todo(Line::step(1, "fjo api version").and_text("  (is the instance up?)"))
            .todo(
                Line::text(
                    "2. if the error persists, ask the server administrator to check the logs",
                )
                .hang(3),
            ),

        UnexpectedStatus { status, body_excerpt } => a
            .fact("status", status.to_string())
            .fact("body", quoted(body_excerpt))
            .todo(Line::text("inspect the response for details."))
            .todo(Line::step(1, format!("{} -i", api_command(ctx))).and_text("  (the full response)"))
            .todo(
                Line::text(
                    "2. check the server and reverse proxy logs",
                )
                .hang(3),
            ),

        // -------------------------------------------------------------------- local
        Decode { pointer, expected, body_excerpt } => a
            .fact("at", pointer)
            .fact("expected", expected)
            .fact("body", quoted(body_excerpt))
            .todo(Line::text(
                "the response does not match the expected API format.",
            ))
            .todo(Line::step(1, api_command(ctx)).and_text("  (inspect the API response)"))
            .todo(Line::text("2. report the error with the field path shown above"))
            .todo(Line::note("a write may have succeeded before response decoding failed. Check before retrying.")),

        // No `problem` fact: the message is already the headline, and repeating it verbatim two
        // lines later reads like a bug. And no "that is not a shape fjo accepts" either — it is
        // not always true. `Usage` also carries "not implemented yet" and "this flag is not
        // supported by this build", where that sentence is simply wrong.
        //
        // The consequence is that a `Usage` message must be self-sufficient: it is the entire
        // explanation the user gets, so write it as advice rather than as a complaint.
        Usage(_) => a
            .todo(Line::step(1, "fjo --help"))
            .todo(Line::step(2, "fjo raw search <words>").and_text("  (find the operation you want)")),

        UnknownJsonField { given, available, suggest } => {
            let a = a.fact("asked for", given).fact("did you mean", suggest.clone().unwrap_or_default());
            let a = if available.is_empty() {
                a
            } else {
                a.fact("available", available.join(", "))
            };
            a.todo(Line::text("use --json for top-level fields and --jq for nested fields."))
                .todo(Line::step(1, "fjo pr list --json").and_text("  (list available fields)"))
                .todo(Line::step(2, "fjo pr list --jq '.[].head.ref'").and_text("  (select a nested field)"))
        }

        JqCompile { expr, message, col } => {
            let a = a.fact("expr", expr);
            let a = match col {
                // A caret under the offending column, aligned with the expression above it.
                Some(c) => a.cont(format!("{}^", " ".repeat(*c))),
                None => a,
            };
            a.fact("message", message)
                .todo(Line::text("correct the jq expression using the error above."))
                .todo(
                    Line::step(1, "fjo api repos/OWNER/REPO/pulls > /tmp/p.json")
                        .and_text("  then iterate with jq locally")
                        .hang(3),
                )
                .todo(Line::text("2. reference: ").and_url("https://jqlang.github.io/jq/manual/").hang(3))
        }

        Template { message, line } => a
            .fact("line", line.to_string())
            .fact("message", message)
            .todo(Line::text(
                "helpers are: tablerow, tablerender, timeago, timefmt, truncate, color, autocolor, \
                 join, pluck, hyperlink.",
            ))
            .todo(Line::text("1. try this template:").hang(3))
            .todo(Line::text("   ").and_cmd(
                "fjo pr list --template '{{range .}}{{tablerow .number .title}}{{end}}{{tablerender}}'",
            ))
            .todo(Line::note("tablerender prints buffered rows; remaining rows are printed automatically at the end.")),

        PathNotFound { path, what } => a
            .fact("path", path.display().to_string())
            .fact("wanted for", *what)
            .todo(Line::text(format!(
                "check the path to the {what}."
            )))
            .todo(Line::step(1, format!("ls -l {}", path.display())))
            .todo(
                Line::text(
                    "2. relative paths start at the current directory, not the repository root",
                )
                .hang(3),
            )
            .todo(Line::note(
                "check wildcard matches; some shells pass an unmatched pattern as a literal filename.",
            )),

        // git's stderr goes in whole, one physical line per physical line, rather than being
        // summarised. It is the only account of what happened, and the same rule that forbids
        // discarding a server message forbids discarding this.
        GitFailed { command, stderr, status } => {
            let mut a = a.fact("ran", command).fact(
                "exit",
                status.map(|s| s.to_string()).unwrap_or_else(|| "killed by a signal".to_owned()),
            );
            for (i, line) in
                stderr.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).enumerate()
            {
                a = if i == 0 { a.fact("git says", line) } else { a.cont(line) };
            }
            a.todo(Line::text(
                "check Git's error above. Your Git configuration and credential helpers apply.",
            ))
            .todo(Line::step(1, command.clone()).and_text("  (run Git directly)"))
            .todo(
                Line::text(
                    "2. for authentication failures, check Git's credential configuration",
                )
                .hang(3),
            )
            .todo(Line::note("fjo auth setup-git makes git use the same credential fjo does."))
        }

        // git worked; the *server* declined. Which is why the remedy is not git advice, and why
        // the stderr goes in whole: Forgejo's entire reply arrives as `remote:` lines, and there
        // is no response body anywhere else to read it from.
        AgitRefused { refspec, remedy, stderr } => {
            let mut a = a.fact("refspec", refspec);
            for (i, line) in
                stderr.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).enumerate()
            {
                a = if i == 0 { a.fact("git says", line) } else { a.cont(line) };
            }
            match remedy {
                AgitRemedy::ForcePush => a
                    .todo(Line::text(
                        "amended or rebased commits require a force push to update the AGit pull request.",
                    ))
                    .todo(Line::step(1, "fjo pr create --agit --force-push"))
                    .todo(Line::note(
                        "keep the same topic to update this pull request; a new topic creates another.",
                    )),

                AgitRemedy::PushOptionsDisabled => a
                    .todo(Line::text(
                        "AGit requires Git push options, which this server has disabled.",
                    ))
                    .todo(
                        Line::text("1. ask the server administrator to enable Git push options")
                            .hang(3),
                    )
                    .todo(
                        Line::step(2, "fjo pr create --head <branch>")
                            .and_text("  (create from a branch instead)"),
                    ),

                AgitRemedy::TopicRequired => a
                    .todo(Line::text(
                        "AGit requires a topic. Reuse it when updating the same pull request.",
                    ))
                    .todo(Line::step(1, "fjo pr create --agit --topic <name>"))
                    .todo(Line::note(
                        "--topic defaults to the current branch name.",
                    )),

                AgitRemedy::Unrecognised => a
                    .todo(Line::text(
                        "check the server response above for the reason.",
                    ))
                    .todo(
                        Line::step(1, "fjo pr create --head <branch>")
                            .and_text("  (if you can push a branch)"),
                    )
                    .todo(Line::note(
                        "if AGit is unavailable, create a pull request from a branch you can push.",
                    )),
            }
        }

        Io(e) => a
            .fact("cause", e.to_string())
            .todo(Line::text("check the local path, permissions, and available disk space."))
            .todo(Line::step(1, "df -h .").and_text("  (out of space?)"))
            .todo(Line::text("2. check that the path exists and is writable by you")),

        Cancelled => a
            .todo(Line::text("check the operation's result before retrying."))
            .todo(Line::step(1, api_command(ctx)).and_text("  (check whether the request went through)"))
            .todo(Line::note("a request already sent may still have completed on the server.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ctx() -> RequestCtx {
        RequestCtx {
            host: Some("git.example.org".into()),
            login: Some("perf3ct".into()),
            method: Some("POST".into()),
            path: Some("/api/v1/repos/perf3ct/fjo/issues".into()),
            repo: Some("perf3ct/fjo".into()),
            status: Some(403),
            token_source: Some(TokenSource::Keyring { entry: "fjo:git.example.org".into() }),
            credential_kind: Some(CredentialKind::Pat),
        }
    }

    /// Names every variant, and is **exhaustive on purpose**: adding an `ErrorKind` variant makes
    /// this match fail to compile, which forces you to add it to `all_variants` below, which makes
    /// the "every variant has a remedy" test cover it. That chain is the mechanism that stops a
    /// variant shipping with no advice.
    fn variant_name(k: &ErrorKind) -> &'static str {
        use ErrorKind::*;
        match k {
            Dns { .. } => "Dns",
            Connect { .. } => "Connect",
            Tls { .. } => "Tls",
            Timeout { .. } => "Timeout",
            Proxy { .. } => "Proxy",
            NoHostConfigured => "NoHostConfigured",
            UnknownHost { .. } => "UnknownHost",
            NotAuthenticated { .. } => "NotAuthenticated",
            WebSessionMissing { .. } => "WebSessionMissing",
            WebSessionExpired { .. } => "WebSessionExpired",
            WebLoginFailed { .. } => "WebLoginFailed",
            WebAuthnRequired { .. } => "WebAuthnRequired",
            TokenRejected { .. } => "TokenRejected",
            OauthNotSupported { .. } => "OauthNotSupported",
            OauthAuthorizationDenied { .. } => "OauthAuthorizationDenied",
            OauthStateMismatch { .. } => "OauthStateMismatch",
            OauthCallbackUnavailable { .. } => "OauthCallbackUnavailable",
            OauthTokenExchangeFailed { .. } => "OauthTokenExchangeFailed",
            OauthRefreshFailed { .. } => "OauthRefreshFailed",
            OauthEntropyUnavailable { .. } => "OauthEntropyUnavailable",
            InsufficientScope { .. } => "InsufficientScope",
            TwoFactorRequired { .. } => "TwoFactorRequired",
            KeyringUnavailable { .. } => "KeyringUnavailable",
            CredFilePermissions { .. } => "CredFilePermissions",
            NotAGitRepo => "NotAGitRepo",
            RepoNotResolved { .. } => "RepoNotResolved",
            AmbiguousRemote { .. } => "AmbiguousRemote",
            RemoteHostUnknown { .. } => "RemoteHostUnknown",
            RepoNotFound { .. } => "RepoNotFound",
            ResourceNotFound { .. } => "ResourceNotFound",
            RouteNotFound { .. } => "RouteNotFound",
            Conflict { .. } => "Conflict",
            Validation { .. } => "Validation",
            QuotaExceeded { .. } => "QuotaExceeded",
            Archived { .. } => "Archived",
            PaginationDidNotTerminate { .. } => "PaginationDidNotTerminate",
            StateConflict { .. } => "StateConflict",
            ChecksPending { .. } => "ChecksPending",
            ChecksFailed { .. } => "ChecksFailed",
            RunFailed { .. } => "RunFailed",
            RateLimited { .. } => "RateLimited",
            Forbidden { .. } => "Forbidden",
            ServerError { .. } => "ServerError",
            UnexpectedStatus { .. } => "UnexpectedStatus",
            Decode { .. } => "Decode",
            Usage(_) => "Usage",
            UnknownJsonField { .. } => "UnknownJsonField",
            JqCompile { .. } => "JqCompile",
            Template { .. } => "Template",
            PathNotFound { .. } => "PathNotFound",
            GitFailed { .. } => "GitFailed",
            AgitRefused { .. } => "AgitRefused",
            Io(_) => "Io",
            Cancelled => "Cancelled",
        }
    }

    /// Every variant, with plausible data. Keep in step with `variant_name`.
    fn all_variants() -> Vec<ErrorKind> {
        use ErrorKind::*;
        vec![
            Dns { host: "git.exmaple.org".into() },
            Connect {
                host: "git.example.org".into(),
                port: 443,
                cause: "connection refused".into(),
                looks_like_plaintext: false,
                scheme: "https".into(),
            },
            // Both branches, because they give OPPOSITE advice and the gate below only
            // guarantees a runnable command for variants it is actually handed.
            Connect {
                host: "172.17.0.1".into(),
                port: 39683,
                cause: "received corrupt message of type InvalidContentType".into(),
                looks_like_plaintext: true,
                scheme: "https".into(),
            },
            // Every other fixture here speaks https, which is exactly why the hardcoded
            // `https://` in the refused-connection advice went unnoticed. This is the case that
            // exposes it: a plain-HTTP instance on a private address, refused.
            Connect {
                host: "192.168.1.5".into(),
                port: 8080,
                cause: "tcp connect error: Connection refused (os error 111)".into(),
                looks_like_plaintext: false,
                scheme: "http".into(),
            },
            Tls {
                host: "git.example.org".into(),
                cause: "invalid peer certificate: UnknownIssuer".into(),
                looks_like_private_ca: true,
            },
            Tls {
                host: "git.example.org".into(),
                cause: "invalid peer certificate: Expired".into(),
                looks_like_private_ca: false,
            },
            Timeout {
                host: "git.example.org".into(),
                after: Duration::from_secs(30),
                phase: Phase::Headers,
            },
            Proxy {
                proxy: "proxy.corp:3128".into(),
                cause: "407 Proxy Authentication Required".into(),
            },
            NoHostConfigured,
            UnknownHost { given: "codeberg.org".into(), known: vec!["git.example.org".into()] },
            NotAuthenticated { host: "git.example.org".into() },
            WebSessionMissing { host: "git.example.org".into() },
            WebSessionExpired { host: "git.example.org".into() },
            // Both shapes: the server's message is the useful half when there is one, and its
            // absence must not produce advice with an empty fact line.
            WebLoginFailed {
                host: "git.example.org".into(),
                reason: Some("Username or password is incorrect.".into()),
            },
            WebLoginFailed { host: "git.example.org".into(), reason: None },
            WebAuthnRequired { host: "git.example.org".into() },
            TokenRejected {
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            OauthNotSupported {
                host: "git.example.org".into(),
                tried: vec![
                    "/.well-known/openid-configuration".into(),
                    "/login/oauth/access_token".into(),
                ],
            },
            OauthAuthorizationDenied {
                host: "git.example.org".into(),
                error: "access_denied".into(),
                description: Some("the user denied the request".into()),
            },
            OauthStateMismatch { host: "git.example.org".into() },
            // Both shapes: they render different headlines and different facts, and the gate
            // below only guarantees a runnable command for what it is actually handed.
            OauthCallbackUnavailable {
                host: "git.example.org".into(),
                port: Some(45231),
                reason: CallbackFailure::Timeout(120),
            },
            OauthCallbackUnavailable {
                host: "git.example.org".into(),
                port: None,
                reason: CallbackFailure::Bind("permission denied".into()),
            },
            OauthTokenExchangeFailed {
                host: "git.example.org".into(),
                error: "invalid_request".into(),
                description: Some("PKCE is required for public clients".into()),
            },
            OauthRefreshFailed {
                host: "git.example.org".into(),
                login: "perf3ct".into(),
                reason: Some("invalid_grant".into()),
            },
            OauthEntropyUnavailable { cause: "Operation not permitted (os error 1)".into() },
            InsufficientScope {
                host: "git.example.org".into(),
                needed: vec!["write:issue".into()],
                have: Some(vec!["read:repository".into(), "read:issue".into()]),
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            InsufficientScope {
                host: "git.example.org".into(),
                needed: vec!["write:issue".into()],
                have: None,
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            InsufficientScope {
                host: "git.example.org".into(),
                needed: vec![],
                have: None,
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            TwoFactorRequired { host: "git.example.org".into() },
            KeyringUnavailable { cause: KeyringCause::NoBackend },
            KeyringUnavailable { cause: KeyringCause::Timeout },
            CredFilePermissions {
                path: PathBuf::from("/home/u/.config/fjo/hosts.toml"),
                mode: 0o644,
            },
            NotAGitRepo,
            RepoNotResolved {
                tried: vec![
                    Attempt::new("-R/--repo", "not given"),
                    Attempt::new("FJO_REPO", "not set"),
                    Attempt::new("git remotes", "none configured"),
                ],
            },
            AmbiguousRemote {
                candidates: vec![
                    RemoteCandidate {
                        remote: "fork".into(),
                        host: "git.example.org".into(),
                        slug: "me/fjo".into(),
                    },
                    RemoteCandidate {
                        remote: "mirror".into(),
                        host: "codeberg.org".into(),
                        slug: "perf3ct/fjo".into(),
                    },
                ],
            },
            RemoteHostUnknown { remote: "origin".into(), host: "work-forge".into() },
            RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: true,
                server_message: None,
            },
            // The probe was off, so a body that named the missing thing landed here instead of
            // on `ResourceNotFound`. It must still reach the user — and the headline must not
            // announce a repository nobody looked for.
            RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: false,
                server_message: Some(
                    "could not find 'no-such-branch' to be a commit, branch or tag".into(),
                ),
            },
            // The same unchecked case with a silent server: the remedy is still the check that
            // was skipped, not a list of causes for a repository nobody ruled out.
            RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: false,
                server_message: None,
            },
            ResourceNotFound {
                kind: "pull request",
                id: "4212".into(),
                slug: Some("perf3ct/fjo".into()),
                server_message: None,
            },
            // The collection: no identifier in the path, and the server said why.
            ResourceNotFound {
                kind: "pull request",
                id: String::new(),
                slug: Some("perf3ct/fjo".into()),
                server_message: Some(
                    "could not find 'no-such-branch' to be a commit, branch or tag".into(),
                ),
            },
            // The same, with a server that said nothing usable — the remedy must not claim a
            // message is printed above when none is.
            ResourceNotFound {
                kind: "pull request",
                id: String::new(),
                slug: Some("perf3ct/fjo".into()),
                server_message: None,
            },
            ResourceNotFound {
                kind: "package",
                id: String::new(),
                slug: None,
                server_message: None,
            },
            RouteNotFound {
                method: "POST".into(),
                path: "/api/v1/repos/perf3ct/fjo/actions/runs/12/rerun".into(),
                instance: Some("forgejo 7.0.0".into()),
            },
            Conflict {
                server_message: "The pull request is not mergeable: base branch has been updated"
                    .into(),
            },
            Validation {
                fields: vec![FieldError {
                    field: Some("title".into()),
                    message: "can't be blank".into(),
                }],
                server_message: None,
            },
            Validation {
                fields: vec![],
                server_message: Some("user does not exist [name: nope]".into()),
            },
            QuotaExceeded {
                server_message: "quota exceeded for size:assets".into(),
                uploading: Some("fjo-v1.2.3-linux.tar.gz".into()),
            },
            Archived { slug: "perf3ct/fjo".into(), host: "git.example.org".into() },
            StateConflict {
                resource: Some("pull request 4212".into()),
                state: None,
                server_message: "The head branch is behind the base branch".into(),
            },
            // The porcelain path: the command already fetched the pull request, so it knows the
            // state and there is no server message to quote.
            StateConflict {
                resource: Some("pull request 4212".into()),
                state: Some("closed".into()),
                server_message: String::new(),
            },
            ChecksPending {
                slug: Some("perf3ct/fjo".into()),
                pr: "4212".into(),
                pending: vec!["build / test (pull_request)".into()],
            },
            ChecksPending { slug: None, pr: "4212".into(), pending: vec![] },
            ChecksFailed {
                slug: Some("perf3ct/fjo".into()),
                pr: "4212".into(),
                failed: vec![FailedCheck::new(
                    "build / test (pull_request)",
                    Some("https://ci.example.org/runs/9".into()),
                )],
            },
            // A check that published no URL: the advice must not tell the reader to open one.
            ChecksFailed {
                slug: None,
                pr: "4212".into(),
                failed: vec![FailedCheck::new("lint", None)],
            },
            ChecksFailed { slug: None, pr: "4212".into(), failed: vec![] },
            RunFailed {
                slug: Some("perf3ct/fjo".into()),
                run: "918".into(),
                conclusion: "failure".into(),
                failed_jobs: vec!["test (ubuntu-latest)".into()],
                url: Some("https://git.example.org/perf3ct/fjo/actions/runs/918".into()),
            },
            RunFailed {
                slug: None,
                run: "918".into(),
                conclusion: String::new(),
                failed_jobs: vec![],
                url: None,
            },
            PaginationDidNotTerminate { pages: 10_000, items: 500_000 },
            RateLimited {
                host: "git.example.org".into(),
                retry_after: Some(Duration::from_secs(30)),
            },
            Forbidden { server_message: "user is not a collaborator on this repository".into() },
            ServerError { status: 502, server_message: "upstream connect error".into() },
            UnexpectedStatus { status: 418, body_excerpt: "I'm a teapot".into() },
            Decode {
                pointer: "/items/3/head/repo/owner".into(),
                expected: "invalid type: null, expected a string".into(),
                body_excerpt: r#"[{"head":{"repo":{"owner":null}}}]"#.into(),
            },
            Usage("--limit expects a positive integer".into()),
            UnknownJsonField {
                given: "titel".into(),
                available: vec!["number".into(), "title".into(), "state".into()],
                suggest: Some("title".into()),
            },
            JqCompile {
                expr: ".[] | .titel".into(),
                message: "undefined variable".into(),
                col: Some(6),
            },
            Template { message: "unknown helper \"tablerowx\"".into(), line: 3 },
            PathNotFound {
                path: PathBuf::from("dist/fjo-v1.2.3-linux.tar.gz"),
                what: "release asset",
            },
            GitFailed {
                command: "git push origin HEAD:refs/for/main/my-topic".into(),
                stderr: "remote: Permission to perf3ct/fjo.git denied.\nfatal: unable to access 'https://git.example.org/perf3ct/fjo.git/': The requested URL returned error: 403"
                    .into(),
                status: Some(128),
            },
            GitFailed {
                command: "git rev-parse --abbrev-ref HEAD".into(),
                stderr: String::new(),
                status: None,
            },
            AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::ForcePush,
                stderr: "! [remote rejected] HEAD -> refs/for/main/fix-parser (non-fast-forward)"
                    .into(),
            },
            AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::PushOptionsDisabled,
                stderr: "fatal: the receiving end does not support push options".into(),
            },
            AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::TopicRequired,
                stderr: "remote: Forgejo: topic is required".into(),
            },
            AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::Unrecognised,
                stderr: String::new(),
            },
            Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied")),
            Cancelled,
        ]
    }

    /// **The test that stops a variant shipping without a remedy.**
    ///
    /// Every `ErrorKind` must render a "what to do:" section containing at least one literal
    /// runnable command. Without this, the natural failure mode of adding a variant is a message
    /// that describes a problem and offers nothing.
    #[test]
    fn every_variant_renders_a_what_to_do_section_with_a_runnable_command() {
        for kind in all_variants() {
            let name = variant_name(&kind);
            let advice = advise(&kind, &ctx());
            assert!(!advice.todo.is_empty(), "{name}: no 'what to do' section");
            assert!(
                advice.todo.iter().any(Line::has_command),
                "{name}: 'what to do' has no literal runnable command"
            );

            let err = Error { kind: Box::new(kind), ctx: Box::new(ctx()) };
            let text = render(&err, Color::Never);
            assert!(text.starts_with("error: "), "{name}: {text}");
            assert!(text.contains("what to do:"), "{name}: {text}");
        }
    }

    /// Bug this prevents: an expired OAuth session rendering the personal-access-token advice,
    /// which sends the user to /user/settings/applications to make a token they do not need and
    /// cannot use to fix the thing that broke.
    #[test]
    fn a_rejected_oauth_session_is_told_to_log_in_again_not_to_make_a_token() {
        let kind = ErrorKind::TokenRejected {
            host: "git.example.org".into(),
            login: Some("perf3ct".into()),
            settings_url: "https://git.example.org/user/settings/applications".into(),
        };

        let mut oauth = ctx();
        oauth.credential_kind = Some(CredentialKind::Oauth2);
        let text = render(&Error { kind: Box::new(kind), ctx: Box::new(oauth) }, Color::Never);
        assert!(text.contains("fjo auth login --host git.example.org --web"), "{text}");
        assert!(!text.contains("create a new token"), "{text}");

        let kind = ErrorKind::TokenRejected {
            host: "git.example.org".into(),
            login: Some("perf3ct".into()),
            settings_url: "https://git.example.org/user/settings/applications".into(),
        };
        let pat = ctx();
        assert_eq!(pat.credential_kind, Some(CredentialKind::Pat));
        let text = render(&Error { kind: Box::new(kind), ctx: Box::new(pat) }, Color::Never);
        assert!(text.contains("create a new token"), "{text}");
    }

    /// A headline is the one line a user reads first, and it has a house style: lowercase start,
    /// no trailing period, no Rust jargon leaking through a `Debug` impl.
    #[test]
    fn headlines_follow_the_house_style() {
        for kind in all_variants() {
            let name = variant_name(&kind);
            let h = headline(&kind);
            assert!(!h.is_empty(), "{name}: empty headline");
            assert!(!h.contains('\n'), "{name}: headline must be one line: {h}");
            assert!(!h.ends_with('.'), "{name}: no trailing period: {h}");
            for jargon in ["Err(", "ErrorKind", "unwrap", "None,", "Some(", "{:?}"] {
                assert!(!h.contains(jargon), "{name}: jargon {jargon:?} in {h}");
            }
        }
    }

    /// All variants must survive the styled path too — an `anstyle` sequence built from a
    /// malformed string would only show up here.
    #[test]
    fn every_variant_also_renders_with_colour() {
        for kind in all_variants() {
            let name = variant_name(&kind);
            let err = Error { kind: Box::new(kind), ctx: Box::new(ctx()) };
            let text = render(&err, Color::Always);
            assert!(text.contains("\u{1b}["), "{name}: no styling emitted");
            assert!(text.contains("what to do:"), "{name}");
        }
    }

    /// Colour must collapse to exactly the plain text, byte for byte, so that piping into a file
    /// or a test harness produces something diffable.
    #[test]
    fn plain_and_styled_renderings_differ_only_in_escape_sequences() {
        let err = Error {
            kind: Box::new(ErrorKind::NoHostConfigured),
            ctx: Box::new(RequestCtx::default()),
        };
        let plain = render(&err, Color::Never);
        let styled = render(&err, Color::Always);
        let stripped: String = strip_ansi(&styled);
        assert_eq!(plain, stripped);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Facts must never contain a credential. The `token from:` line names *where* the credential
    /// lives, never what it is.
    #[test]
    fn no_rendering_can_contain_a_token() {
        let err = Error {
            kind: Box::new(ErrorKind::TokenRejected {
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                settings_url: "https://git.example.org/user/settings/applications".into(),
            }),
            ctx: Box::new(ctx()),
        };
        let text = render(&err, Color::Never);
        assert!(text.contains("the OS keyring, entry fjo:git.example.org"));
        assert!(!text.contains("token abc"), "{text}");
    }

    /// `Display for Error` is the headline, not the essay — log lines and `--json` want one line.
    #[test]
    fn display_is_the_headline_only() {
        let err = Error::new(ErrorKind::Cancelled);
        assert_eq!(err.to_string(), "cancelled");
    }

    // ---------------------------------------------------------------------- snapshots

    fn snap(kind: ErrorKind, ctx: RequestCtx) -> String {
        render(&Error { kind: Box::new(kind), ctx: Box::new(ctx) }, Color::Never)
    }

    /// The worked example from the plan. This snapshot *is* the specification of the voice: if it
    /// changes, the change was to how the tool feels and wants reviewing as such.
    #[test]
    fn snapshot_insufficient_scope() {
        insta::assert_snapshot!(snap(
            ErrorKind::InsufficientScope {
                host: "git.example.org".into(),
                needed: vec!["write:issue".into()],
                have: Some(vec!["read:repository".into(), "read:issue".into()]),
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            ctx()
        ));
    }

    #[test]
    fn snapshot_insufficient_scope_with_unknown_scopes() {
        insta::assert_snapshot!(snap(
            ErrorKind::InsufficientScope {
                host: "git.example.org".into(),
                needed: vec!["write:issue".into()],
                have: None,
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            ctx()
        ));
    }

    #[test]
    fn snapshot_token_rejected() {
        let mut c = ctx();
        c.status = Some(401);
        c.method = Some("GET".into());
        c.path = Some("/api/v1/user".into());
        insta::assert_snapshot!(snap(
            ErrorKind::TokenRejected {
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                settings_url: "https://git.example.org/user/settings/applications".into(),
            },
            c
        ));
    }

    /// Both flavours of 404, side by side, because the whole point is that they read differently.
    #[test]
    fn snapshot_repo_not_found_is_honestly_ambiguous() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls/4212".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: true,
                server_message: None,
            },
            c
        ));
    }

    /// The same variant when the probe never ran — `Client::probe_404(false)`, which every bulk
    /// loop sets, or a probe that failed. The headline must not say the repository could not be
    /// found, because nobody looked; the remedy is the look itself.
    #[test]
    fn snapshot_repo_not_checked_does_not_claim_the_repository_is_missing() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls/4212".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: false,
                server_message: None,
            },
            c
        ));
    }

    /// Bug this pins: the headline asserting the repository was not found directly above a
    /// `server says:` line about a *branch*, with nothing having checked the repository at all.
    #[test]
    fn an_unchecked_404_does_not_announce_a_missing_repository() {
        let kind = ErrorKind::RepoNotFound {
            slug: "perf3ct/fjo".into(),
            host: "git.example.org".into(),
            login: Some("perf3ct".into()),
            probed: false,
            server_message: Some(
                "could not find 'no-such-branch' to be a commit, branch or tag".into(),
            ),
        };
        let h = headline(&kind);
        assert!(!h.contains("could not find the repository"), "{h}");
        assert!(h.contains("perf3ct/fjo"), "the slug is still named: {h}");

        let text = render(&Error { kind: Box::new(kind), ctx: Box::new(ctx()) }, Color::Never);
        assert!(text.contains("(not checked)"), "{text}");
        // The remedy is the request the probe would have made, spelled out and runnable.
        assert!(text.contains("fjo api repos/perf3ct/fjo"), "{text}");
    }

    #[test]
    fn snapshot_resource_not_found_is_definite() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls/4212".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::ResourceNotFound {
                kind: "pull request",
                id: "4212".into(),
                slug: Some("perf3ct/fjo".into()),
                server_message: None,
            },
            c
        ));
    }

    /// The third flavour of 404, and the one that had no rendering of its own: a `POST` to a
    /// collection. There is no identifier in the path, so the diagnostic must not claim one is
    /// wrong — and the server's sentence is the only thing that knows what actually was.
    #[test]
    fn snapshot_resource_not_found_on_a_collection_blames_no_identifier() {
        let mut c = ctx();
        c.method = Some("POST".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::ResourceNotFound {
                kind: "pull request",
                id: String::new(),
                slug: Some("perf3ct/fjo".into()),
                server_message: Some(
                    "could not find 'no-such-branch' to be a commit, branch or tag".into(),
                ),
            },
            c
        ));
    }

    /// The same shape with a silent server. The remedy must change, not just lose a line: there
    /// is no "message above" to point at.
    #[test]
    fn snapshot_resource_not_found_on_a_collection_without_a_message() {
        let mut c = ctx();
        c.method = Some("POST".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::ResourceNotFound {
                kind: "pull request",
                id: String::new(),
                slug: Some("perf3ct/fjo".into()),
                server_message: None,
            },
            c
        ));
    }

    /// With the disambiguation probe disabled, the *same* body arrives on `RepoNotFound`. The
    /// server's own sentence has to survive the trip — and it is exactly this pairing that made
    /// the old collapsed wording indefensible: "could not find the repository perf3ct/fjo" sat
    /// one line above a server message about a branch, having checked nothing.
    #[test]
    fn snapshot_repo_not_found_keeps_the_server_message_too() {
        let mut c = ctx();
        c.method = Some("POST".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::RepoNotFound {
                slug: "perf3ct/fjo".into(),
                host: "git.example.org".into(),
                login: Some("perf3ct".into()),
                probed: false,
                server_message: Some(
                    "could not find 'no-such-branch' to be a commit, branch or tag".into(),
                ),
            },
            c
        ));
    }

    #[test]
    fn snapshot_route_not_found() {
        let mut c = ctx();
        c.path = Some("/api/v1/repos/perf3ct/fjo/actions/runs/12/rerun".into());
        c.status = Some(404);
        insta::assert_snapshot!(snap(
            ErrorKind::RouteNotFound {
                method: "POST".into(),
                path: "/api/v1/repos/perf3ct/fjo/actions/runs/12/rerun".into(),
                instance: Some("forgejo 7.0.0".into()),
            },
            c
        ));
    }

    #[test]
    fn snapshot_conflict_keeps_the_server_message() {
        let mut c = ctx();
        c.method = Some("POST".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls/12/merge".into());
        c.status = Some(409);
        insta::assert_snapshot!(snap(
            ErrorKind::Conflict {
                server_message: "The pull request is not mergeable: base branch has been updated"
                    .into(),
            },
            c
        ));
    }

    #[test]
    fn snapshot_validation() {
        let mut c = ctx();
        c.status = Some(422);
        insta::assert_snapshot!(snap(
            ErrorKind::Validation {
                fields: vec![
                    FieldError { field: Some("title".into()), message: "can't be blank".into() },
                    FieldError {
                        field: Some("assignees".into()),
                        message: "user does not exist [name: nope]".into()
                    },
                ],
                server_message: None,
            },
            c
        ));
    }

    #[test]
    fn snapshot_quota_exceeded() {
        let mut c = ctx();
        c.path = Some("/api/v1/repos/perf3ct/fjo/releases/12/assets".into());
        c.status = Some(413);
        insta::assert_snapshot!(snap(
            ErrorKind::QuotaExceeded {
                server_message: "quota exceeded for size:assets".into(),
                uploading: Some("fjo-v1.2.3-linux.tar.gz".into()),
            },
            c
        ));
    }

    #[test]
    fn snapshot_archived() {
        let mut c = ctx();
        c.status = Some(423);
        insta::assert_snapshot!(snap(
            ErrorKind::Archived { slug: "perf3ct/fjo".into(), host: "git.example.org".into() },
            c
        ));
    }

    #[test]
    fn snapshot_state_conflict_from_a_405() {
        let mut c = ctx();
        c.method = Some("POST".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls/4212/merge".into());
        c.status = Some(405);
        insta::assert_snapshot!(snap(
            ErrorKind::StateConflict {
                resource: Some("pull request 4212".into()),
                state: None,
                server_message: "The head branch is behind the base branch".into(),
            },
            c
        ));
    }

    /// The porcelain shape: the command already knew the state, so there is no server message and
    /// the headline says the state outright.
    #[test]
    fn snapshot_state_conflict_known_locally() {
        insta::assert_snapshot!(snap(
            ErrorKind::StateConflict {
                resource: Some("pull request 4212".into()),
                state: Some("closed".into()),
                server_message: String::new(),
            },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_checks_pending() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/commits/abc123/status".into());
        c.status = Some(200);
        insta::assert_snapshot!(snap(
            ErrorKind::ChecksPending {
                slug: Some("perf3ct/fjo".into()),
                pr: "4212".into(),
                pending: vec!["build / test (pull_request)".into(), "lint".into()],
            },
            c
        ));
    }

    /// The counterpart to `snapshot_checks_pending`, and the point of the pair: pending says
    /// "wait", failed says "here is the log". Neither invents a remedy the other's shape lacks.
    #[test]
    fn snapshot_checks_failed() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/commits/abc123/status".into());
        c.status = Some(200);
        insta::assert_snapshot!(snap(
            ErrorKind::ChecksFailed {
                slug: Some("perf3ct/fjo".into()),
                pr: "4212".into(),
                failed: vec![
                    FailedCheck::new(
                        "build / test (pull_request)",
                        Some("https://ci.example.org/runs/9".into())
                    ),
                    FailedCheck::new("lint", None),
                ],
            },
            c
        ));
    }

    /// The whole reason this is not `Usage`: every check here published a URL and none of them
    /// is a Forgejo Actions run id, so the advice points at the URLs.
    #[test]
    fn snapshot_checks_failed_with_no_urls_does_not_offer_one() {
        insta::assert_snapshot!(snap(
            ErrorKind::ChecksFailed {
                slug: None,
                pr: "4212".into(),
                failed: vec![FailedCheck::new("lint", None)],
            },
            RequestCtx::default()
        ));
    }

    /// An AGit refusal, which used to render as a multi-line `Usage` headline. The diagnosis is
    /// now a fact and the remedy is in the "what to do" block, where the three-part shape puts it.
    #[test]
    fn snapshot_agit_refused_needs_a_force_push() {
        insta::assert_snapshot!(snap(
            ErrorKind::AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::ForcePush,
                stderr: "remote: Gitea: user does not have permission\n\
                     ! [remote rejected] HEAD -> refs/for/main/fix-parser (non-fast-forward)"
                    .into(),
            },
            RequestCtx::default()
        ));
    }

    /// The refusal nothing on the client side can fix, which is exactly why exit 2 was wrong:
    /// no command line could have avoided this.
    #[test]
    fn snapshot_agit_refused_without_push_options() {
        insta::assert_snapshot!(snap(
            ErrorKind::AgitRefused {
                refspec: "HEAD:refs/for/main/fix-parser".into(),
                remedy: AgitRemedy::PushOptionsDisabled,
                stderr: "fatal: the receiving end does not support push options".into(),
            },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_run_failed() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/actions/runs/918".into());
        c.status = Some(200);
        insta::assert_snapshot!(snap(
            ErrorKind::RunFailed {
                slug: Some("perf3ct/fjo".into()),
                run: "918".into(),
                conclusion: "failure".into(),
                failed_jobs: vec!["test (ubuntu-latest)".into()],
                url: Some("https://git.example.org/perf3ct/fjo/actions/runs/918".into()),
            },
            c
        ));
    }

    /// A local miss, next to `ResourceNotFound` above: they must not read alike, because they do
    /// not mean alike.
    #[test]
    fn snapshot_path_not_found_is_local() {
        insta::assert_snapshot!(snap(
            ErrorKind::PathNotFound {
                path: PathBuf::from("dist/fjo-v1.2.3-linux.tar.gz"),
                what: "release asset",
            },
            RequestCtx::default()
        ));
    }

    /// Every line of git's stderr survives, one physical line each.
    #[test]
    fn snapshot_git_failed_keeps_every_line_of_stderr() {
        insta::assert_snapshot!(snap(
            ErrorKind::GitFailed {
                command: "git push origin HEAD:refs/for/main/my-topic".into(),
                stderr: "remote: Permission to perf3ct/fjo.git denied.\nfatal: unable to access 'https://git.example.org/perf3ct/fjo.git/': The requested URL returned error: 403"
                    .into(),
                status: Some(128),
            },
            RequestCtx::default()
        ));
    }

    /// The wording here is a contract with `http::paginate`'s rule-(f) test, which asserts on
    /// this rendering rather than on the variant.
    #[test]
    fn snapshot_pagination_did_not_terminate() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/issues".into());
        c.status = Some(200);
        insta::assert_snapshot!(snap(
            ErrorKind::PaginationDidNotTerminate { pages: 10_000, items: 500_000 },
            c
        ));
    }

    /// The case that cost a round trip through CI: the port was right and the advice said to
    /// change it. A plain-HTTP instance on a container or LAN address hits this every time,
    /// because `scheme_for` assumes https for anything that is not loopback.
    #[test]
    fn snapshot_connect_to_a_plaintext_server() {
        insta::assert_snapshot!(snap(
            ErrorKind::Connect {
                host: "172.17.0.1".into(),
                port: 39683,
                cause: "received corrupt message of type InvalidContentType".into(),
                looks_like_plaintext: true,
                scheme: "https".into(),
            },
            RequestCtx {
                host: Some("172.17.0.1".into()),
                method: Some("GET".into()),
                path: Some("/api/v1/repos/fjotest/agit".into()),
                ..Default::default()
            },
        ));
    }

    /// The two branches must not converge: telling someone to check the port when the port is
    /// correct is the failure this split exists to prevent.
    #[test]
    fn a_plaintext_connect_does_not_blame_the_port() {
        let plain = snap(
            ErrorKind::Connect {
                host: "172.17.0.1".into(),
                port: 39683,
                cause: "received corrupt message of type InvalidContentType".into(),
                looks_like_plaintext: true,
                scheme: "https".into(),
            },
            RequestCtx::default(),
        );
        assert!(plain.contains("http://172.17.0.1:39683"), "{plain}");
        assert!(!plain.contains(":3000"), "must not suggest a different port:\n{plain}");

        let refused = snap(
            ErrorKind::Connect {
                host: "git.example.org".into(),
                port: 443,
                cause: "connection refused".into(),
                looks_like_plaintext: false,
                scheme: "https".into(),
            },
            RequestCtx::default(),
        );
        assert!(refused.contains("the connection failed"), "{refused}");
    }

    /// Bug this prevents: the advice for a refused connection asserting `https://` whatever the
    /// request actually used, and then suggesting a re-login with a bare `<host>:3000`.
    ///
    /// For a plain-HTTP instance on a private address the first is merely wrong — it sends the
    /// reader to curl a URL the server does not serve. The second is worse: a bare host makes
    /// `config::hosts::scheme_for` guess https for anything that is not loopback, so a reader
    /// who followed it would arrive at the *plaintext* failure the sibling branch above exists
    /// to explain. Two remedies, each walking the reader into the other's error.
    ///
    /// The renderer must repeat the scheme it was handed rather than choose one.
    #[test]
    fn a_refused_connect_repeats_the_scheme_the_request_used() {
        let http = snap(
            ErrorKind::Connect {
                host: "192.168.1.5".into(),
                port: 8080,
                cause: "tcp connect error: Connection refused (os error 111)".into(),
                looks_like_plaintext: false,
                scheme: "http".into(),
            },
            RequestCtx::default(),
        );
        assert!(
            !http.contains("https://"),
            "the request was plain HTTP; no part of the advice may switch the reader to \
             https:\n{http}"
        );
        assert!(http.contains("http://192.168.1.5:8080/api/v1/version"), "{http}");
        assert!(
            http.contains("--host http://192.168.1.5:3000"),
            "the login suggestion has to carry the scheme, or scheme_for guesses https for this \
             non-loopback host and the reader lands in the plaintext branch:\n{http}"
        );

        // And https is still https: the fix is to repeat the scheme, not to prefer http.
        let https = snap(
            ErrorKind::Connect {
                host: "git.example.org".into(),
                port: 443,
                cause: "connection refused".into(),
                looks_like_plaintext: false,
                scheme: "https".into(),
            },
            RequestCtx::default(),
        );
        assert!(https.contains("https://git.example.org"), "{https}");
        assert!(!https.contains("http://git.example.org"), "{https}");
    }

    #[test]
    fn snapshot_network_failure() {
        insta::assert_snapshot!(snap(
            ErrorKind::Connect {
                host: "git.example.org".into(),
                port: 443,
                cause: "tcp connect error: Connection refused (os error 111)".into(),
                looks_like_plaintext: false,
                scheme: "https".into(),
            },
            RequestCtx {
                host: Some("git.example.org".into()),
                method: Some("GET".into()),
                path: Some("/api/v1/user".into()),
                ..RequestCtx::default()
            }
        ));
    }

    #[test]
    fn snapshot_keyring_unavailable() {
        insta::assert_snapshot!(snap(
            ErrorKind::KeyringUnavailable { cause: KeyringCause::NoBackend },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_decode_names_the_pointer() {
        let mut c = ctx();
        c.method = Some("GET".into());
        c.path = Some("/api/v1/repos/perf3ct/fjo/pulls".into());
        c.status = Some(200);
        insta::assert_snapshot!(snap(
            ErrorKind::Decode {
                pointer: "/3/head/repo/owner/login".into(),
                expected: "invalid type: null, expected a string".into(),
                body_excerpt: r#"[{"head":{"repo":{"owner":{"login":null}}}}]"#.into(),
            },
            c
        ));
    }

    #[test]
    fn snapshot_unknown_json_field() {
        insta::assert_snapshot!(snap(
            ErrorKind::UnknownJsonField {
                given: "titel".into(),
                available: vec!["number".into(), "title".into(), "state".into(), "url".into()],
                suggest: Some("title".into()),
            },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_forbidden_is_not_a_scope_problem() {
        insta::assert_snapshot!(snap(
            ErrorKind::Forbidden {
                server_message: "user is not a collaborator on this repository".into()
            },
            ctx()
        ));
    }

    #[test]
    fn snapshot_rate_limited() {
        let mut c = ctx();
        c.status = Some(429);
        insta::assert_snapshot!(snap(
            ErrorKind::RateLimited {
                host: "git.example.org".into(),
                retry_after: Some(Duration::from_secs(30)),
            },
            c
        ));
    }

    #[test]
    fn snapshot_two_factor_required() {
        insta::assert_snapshot!(snap(
            ErrorKind::TwoFactorRequired { host: "git.example.org".into() },
            ctx()
        ));
    }

    /// The private-CA case, which is the one a self-hosted user actually hits.
    #[test]
    fn snapshot_tls_private_ca() {
        insta::assert_snapshot!(snap(
            ErrorKind::Tls {
                host: "git.internal.corp".into(),
                cause: "invalid peer certificate: UnknownIssuer".into(),
                looks_like_private_ca: true,
            },
            RequestCtx {
                host: Some("git.internal.corp".into()),
                method: Some("GET".into()),
                path: Some("/api/v1/user".into()),
                ..RequestCtx::default()
            }
        ));
    }

    /// The caret must land under the reported column, which only the `cont` fact alignment makes
    /// possible.
    #[test]
    fn snapshot_jq_compile_error_points_at_the_column() {
        insta::assert_snapshot!(snap(
            ErrorKind::JqCompile {
                expr: ".[] | .titel".into(),
                message: "undefined variable".into(),
                col: Some(6),
            },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_ambiguous_remote() {
        insta::assert_snapshot!(snap(
            ErrorKind::AmbiguousRemote {
                candidates: vec![
                    RemoteCandidate {
                        remote: "fork".into(),
                        host: "git.example.org".into(),
                        slug: "me/fjo".into(),
                    },
                    RemoteCandidate {
                        remote: "mirror".into(),
                        host: "codeberg.org".into(),
                        slug: "perf3ct/fjo".into(),
                    },
                ],
            },
            RequestCtx::default()
        ));
    }

    #[test]
    fn snapshot_repo_not_resolved_shows_its_work() {
        insta::assert_snapshot!(snap(
            ErrorKind::RepoNotResolved {
                tried: vec![
                    Attempt::new("-R/--repo", "not given"),
                    Attempt::new("FJO_REPO / FORGEJO_REPO", "not set"),
                    Attempt::new("git config remote.*.fjo-resolved", "no value"),
                    Attempt::new("remote name scoring", "no remote matched a configured host"),
                ],
            },
            RequestCtx::default()
        ));
    }
}

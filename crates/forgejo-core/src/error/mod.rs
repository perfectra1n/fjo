//! The error taxonomy, and the contract every other module codes against.
//!
//! Two rules govern this module, and both are load-bearing:
//!
//! 1. **Every variant must be renderable into advice.** [`render`] turns an [`ErrorKind`]
//!    into three parts: what happened, the relevant facts, and what to *do* about it. A
//!    test in that module asserts every variant produces a "what to do" section, so a new
//!    variant cannot ship without a remedy.
//! 2. **Never discard a server message.** Forgejo's error bodies are inconsistent, but even
//!    an ugly server message beats a generic one. [`classify`] always preserves it.

pub mod classify;
pub mod compat;
pub mod render;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An error, plus the request context needed to explain it.
///
/// **Both** fields are boxed, and that matters more than it looks. `Result<T, Error>` is the
/// return type of all 506 generated client functions, so `Error`'s size is paid on every one
/// of them. `ErrorKind` is large because it carries owned diagnostics, but so is `RequestCtx`
/// — seven `Option<String>`s. Boxing only the enum left `Error` at 168 bytes, over Clippy's
/// `result_large_err` threshold, and the lint then fired on every fallible function in the
/// crate. Boxing both brings it to 16 bytes; `Deref` on the accessors means no call site had
/// to change.
#[derive(Debug)]
pub struct Error {
    pub kind: Box<ErrorKind>,
    pub ctx: Box<RequestCtx>,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Self { kind: Box::new(kind), ctx: Box::new(RequestCtx::default()) }
    }

    /// Attach request context. Called by the transport as an error propagates outward.
    pub fn with_ctx(mut self, ctx: RequestCtx) -> Self {
        self.ctx = Box::new(ctx);
        self
    }

    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::new(kind)
    }
}

impl fmt::Display for Error {
    /// The single-line summary. For the full three-part diagnostic, use
    /// [`render::render`], which is what the binary prints.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render::headline(&self.kind))
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &*self.kind {
            ErrorKind::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// What we know about the request that failed. Every field is optional because errors can
/// originate before a request exists (config problems) or after it is gone (decode errors).
#[derive(Debug, Default, Clone)]
pub struct RequestCtx {
    pub host: Option<String>,
    pub login: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub repo: Option<String>,
    pub status: Option<u16>,
    /// Where the credential came from. Lets a 401 message say *which* token was rejected,
    /// which is the difference between an actionable error and a confusing one.
    pub token_source: Option<TokenSource>,
    /// What kind of credential it was. Decides whether a 401 advises creating a new token or
    /// logging in again.
    pub credential_kind: Option<CredentialKind>,
}

/// What kind of credential was presented, as distinct from [`TokenSource`]'s *where it came
/// from*.
///
/// Both are needed to explain a 401. "The token in your keyring was rejected" and "your OAuth
/// session expired" are the same status code from the same place, with different remedies: one
/// asks you to make a new token, the other to log in again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// A personal access token. Opaque, and does not expire.
    Pat,
    /// An OAuth2 access token. A JWT, and expires within the hour.
    Oauth2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Keyring { entry: String },
    File { path: PathBuf },
    Env { var: String },
    Flag,
}

/// Why the OAuth redirect never arrived. Two shapes, one variant, because the facts differ
/// but the remedy converges on the same two commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackFailure {
    /// The loopback socket could not be bound at all.
    Bind(String),
    /// Bound fine, but nothing came back within this many seconds.
    Timeout(u64),
}

/// Which phase of a request timed out. Distinguishing these matters: a connect timeout is
/// a network problem, a body timeout usually is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connect,
    Headers,
    Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyringCause {
    /// No credential backend at all — on Linux, typically no D-Bus session. This is the
    /// normal case over SSH, in containers, and in CI, so it must never be fatal.
    NoBackend,
    Locked,
    Denied,
    Timeout,
    Other(String),
}

/// A single field-level complaint from a 422 response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: Option<String>,
    pub message: String,
}

/// One step of repository resolution, recorded so a failure can show its work rather than
/// just saying "could not determine repository".
#[derive(Debug, Clone)]
pub struct Attempt {
    pub what: &'static str,
    pub outcome: String,
}

impl Attempt {
    pub fn new(what: &'static str, outcome: impl Into<String>) -> Self {
        Self { what, outcome: outcome.into() }
    }
}

/// One failed status check, exactly as the checks table showed it.
///
/// A status check is a name, a state and a URL, and that really is all the API offers. The URL
/// is the only pointer to the log that is correct for *both* Forgejo Actions and an external CI
/// system posting through the commit-status API, which is why it is carried rather than a run
/// id: a run id would be right only for the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check's name — its `context`, which is what the CHECK column shows.
    pub name: String,
    /// The check's own `target_url`, when it published one.
    pub url: Option<String>,
}

impl FailedCheck {
    pub fn new(name: impl Into<String>, url: Option<String>) -> Self {
        Self { name: name.into(), url: url.filter(|u| !u.trim().is_empty()) }
    }
}

/// What a refused AGit push leaves the user able to do.
///
/// AGit's entire protocol is `git push`, so the server's reason arrives as `remote:` lines on
/// git's stderr and nowhere else — there is no status code and no JSON body to classify. Three
/// of those reasons have a specific remedy; the fourth is the honest admission that this one is
/// not recognised, because inventing a cause for an unknown refusal is how a tool sends someone
/// after the wrong problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgitRemedy {
    /// The update was not a fast-forward. Pushing the same topic again is how an AGit pull
    /// request is updated, and an amended or rebased history needs `--force-push`.
    ForcePush,
    /// The instance does not accept git push options, which AGit needs to carry the title and
    /// description. Nothing on the client side changes that.
    PushOptionsDisabled,
    /// `refs/for/<base>` arrived without a topic, which Forgejo refuses.
    TopicRequired,
    /// Not a refusal this build recognises. git's stderr is the whole diagnosis.
    Unrecognised,
}

impl AgitRemedy {
    /// Classify git's stderr.
    ///
    /// The order of the tests is load-bearing: an instance without push options prints
    /// `rejected` alongside its own message, so the specific check has to run before the
    /// general one or every such refusal would be reported as an amended history.
    pub fn from_stderr(stderr: &str) -> Self {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("does not support push options") {
            Self::PushOptionsDisabled
        } else if lower.contains("non-fast-forward")
            || lower.contains("fetch first")
            || lower.contains("rejected")
        {
            Self::ForcePush
        } else if lower.contains("topic") {
            Self::TopicRequired
        } else {
            Self::Unrecognised
        }
    }
}

/// `#[non_exhaustive]` because this crate is published: adding a variant on a spec bump
/// must not be a breaking change for downstream matches.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    // ---------------------------------------------------------------- transport
    Dns {
        host: String,
    },
    Connect {
        host: String,
        port: u16,
        cause: String,
        /// The scheme the request actually used, so the remedy can repeat it instead of
        /// asserting one.
        ///
        /// Without this the advice hardcoded `https://`, which is wrong in the case that
        /// matters most: a plain-HTTP instance on a private address whose connection was
        /// refused would be told to try `https://<host>` and to re-login with a bare
        /// `<host>:3000`. A bare host makes `config::hosts::scheme_for` guess https for
        /// anything non-loopback, so following that advice lands the reader in the
        /// `looks_like_plaintext` branch below — the renderer walking someone out of one
        /// connect failure and into the other.
        scheme: String,
        /// The TLS layer read the reply as a handshake and found HTTP.
        ///
        /// Set when the failure carries rustls' signature for exactly that. It matters because
        /// the ordinary advice for a failed connect — check the port — is precisely wrong here:
        /// the port is right, and it is the scheme that is not. `scheme_for` guesses `https` for
        /// anything that is not loopback, so a plain-HTTP instance on a private address lands
        /// here every time.
        looks_like_plaintext: bool,
    },
    Tls {
        host: String,
        cause: String,
        looks_like_private_ca: bool,
    },
    Timeout {
        host: String,
        after: Duration,
        phase: Phase,
    },
    Proxy {
        proxy: String,
        cause: String,
    },

    // ---------------------------------------------------------- config and auth
    NoHostConfigured,
    UnknownHost {
        given: String,
        known: Vec<String>,
    },
    NotAuthenticated {
        host: String,
    },
    /// No web session is filed for this host, and one is required: the route being asked for is
    /// not in the API, so an API token cannot substitute.
    WebSessionMissing {
        host: String,
    },
    /// A web session existed but could not be renewed, because the long-lived remember token is
    /// gone: expired, revoked by "log out everywhere", or invalidated by a password change.
    ///
    /// Distinct from [`ErrorKind::WebSessionMissing`] because the remedy is the same command but
    /// the cause is not, and a user who just logged in deserves to be told that it lapsed rather
    /// than that it was never there.
    WebSessionExpired {
        host: String,
    },
    /// Forgejo refused the password (or the second factor) at `/user/login`.
    ///
    /// `reason` is the server's own flash message where one could be read, because Forgejo
    /// distinguishes cases this tool should not try to re-derive — a wrong password, a disabled
    /// account, a login source that forbids password auth.
    WebLoginFailed {
        host: String,
        reason: Option<String>,
    },
    /// The account's second factor is WebAuthn, which has no headless completion.
    WebAuthnRequired {
        host: String,
    },
    TokenRejected {
        host: String,
        login: Option<String>,
        settings_url: String,
    },
    /// Forgejo token scopes are `read:<area>` / `write:<area>` and are fixed at creation —
    /// they cannot be added to an existing token. `have` is `None` when the API does not
    /// tell us, and the message says so rather than guessing.
    InsufficientScope {
        host: String,
        needed: Vec<String>,
        have: Option<Vec<String>>,
        settings_url: String,
    },
    TwoFactorRequired {
        host: String,
    },
    KeyringUnavailable {
        cause: KeyringCause,
    },
    CredFilePermissions {
        path: PathBuf,
        mode: u32,
    },

    // ------------------------------------------------------------------- OAuth2
    /// The instance answers nothing at the OAuth2 endpoints. Usually an older Forgejo, or an
    /// administrator who emptied `[oauth2] DEFAULT_APPLICATIONS`.
    OauthNotSupported {
        host: String,
        tried: Vec<String>,
    },
    /// The browser came back with `error=`, most often `access_denied` — the Authorize button
    /// was not clicked.
    OauthAuthorizationDenied {
        host: String,
        error: String,
        description: Option<String>,
    },
    /// The `state` on the callback did not match the one we sent. No code was exchanged.
    OauthStateMismatch {
        host: String,
    },
    /// The loopback listener could not be bound, or nothing arrived before the deadline.
    OauthCallbackUnavailable {
        host: String,
        port: Option<u16>,
        reason: CallbackFailure,
    },
    /// The token endpoint refused the authorization code.
    OauthTokenExchangeFailed {
        host: String,
        error: String,
        description: Option<String>,
    },
    /// The operating system's random source refused. Effectively impossible on a working
    /// system, but a login cannot proceed without unguessable values, and silently continuing
    /// with predictable ones would defeat both PKCE and the state check.
    OauthEntropyUnavailable {
        cause: String,
    },
    /// The refresh token is spent or expired. Forgejo's default refresh lifetime is 730 hours,
    /// so this is what a month-old session looks like.
    OauthRefreshFailed {
        host: String,
        login: String,
        reason: Option<String>,
    },

    // -------------------------------------------------------- context resolution
    NotAGitRepo,
    RepoNotResolved {
        tried: Vec<Attempt>,
    },
    AmbiguousRemote {
        candidates: Vec<RemoteCandidate>,
    },
    RemoteHostUnknown {
        remote: String,
        host: String,
    },

    // ------------------------------------------------------------ API semantics
    /// A repository-scoped 404 that the disambiguation probe did not resolve to an inner object.
    ///
    /// [`probed`](ErrorKind::RepoNotFound::probed) is the difference between the two cases this
    /// carries, and it decides the headline. `true` means `GET /repos/{owner}/{repo}` was asked
    /// and 404'd too: the repository really is unreachable, ambiguously between "does not
    /// exist", "private and the token lacks scope", and "wrong host", so the message names all
    /// three. `false` means nobody asked — and the message must not then claim the repository
    /// was not found, because that is an assertion made on no evidence.
    RepoNotFound {
        slug: String,
        host: String,
        login: Option<String>,
        /// Whether `GET /repos/{owner}/{repo}` was actually run and came back 404.
        ///
        /// `false` when the probe was turned off (`Client::probe_404(false)`, which bulk loops
        /// do) or when the probe itself failed, so the repository's existence is simply unknown.
        /// Collapsing the two used to make a never-checked 404 report "could not find the
        /// repository perf3ct/fjo" on no evidence — and once `server_message` was added, that
        /// headline could sit directly above a server sentence contradicting it.
        probed: bool,
        /// What the server said, when it said anything beyond "not found".
        ///
        /// The same slot, and the same reason, as [`ErrorKind::ResourceNotFound::server_message`]
        /// below — and it is needed here for a reason that is easy to miss: with the
        /// disambiguation probe turned off or failing, *every* repository-scoped 404 lands here,
        /// including the ones whose body names exactly what was missing.
        server_message: Option<String>,
    },
    /// A 404 where the repository *was* found, so only the inner thing is missing.
    ///
    /// An **empty `id`** is not a missing object at all: it means the path ended on the
    /// collection (`POST /repos/{o}/{r}/pulls`), so there was never an identifier in it to be
    /// wrong. The renderer says so rather than reporting a nameless object, because advice to go
    /// and list pull requests is advice about the thing the user was trying to create.
    ResourceNotFound {
        kind: &'static str,
        /// The identifier from the path, or empty when the path named only the collection.
        id: String,
        slug: Option<String>,
        /// What the server said, when it said anything beyond "not found".
        ///
        /// Without this the message is discarded **by construction**, which is the rule the whole
        /// module exists to enforce — and 404 is where it bites hardest, because Forgejo answers
        /// a create whose head branch does not exist with a 404 whose body is
        /// `["could not find 'no-such-branch' to be a commit, branch or tag …"]`. That sentence
        /// is the entire diagnosis. `None` when the body only restated the status code, so that
        /// `server says: Not Found` never takes up a line.
        server_message: Option<String>,
    },
    /// A 404 that looks like the endpoint itself is absent — an older Forgejo.
    RouteNotFound {
        method: String,
        path: String,
        instance: Option<String>,
    },
    Conflict {
        server_message: String,
    },
    Validation {
        fields: Vec<FieldError>,
        server_message: Option<String>,
    },
    /// HTTP 413. Forgejo-specific: quotas count LFS objects, packages, and release assets
    /// together, not just the git repository.
    QuotaExceeded {
        server_message: String,
        uploading: Option<String>,
    },
    /// HTTP 423.
    Archived {
        slug: String,
        host: String,
    },
    /// The thing exists and you may touch it — it is simply not in a state where this operation
    /// means anything: merging a closed pull request, re-running a run that is still going,
    /// deleting a release that was never published.
    ///
    /// Forgejo answers most of these with **405**, not 409, and the body is the only place the
    /// real reason appears — which is why `server_message` is carried verbatim and why
    /// [`classify`] no longer reads a 405 as a missing route. `state` is for the cases where the
    /// caller already knows (it fetched the pull request before trying to merge it) and can say
    /// so without a server message.
    StateConflict {
        /// What was refused, e.g. `pull request 4212`. Display only.
        resource: Option<String>,
        /// The state it is actually in — `closed`, `merged`, `draft`. Display only.
        state: Option<String>,
        server_message: String,
    },
    /// A pull request's status checks have not finished.
    ///
    /// Deliberately not a flavour of "checks failed": nothing has failed, and the remedy is to
    /// wait rather than to fix anything. See [`ErrorKind::exit_code`] for why this is the one
    /// variant that shares an exit code with another.
    ChecksPending {
        slug: Option<String>,
        /// The pull request index as the user typed it, without the `#`.
        pr: String,
        /// The checks still running, when they are known. Names only.
        pending: Vec<String>,
    },
    /// A pull request's status checks finished, and at least one of them failed.
    ///
    /// The counterpart to [`ErrorKind::ChecksPending`], and deliberately a separate variant from
    /// [`ErrorKind::RunFailed`]: a *status check* is not necessarily an Actions run. Anything
    /// holding a token can post one through the commit-status API, so advice pointing at
    /// `fjo run view <run>` is wrong for every check that came from external CI, and there is no
    /// run id to point at in the first place. What every check does have is its own URL, which is
    /// why [`FailedCheck`] carries that and nothing else.
    ///
    /// This variant adds no prose the checks table did not already print. Its job is that the
    /// exit code comes from [`ErrorKind::exit_code`] like every other status in the tool, rather
    /// than from a `std::process::exit` that no unit test can observe.
    ChecksFailed {
        slug: Option<String>,
        /// The pull request index as the user typed it, without the `#`.
        pr: String,
        /// The checks that failed. Names, and their own URLs where they published one.
        failed: Vec<FailedCheck>,
    },
    /// An Actions run finished, unsuccessfully.
    RunFailed {
        slug: Option<String>,
        /// The run id as the user would pass it back to `fjo run view`.
        run: String,
        /// The run's own word for how it ended: `failure`, `cancelled`, `timed_out`.
        conclusion: String,
        failed_jobs: Vec<String>,
        /// The web URL of the run, when the API gave one.
        url: Option<String>,
    },
    /// Pagination hit its own ceiling: every page came back full and the instance never once
    /// said the collection had ended.
    ///
    /// Rule (f) in `http::paginate`. Rules (a)-(e) all need the server to say *something* — a
    /// `Link` without a `next`, a short page, an `X-Total-Count`, an empty page — and an
    /// instance that ignores `?page`, or a reverse proxy that strips `Link` and `X-Total-Count`,
    /// says none of them. The walk errors rather than truncating, because quietly returning the
    /// first N pages would be the same silent data loss the pagination rules exist to prevent,
    /// just arriving from the other direction.
    PaginationDidNotTerminate {
        /// Pages actually consumed before the ceiling fired.
        pages: u32,
        /// Items accumulated across them.
        items: usize,
    },
    RateLimited {
        host: String,
        retry_after: Option<Duration>,
    },
    /// A 403 that is *not* a scope problem.
    Forbidden {
        server_message: String,
    },
    ServerError {
        status: u16,
        server_message: String,
    },
    UnexpectedStatus {
        status: u16,
        body_excerpt: String,
    },

    // -------------------------------------------------------------------- data
    /// A response did not match the generated model. `pointer` is a JSON pointer to the
    /// exact offending field, courtesy of `serde_path_to_error` — far more useful than a
    /// byte offset when the body is 40 KB of JSON.
    Decode {
        pointer: String,
        expected: String,
        body_excerpt: String,
    },

    // ------------------------------------------------------------------- local
    Usage(String),
    UnknownJsonField {
        given: String,
        available: Vec<String>,
        suggest: Option<String>,
    },
    JqCompile {
        expr: String,
        message: String,
        col: Option<usize>,
    },
    Template {
        message: String,
        line: usize,
    },
    /// A path the user named does not exist **on this machine**.
    ///
    /// Deliberately separate from [`ErrorKind::ResourceNotFound`], which is an API 404. The two
    /// read almost identically and have nothing else in common: one is fixed with `ls`, the
    /// other with `fjo pr list`, and a script that treats "not on the server" and "not on my
    /// disk" as the same condition will do the wrong thing with at least one of them. They also
    /// carry different exit codes for exactly that reason.
    PathNotFound {
        path: PathBuf,
        /// What it was wanted for — `release asset`, `--body-file`. Display only.
        what: &'static str,
    },
    /// A shelled-out `git` command failed.
    ///
    /// fjo runs the real `git` rather than linking libgit2, so that a user's `insteadOf`
    /// rewrites, `includeIf` blocks, and credential helpers all apply. The price is that git's
    /// diagnostics arrive as a subprocess's stderr, and the same rule that governs server
    /// bodies governs them: the subprocess is the only party that knows what went wrong, so
    /// both the command and its stderr are carried verbatim and printed.
    GitFailed {
        /// The command as run, e.g. `git push origin HEAD:refs/for/main/my-topic`.
        command: String,
        /// git's own stderr, unedited.
        stderr: String,
        /// The exit status, or `None` when the process was signalled rather than exiting.
        status: Option<i32>,
    },
    /// The server refused an AGit push — `git push <remote> HEAD:refs/for/<base>/<topic>`.
    ///
    /// Distinct from [`ErrorKind::GitFailed`], which covers a git command that failed for git's
    /// own reasons. Here git worked perfectly and the *server* declined the pull request, and the
    /// three refusals that happen in practice each have a different answer — force the push, ask
    /// an admin, or give the ref a topic. `git push` being the entire AGit protocol is what puts
    /// a server refusal in a subprocess's stderr, so the stderr is carried verbatim under the
    /// same rule that forbids discarding a server's message body.
    ///
    /// Callers must scrub the stderr before it arrives here: a remote URL can carry an embedded
    /// credential and git repeats that URL in a push failure.
    AgitRefused {
        /// The refspec that was pushed, e.g. `HEAD:refs/for/main/fix-parser`.
        refspec: String,
        /// What can be done about it, classified from the stderr below.
        remedy: AgitRemedy,
        /// git's stderr, scrubbed but otherwise verbatim. Forgejo's whole reply is in here.
        stderr: String,
    },
    Io(std::io::Error),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct RemoteCandidate {
    pub remote: String,
    pub host: String,
    pub slug: String,
}

impl ErrorKind {
    /// Exit codes follow `gh` where `gh` has a convention, so wrapper scripts written
    /// against `gh` keep working: 0 success, 1 generic, 2 usage, 4 auth. Beyond that:
    /// 5 not found, 6 network, 7 server, 8 rate limited, 130 interrupted.
    ///
    /// Four of these deserve their reasoning written down.
    ///
    /// [`ErrorKind::PathNotFound`] is **2, not 5**. 5 has one job — "the server does not have
    /// it" — and a caller uses it to decide whether to create the thing. A local path that does
    /// not exist is a mistake in the command line, caught before anything is sent, so it belongs
    /// with `Usage` and friends. Collapsing the two would make 5 useless for the decision it
    /// exists to support.
    ///
    /// [`ErrorKind::PaginationDidNotTerminate`] is **7, the server bucket**, even though the
    /// remedy on offer is `--limit N`. 7 and 2 answer different questions, and the one a script
    /// asks here is "was my invocation wrong?" — it was not. The request was valid and correctly
    /// formed; the far end ignored `?page` or a proxy ate the headers. `--limit` is a way to
    /// work *around* a misbehaving instance, not a correction of a mistake, in the same way that
    /// `Retry-After` does not make [`ErrorKind::RateLimited`] a usage error. Filing this under 2
    /// would tell a CI wrapper to go fix the script, which is the one thing that cannot help.
    /// It is not in [`ErrorKind::is_transient`]: a retry walks into the same wall.
    ///
    /// [`ErrorKind::AgitRefused`] is **1, not 2**, and the taxonomy's own history argues the other
    /// way: before it had a variant this was an [`ErrorKind::Usage`], which exits 2. That was
    /// wrong on the merits. 2 says the invocation was wrong, and nothing about the invocation was:
    /// the user typed a valid command and the *server* declined the push — because the history was
    /// amended, or because the instance was built without push options, which no command line
    /// could have avoided. Exit 2 tells a CI wrapper to go and fix the script, which is the one
    /// thing that cannot help. It belongs with the other settled refusals at 1, next to
    /// [`ErrorKind::GitFailed`], whose shape it shares.
    ///
    /// [`ErrorKind::ChecksFailed`] is **1, the code `gh pr checks` uses** and the code
    /// `fjo pr checks` already produced — by calling `std::process::exit` directly, the last such
    /// call in the tree, until this variant gave it somewhere to go. The number is not what is interesting here; where it comes from is. A
    /// hand-written exit is a second source of truth for the table this function *is*, and no unit
    /// test can observe it without spawning a process. It is deliberately not 8: 8 means "wait and
    /// try again", which is [`ErrorKind::ChecksPending`] below, and a check that has already failed
    /// will not pass by waiting.
    ///
    /// [`ErrorKind::ChecksPending`] is **8, which [`ErrorKind::RateLimited`] also uses**, and
    /// that is deliberate rather than an oversight. `gh pr checks` exits 8 for pending checks,
    /// and `case $? in 8) sleep 60;; esac` is the idiom the command exists to support; diverging
    /// would break every script ported from `gh`. The overlap is harmless because the two
    /// conditions call for the *same* action — wait and try again — so a script that branches on
    /// 8 does the right thing under either. Anything that must tell them apart has
    /// [`Error::kind`].
    pub fn exit_code(&self) -> i32 {
        use ErrorKind::*;
        match self {
            Usage(_)
            | UnknownJsonField { .. }
            | JqCompile { .. }
            | Template { .. }
            | PathNotFound { .. } => 2,

            NotAuthenticated { .. }
            | TokenRejected { .. }
            | InsufficientScope { .. }
            | TwoFactorRequired { .. }
            | NoHostConfigured
            | UnknownHost { .. }
            | OauthNotSupported { .. }
            | OauthAuthorizationDenied { .. }
            | OauthStateMismatch { .. }
            | OauthCallbackUnavailable { .. }
            | OauthTokenExchangeFailed { .. }
            | OauthRefreshFailed { .. }
            | OauthEntropyUnavailable { .. } => 4,

            RepoNotFound { .. } | ResourceNotFound { .. } | RouteNotFound { .. } => 5,

            Dns { .. } | Connect { .. } | Tls { .. } | Timeout { .. } | Proxy { .. } => 6,

            ServerError { .. } | PaginationDidNotTerminate { .. } => 7,

            RateLimited { .. } | ChecksPending { .. } => 8,

            Cancelled => 130,

            // 1 is the honest answer for the rest: something the user asked for did not happen,
            // and no finer-grained code would tell a script anything it could act on.
            // `StateConflict`, `RunFailed` and `GitFailed` all land here alongside `Conflict`.
            _ => 1,
        }
    }

    /// Whether retrying this request could plausibly succeed. Note that this says nothing
    /// about whether it is *safe* to retry — see `http::retry`, which additionally refuses
    /// to retry POST and PATCH regardless of what this returns.
    pub fn is_transient(&self) -> bool {
        use ErrorKind::*;
        matches!(
            self,
            Dns { .. } | Connect { .. } | Timeout { .. } | RateLimited { .. } | ServerError { .. }
        )
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(ErrorKind::Io(e))
    }
}

/// Convenience constructor for the most common local error.
pub fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boxing invariant, asserted rather than commented.
    ///
    /// `Error` is the error half of every one of the 506 generated client functions, so its size
    /// is paid on all of them. Boxing only `ErrorKind` left it at 168 bytes and Clippy's
    /// `result_large_err` then fired crate-wide. Adding a variant may grow `ErrorKind` freely —
    /// it is behind a `Box` — but un-boxing either field would reintroduce the lint everywhere at
    /// once, and this test is what stops that happening by accident.
    #[test]
    fn error_is_two_pointers_wide() {
        assert_eq!(std::mem::size_of::<Error>(), 2 * std::mem::size_of::<usize>());
        assert_eq!(std::mem::size_of::<Result<(), Error>>(), 2 * std::mem::size_of::<usize>());
    }

    #[test]
    fn a_local_path_miss_is_a_usage_error_not_a_404() {
        let local = ErrorKind::PathNotFound {
            path: PathBuf::from("dist/fjo.tar.gz"),
            what: "release asset",
        };
        let remote = ErrorKind::ResourceNotFound {
            kind: "release",
            id: "v1".into(),
            slug: None,
            server_message: None,
        };
        assert_eq!(local.exit_code(), 2);
        assert_eq!(remote.exit_code(), 5);
    }

    /// Matches `gh pr checks`, and the overlap with `RateLimited` is the documented intent —
    /// both mean "wait, then try again".
    #[test]
    fn pending_checks_exit_8_like_gh() {
        let pending = ErrorKind::ChecksPending { slug: None, pr: "42".into(), pending: vec![] };
        assert_eq!(pending.exit_code(), 8);
        assert_eq!(ErrorKind::RateLimited { host: "h".into(), retry_after: None }.exit_code(), 8);
    }

    #[test]
    fn the_new_failure_variants_exit_1() {
        for kind in [
            ErrorKind::StateConflict {
                resource: Some("pull request 42".into()),
                state: Some("closed".into()),
                server_message: "the pull request is closed".into(),
            },
            ErrorKind::RunFailed {
                slug: None,
                run: "12".into(),
                conclusion: "failure".into(),
                failed_jobs: vec![],
                url: None,
            },
            ErrorKind::GitFailed {
                command: "git push".into(),
                stderr: "permission denied".into(),
                status: Some(128),
            },
        ] {
            assert_eq!(kind.exit_code(), 1, "{kind:?}");
        }
    }

    /// Both are settled failures of something the user asked for, so they share the 1 bucket —
    /// and neither is a usage error, which is the distinction the doc comment argues.
    #[test]
    fn a_refused_push_and_a_red_check_are_failures_not_usage_errors() {
        let agit = ErrorKind::AgitRefused {
            refspec: "HEAD:refs/for/main/fix-parser".into(),
            remedy: AgitRemedy::ForcePush,
            stderr: "! [remote rejected] HEAD -> refs/for/main/fix-parser (non-fast-forward)"
                .into(),
        };
        let checks = ErrorKind::ChecksFailed {
            slug: Some("perf3ct/fjo".into()),
            pr: "4212".into(),
            failed: vec![FailedCheck::new("build", None)],
        };
        assert_eq!(agit.exit_code(), 1, "a server refusal is not a bad command line");
        assert_eq!(checks.exit_code(), 1, "gh pr checks exits 1 for a failed check");
        // And specifically *not* the codes each was most at risk of collapsing into.
        assert_ne!(agit.exit_code(), ErrorKind::Usage(String::new()).exit_code());
        assert_ne!(
            checks.exit_code(),
            ErrorKind::ChecksPending { slug: None, pr: "4212".into(), pending: vec![] }.exit_code(),
            "failed and pending must not be the same code: one waits, the other does not"
        );
        assert!(!agit.is_transient() && !checks.is_transient());
    }

    /// The order of the tests in `from_stderr` is the whole subtlety: a push-options refusal also
    /// says "rejected", so the general test must not run first.
    #[test]
    fn an_agit_refusal_is_classified_from_gits_own_words() {
        assert_eq!(
            AgitRemedy::from_stderr(
                "! [remote rejected] HEAD -> refs/for/main/t (non-fast-forward)"
            ),
            AgitRemedy::ForcePush
        );
        assert_eq!(
            AgitRemedy::from_stderr("fatal: the receiving end does not support push options"),
            AgitRemedy::PushOptionsDisabled
        );
        assert_eq!(
            AgitRemedy::from_stderr(
                "remote: rejected: the receiving end does not support push options"
            ),
            AgitRemedy::PushOptionsDisabled,
            "a push-options refusal that also says rejected must not read as an amended history"
        );
        assert_eq!(
            AgitRemedy::from_stderr("remote: Forgejo: topic is required"),
            AgitRemedy::TopicRequired
        );
        // Nothing recognised is its own answer, not a guess at the most likely cause.
        assert_eq!(AgitRemedy::from_stderr("something nobody predicted"), AgitRemedy::Unrecognised);
    }

    /// A check with no `target_url` must carry `None`, not `Some("")` — the renderer decides
    /// whether to offer "open the URL above" on exactly that.
    #[test]
    fn a_check_without_a_url_carries_none() {
        assert_eq!(FailedCheck::new("build", Some("  ".into())).url, None);
        assert_eq!(FailedCheck::new("build", None).url, None);
        assert_eq!(
            FailedCheck::new("build", Some("https://ci/1".into())).url.as_deref(),
            Some("https://ci/1")
        );
    }

    /// None of the new variants may be retried by the transport: they describe a settled
    /// outcome, not a flaky one.
    #[test]
    fn the_new_variants_are_not_transient() {
        assert!(
            !ErrorKind::ChecksPending { slug: None, pr: "1".into(), pending: vec![] }
                .is_transient()
        );
        assert!(
            !ErrorKind::StateConflict {
                resource: None,
                state: None,
                server_message: String::new()
            }
            .is_transient()
        );
        // Especially this one: a retry re-walks the same non-terminating collection.
        assert!(
            !ErrorKind::PaginationDidNotTerminate { pages: 10_000, items: 500_000 }.is_transient()
        );
    }

    /// A runaway walk is the instance misbehaving, not the command line being wrong, so it
    /// shares a bucket with `ServerError` rather than with `Usage`.
    #[test]
    fn a_runaway_walk_is_a_server_problem() {
        assert_eq!(
            ErrorKind::PaginationDidNotTerminate { pages: 10_000, items: 500_000 }.exit_code(),
            7
        );
    }
}

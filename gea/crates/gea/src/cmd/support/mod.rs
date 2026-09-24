//! The plumbing every layer-3 group shares: field tables, the `--json`/`--jq`/`--template`
//! triad, pagination caps, prompting, and the stderr note.
//!
//! # Why this module exists
//!
//! It did not, for a while, and the reason is worth recording because it shaped six other
//! files. `cmd/mod.rs` was frozen while several groups were built out in parallel: a module can
//! only be declared from a file that already exists, so no wave could add a neutral
//! `cmd/support.rs` without colliding with the others. Each wave grew a private copy instead —
//! `cmd/admin/support.rs`, `cmd/run/emit.rs`, `cmd/repo/shared.rs`, `cmd/auth/common.rs`,
//! `cmd/issue/shared.rs` and `cmd/times/porcelain.rs` — and one of them was then reached
//! *across* into, so eleven command groups imported `crate::cmd::admin::support` for
//! general-purpose output plumbing: a dependency graph claiming repository topics depend on
//! instance administration.
//!
//! The copies had already begun to disagree. Six adapters from the generated field tables
//! described the same `Vec<String>` field three different ways; two `split_editor_text`s each
//! mishandled a different half of what a Windows editor writes; two `label_ids` disagreed about
//! whether an organization's labels count.
//!
//! Everything genuinely common now lives here. What stayed behind stayed deliberately: a
//! helper that needs a `PullRequest`, an `Issue` or a `Label` is domain code, and moving it here
//! would relocate the coupling rather than remove it.
//!
//! # The rules it encodes
//!
//! 1. **`--json` field discovery answers before any HTTP request.** Bare `--json` prints the
//!    field list to stdout and exits 0 (`docs/output.md`, divergence 2), and it must work with
//!    no token, no network, and no configured host. Every entry point that resolves `--json`
//!    ([`emit::Json::resolve`], [`listing::discover`], [`machine::Triad`]'s callers) is called
//!    *before* the runtime is built.
//! 2. **A command never decides its own presentation.** The machine-versus-human fork lives in
//!    one of the facades here, so a group cannot accidentally print a table into a pipe or a
//!    banner into `head -1`.
//! 3. **Progress and advice go to stderr, and only for a terminal.** [`note`].
//! 4. **Prompting needs stdin *and* stdout to be real terminals**, per
//!    `docs/porcelain-conventions.md`. [`interact`].
//!
//! # Two facades, not one
//!
//! [`emit::Emit`] borrows a writer; [`listing::list`] opens `--output`'s destination itself.
//! They are not merged because the difference is real — `Emit` is what makes a command testable
//! without a `Runtime`, and `listing` is what lets a command render into a file. Both sit on the
//! same [`machine::Triad`] and the same [`fields`] adapter, which is where the duplication that
//! mattered actually was.

use std::io::Write;

use gitea_core::error::{Error, ErrorKind};
use gitea_core::types::RepoSlug;

use crate::output::Term;

pub mod editor;
pub mod emit;
pub mod fields;
pub mod interact;
pub mod listing;
pub mod machine;
pub mod page;
pub mod size;
#[cfg(test)]
pub mod testing;

pub use editor::{
    BodyOpts, TitleBody, edit, editor_seed, read_flagged, read_source, split_editor_text,
};
pub use emit::{Emit, Json};
pub use interact::{
    can_prompt, confirm_action, confirm_question, confirm_runtime, confirm_term, may_prompt,
};
pub use listing::{Fields, Listing};
pub use page::{
    DEFAULT_LIMIT, banner, drain, item_cap, limit, plural_s, total_if_truncated, truncation_banner,
};

/// A usage error: exit 2, and the message is the whole content.
pub fn usage(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage(message.into()))
}

/// A local "there is no such thing here" that reads like the server's own 404.
pub fn not_found(kind: &'static str, id: impl std::fmt::Display, slug: &RepoSlug) -> Error {
    Error::new(ErrorKind::ResourceNotFound {
        kind,
        id: id.to_string(),
        slug: Some(slug.to_string()),
        // Discovered locally: there was no server reply to quote.
        server_message: None,
    })
}

/// Open the destination `--output` selected, defaulting to stdout.
pub fn writer(globals: &crate::global::GlobalOpts) -> gitea_core::error::Result<Box<dyn Write>> {
    Ok(crate::output::open_dest(&crate::output::dest_for(globals.output.as_deref()))?)
}

/// The typed API surface over the runtime's client.
///
/// `Api` owns a `Client`, and `Client` is one `Arc`, so this is a refcount bump rather than a
/// second connection pool.
pub fn api(rt: &crate::runtime::Runtime) -> gitea_client::Api {
    gitea_client::Api::new(rt.client().clone())
}

/// A typed model as a `serde_json::Value`, for the machine path.
///
/// Infallible in practice — every generated model is a plain struct of JSON-representable
/// fields — but a `Decode` error is a far better report than an `unwrap` if that ever changes.
pub fn to_value<T: serde::Serialize + ?Sized>(
    value: &T,
) -> gitea_core::error::Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| {
        Error::new(ErrorKind::Decode {
            pointer: "/".to_owned(),
            expected: format!("a JSON-representable model: {e}"),
            body_excerpt: String::new(),
        })
    })
}

/// The error for a value that is required, missing, and cannot be asked for.
///
/// Naming the flag is the whole content of the message: "title is required" sends the reader to
/// the manual, "pass --title" does not.
pub fn missing(flag: &str, what: &str) -> Error {
    usage(format!("{what} is required; pass {flag} when prompting is unavailable"))
}

/// Resolve `@me` for any user-valued flag.
///
/// One resolver, deliberately: two of these is exactly how `-a @me` comes to mean two things.
pub async fn resolve_me(
    api: &gitea_client::Api,
    values: &[String],
) -> gitea_core::error::Result<Vec<String>> {
    if !values.iter().any(|v| v == "@me") {
        return Ok(values.to_vec());
    }
    let login = me(api).await?;
    Ok(values.iter().map(|v| if v == "@me" { login.clone() } else { v.clone() }).collect())
}

/// The authenticated user's login, for a default owner or an `@me` in a scalar position.
pub async fn me(api: &gitea_client::Api) -> gitea_core::error::Result<String> {
    Ok(api.user().get_current().await?.login)
}

/// Open a URL in the user's browser, honouring `gea config get browser` and `$BROWSER`.
///
/// **Detached**, so `gea pr view --web` returns immediately instead of blocking until the
/// browser exits — which for a `firefox` that was not already running is the difference between
/// a prompt coming back and a hung terminal.
pub fn open_web(rt: &crate::runtime::Runtime, url: &str) -> gitea_core::error::Result<()> {
    let browser =
        rt.config().resolved_browser(Some(rt.host().as_str()), &gitea_core::config::SystemEnv);
    open_url(rt.term(), browser, url)
}

/// [`open_web`] without a [`crate::runtime::Runtime`].
///
/// `auth login` needs exactly this and cannot have the other: `Runtime::new` fails with "no
/// Gitea host is set up yet", which is the very state `auth login` exists to leave. Rather
/// than let that command reach for `open::` directly and quietly lose the `browser` preference
/// and the non-TTY behaviour, both callers share this.
pub fn open_url(term: &Term, browser: Option<String>, url: &str) -> gitea_core::error::Result<()> {
    if !term.tty {
        // The URL is the answer when nobody is watching a browser: `gea pr view --web | cat`
        // should still tell you where it would have gone.
        println!("{url}");
        return Ok(());
    }
    eprintln!("Opening {url} in your browser.");
    let result = match browser {
        Some(b) => open::with_detached(url, b),
        None => open::that_detached(url),
    };
    result.map_err(|e| {
        usage(format!(
            "could not open a browser for {url}: {e}\nset one with `gea config set browser <cmd>`"
        ))
    })
}

/// The "nothing matched" note. **Emptiness is exit 0**, so this is a note and not an error.
pub fn empty_note(term: &Term, what: &str) {
    note(term, &format!("no {what} matched"));
}

/// A timestamp as `gh` shows one in a table: `about 2 hours ago`.
///
/// Routed through [`crate::output::template::funcs::timeago`] rather than reimplemented, so a
/// column and a `--template {{timeago .updated_at}}` agree on the words.
pub fn ago(ts: Option<&gitea_core::types::Timestamp>) -> String {
    match ts {
        // An unset timestamp is the zero value, which `timeago` would render as "55 years ago".
        Some(t) if !t.is_unset() => crate::output::template::funcs::timeago(&t.to_string()),
        _ => String::new(),
    }
}

/// A progress or status line. **stderr**, and only when the output terminal is real.
///
/// stdout is the machine channel: `gea topic add rust --json topics | jq` must not receive
/// prose. The TTY check is because a note in a log file nobody reads is just noise.
///
/// `writeln!` with the result discarded rather than `eprintln!`: `eprintln!` *panics* when the
/// write fails, so `gea issue list 2>&1 | head -1` would abort the process on `EPIPE` instead
/// of printing a table. A note is advisory; failing to deliver it is not worth a crash.
pub fn note(term: &Term, message: &str) {
    if term.tty {
        let _ = writeln!(std::io::stderr(), "{message}");
    }
}

/// Something the caller needs even when nobody is watching. **stderr, always.**
///
/// The difference from [`note`] is who the message is for. A note is advisory — a hint a human
/// might like and a script has no use for — so gating it on a TTY keeps logs free of noise, and
/// that reasoning is right for what `note` carries.
///
/// It is wrong for a message reporting that the command did *not* fully do what its output
/// implies, which is exactly what a script cannot infer and most needs. The case this was added
/// for: an AGit push is accepted, the pull request exists, and reading it back fails. Exit 0 with
/// an empty stdout is then indistinguishable from success to `URL=$(gea pr create --agit …)`,
/// and gating the only explanation on a TTY makes it a silent failure in precisely the situation
/// where it matters.
///
/// Same discarded `writeln!` as `note`, for the same reason: `eprintln!` panics on `EPIPE`.
pub fn warn(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}

//! `fjo auth export --web` and `fjo auth import --web` — moving a session between machines.
//!
//! # Why the remember token and not the session cookie
//!
//! The session lapses in about a day (`SESSION_LIFE_TIME`, 86400 by default). Exporting that
//! alone gives a CI job that works this afternoon and fails tomorrow — the worst failure shape,
//! because it looks like a regression in whatever changed most recently rather than an expiry.
//! The whole document goes, and the receiving machine mints its own sessions from the remember
//! token for the rest of its ~31 days.
//!
//! # Why only `--web`
//!
//! API tokens are deliberately not exportable. `FJO_TOKEN` with a scoped token created in the
//! web UI is already the better CI story, and an exported one would lose the `scopes` recorded
//! on the login — which is the only reason an `InsufficientScope` error can say what the token
//! actually has instead of `unknown`.
//!
//! # The two guards, and what they are protecting against
//!
//! This value authenticates the **whole account** for about a month and cannot be scoped the way
//! a token can. So export refuses a terminal unless forced — a full-account credential scrolling
//! into scrollback is how one ends up in a screen recording — and its warning goes to stderr,
//! never stdout, so `| gh secret set` stays a clean pipe.

use clap::Args as ClapArgs;
use forgejo_core::config::{HostKey, Slot};
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::web::{WebClient, WebCredential, session};

use crate::cmd::auth::common::{self, Setup, warn};
use crate::cmd::auth::web_password;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
pub struct ExportArgs {
    /// Export the web session (the only kind that can be moved)
    #[arg(long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ImportArgs {
    /// Import a web session document, read from stdin
    #[arg(long)]
    pub web: bool,
}

pub fn export(globals: &GlobalOpts, args: &ExportArgs) -> Result<()> {
    require_web(args.web, "export")?;
    let mut setup = Setup::load()?;
    let (key, login) = resolve(&mut setup, globals)?;

    let mut creds = setup.credentials(Some(&key));
    let found = creds.secret(&mut setup.hosts, &key, &login, Slot::Web)?;
    for kind in creds.take_warnings() {
        warn(&kind);
    }
    let cred = found
        .as_ref()
        .and_then(|t| WebCredential::parse(t.expose()))
        .ok_or_else(|| Error::new(ErrorKind::WebSessionMissing { host: key.to_string() }))?;

    // A terminal is not a destination for this. `--force` is the flag that already means
    // "yes, really write that to my terminal" everywhere else in the tool.
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) && !globals.force {
        return Err(Error::new(ErrorKind::Usage(format!(
            "refusing to print a web session to the terminal: it authenticates the whole {} \
             account until {}, and cannot be scoped. Pipe it (`| gh secret set FJO_WEB_SESSION`), \
             redirect it, or pass --force if you really want it on screen.",
            cred.user,
            cred.remember_expires_at.strftime("%Y-%m-%d")
        ))));
    }

    // stderr, so stdout stays a clean pipe.
    eprintln!(
        "warning: this is a full-account credential for {} on {}, valid until {}. It cannot be \
         scoped like a token; prefer a dedicated account for CI.",
        cred.user,
        key,
        cred.remember_expires_at.strftime("%Y-%m-%d")
    );
    println!("{}", cred.to_export_json()?);
    Ok(())
}

pub fn import(globals: &GlobalOpts, args: &ImportArgs) -> Result<()> {
    require_web(args.web, "import")?;
    let mut setup = Setup::load()?;
    let (key, _) = resolve(&mut setup, globals)?;
    let url = setup.hosts.get(&key).map(|e| e.url.clone()).ok_or_else(|| {
        Error::new(ErrorKind::UnknownHost { given: key.to_string(), known: setup.hosts.known() })
    })?;

    let mut raw = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut raw)
        .map_err(|e| usage(format!("could not read the session from stdin: {e}")))?;
    let cred = WebCredential::parse(raw.trim()).ok_or_else(|| {
        usage(
            "that does not look like a session document. Produce one with \
             `fjo auth export --web` on a machine that is signed in.",
        )
    })?;

    // Verify before storing, the same contract login and import share: filing away a credential
    // that cannot mint a session moves the failure somewhere with less context.
    let client = WebClient::new(&url, &crate::runtime::user_agent())?;
    let cred = crate::runtime::block_on_value(session::renew(&client, cred))?;

    web_password::store(&mut setup, &key, &cred)?;
    println!(
        "✓ web session for {} on {key} imported; it lapses on {}",
        cred.user,
        cred.remember_expires_at.strftime("%Y-%m-%d")
    );
    Ok(())
}

/// `--web` is required rather than defaulted, so that a future second kind cannot silently
/// change what a bare `fjo auth export` means in someone's script.
fn require_web(web: bool, verb: &str) -> Result<()> {
    if web {
        return Ok(());
    }
    Err(usage(format!(
        "say what to {verb}: `fjo auth {verb} --web`. API tokens are not transferable this way; \
         use a scoped token in FJO_TOKEN for CI instead."
    )))
}

/// Host and login exactly as every other `auth` subcommand resolves them, so that
/// `--host`/`--login` mean here what they mean in `fjo auth token`.
fn resolve(setup: &mut Setup, globals: &GlobalOpts) -> Result<(HostKey, String)> {
    let key = setup.hosts.resolve_host(globals.host.as_deref(), common::env())?;
    let login = setup.hosts.resolve_login(&key, globals.login.as_deref())?;
    Ok((key, login))
}

fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage(msg.into()))
}

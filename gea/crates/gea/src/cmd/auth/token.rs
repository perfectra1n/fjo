//! `gea auth token` — the deliberate, single exit through which a token becomes plaintext.
//!
//! `auth status` will not print a token under any flag. This command will, because a script
//! genuinely needs one (`curl -H "Authorization: token $(gea auth token)"`, a `docker login`,
//! a `git` remote helper). Concentrating that into one obviously-named command is the point: it
//! makes "where can a token leak from?" answerable, and it lets *this* command warn when the
//! destination is a terminal, where the secret lands in scrollback and in any recording.

use std::io::Write;

use clap::Args as ClapArgs;
use gitea_core::error::{Error, ErrorKind, Result};

use super::common::{self, Setup};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {}

const LONG_HELP: &str = "\
Print the token for a host. Treat this output as a secret.

On a terminal, a warning is printed first because the token may remain in scrollback
or recordings. Piped output has no trailing newline.

  curl -H \"Authorization: token $(gea auth token --host git.example.org)\" ...";

pub fn run(globals: &GlobalOpts, _args: &Args) -> Result<()> {
    let mut setup = Setup::load()?;
    let key = setup.hosts.resolve_host(globals.host.as_deref(), common::env())?;
    let login = setup.hosts.resolve_login(&key, globals.login.as_deref())?;

    let mut creds = setup.credentials(Some(&key));
    let token = creds.token(&mut setup.hosts, &key, &login)?;
    for kind in creds.take_warnings() {
        common::warn(&kind);
    }
    if let Err(e) = setup.hosts.save_if_dirty() {
        common::warn(&e.kind);
    }

    let Some(token) = token else {
        return Err(Error::new(ErrorKind::NotAuthenticated { host: key.to_string() }));
    };

    // The bug this prevents: printing the stored value verbatim. For an OAuth session that
    // value is a document containing *both* tokens, and the refresh token is the one that must
    // never leave this machine — it is the session, where the access token is an hour of it.
    let credential = common::Credential::new(token);

    let term = Term::detect();
    support::note(
        &term,
        "warning: this token is now in your terminal's scrollback; `gea auth token | pbcopy` \
         or a pipe keeps it out of your history",
    );
    if credential.session().is_some() {
        support::note(
            &term,
            "note: this is an OAuth access token and expires within the hour; for a script or \
             a CI job, create a token in the web UI instead",
        );
    }

    let mut out = support::writer(globals)?;
    // A newline for a human, none for a pipe. `$(gea auth token)` strips a trailing newline
    // anyway, but `read -r -N` and a `curl --config -` do not, and a stray byte in an
    // `Authorization` header is a 401 nobody can explain.
    if term.tty {
        writeln!(out, "{}", credential.expose())?;
    } else {
        out.write_all(credential.expose().as_bytes())?;
    }
    out.flush()?;
    Ok(())
}

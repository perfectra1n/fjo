//! `gea auth logout` — forget a credential, from wherever it is.

use std::io::Write;

use clap::Args as ClapArgs;
use gitea_core::Result;

use super::common::{self, Setup};
use crate::cmd::support;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

const LONG_HELP: &str = "\
Remove a saved login and its stored token.

Environment tokens cannot be removed by gea. Unset GEA_TOKEN or GITEA_TOKEN
in your shell if needed.

  gea auth logout --host git.example.org
  gea auth logout --host git.example.org --login ci-bot --yes";

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let mut setup = Setup::load()?;
    let key = setup.hosts.resolve_host(globals.host.as_deref(), common::env())?;
    let login = setup.hosts.resolve_login(&key, globals.login.as_deref())?;

    support::confirm_question(
        support::interact::may_prompt_for(&setup.config, Some(key.as_str())),
        args.yes,
        &format!("Log {login} out of {key}?"),
    )?;

    let mut creds = setup.credentials(Some(&key));
    // `forget` before `remove_login`, not after: the file store finds the token *through* the
    // login entry, so deleting the entry first would leave the token behind in `hosts.toml` —
    // a logout that leaves the secret on disk.
    creds.forget(&mut setup.hosts, &key, &login)?;
    let removed = setup.hosts.remove_login(&key, &login)?;

    // A host with no logins left is not a host anyone is authenticated to. Keeping the shell of
    // one means `resolve_host` can still pick it and every command then fails with an
    // authentication error instead of `no Gitea host is set up yet`.
    let host_gone = setup.hosts.get(&key).is_some_and(|h| h.logins.is_empty());
    if host_gone {
        setup.hosts.remove_host(&key);
    }
    setup.hosts.save_if_dirty()?;

    for kind in creds.take_warnings() {
        common::warn(&kind);
    }

    if gitea_core::config::secrets::TOKEN_VARS.iter().any(|v| common::env().get(v).is_some()) {
        common::warn(&gitea_core::ErrorKind::Usage(format!(
            "a token is still exported in your environment ({}), and no process can unset a \
             variable in its parent shell; unset it yourself or gea will keep using it",
            gitea_core::config::secrets::TOKEN_VARS.join(" or ")
        )));
    }

    let mut out = support::writer(globals)?;
    if removed {
        writeln!(out, "Logged {login} out of {key}")?;
    } else {
        // Idempotent on purpose: `auth logout` in a teardown script must not fail because an
        // earlier run already did the job.
        writeln!(out, "No login {login} was recorded for {key}; nothing to remove")?;
    }
    if host_gone {
        writeln!(out, "{key} had no other logins and was removed from hosts.toml")?;
    }
    out.flush()?;
    Ok(())
}

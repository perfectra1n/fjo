//! `gea auth switch` — change which host and login every other command defaults to.

use std::io::Write;

use clap::Args as ClapArgs;
use gitea_core::Result;
use gitea_core::config::HostKey;

use super::common::{self, Setup};
use crate::cmd::support;
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {}

const LONG_HELP: &str = "\
Change the active host, login, or both.

Prompts when several choices are available. Without a terminal, specify --host
or --login to resolve an ambiguous choice. The updated hosts.toml uses 0600 permissions.

  gea auth switch                                  # pick interactively
  gea auth switch --host git.example.org
  gea auth switch --login ci-bot
  gea auth switch --host git.example.org --login ci-bot";

pub fn run(globals: &GlobalOpts, _args: &Args) -> Result<()> {
    let mut setup = Setup::load()?;

    let hosts = setup.hosts.known();
    if hosts.is_empty() {
        return Err(gitea_core::Error::new(gitea_core::ErrorKind::NoHostConfigured));
    }

    // An explicit --host still goes through `resolve_host` so a typo gets `UnknownHost`, whose
    // message lists the hosts that do exist.
    let key = match globals.host.as_deref() {
        Some(h) => setup.hosts.resolve_host(Some(h), common::env())?,
        None => {
            let chosen = support::interact::choose(
                support::interact::may_prompt_for(&setup.config, None),
                "Which host?",
                "--host",
                hosts,
            )?;
            HostKey::parse(&chosen)?
        }
    };

    let logins: Vec<String> = setup
        .hosts
        .get(&key)
        .map(|h| h.logins.iter().map(|l| l.user.clone()).collect())
        .unwrap_or_default();
    if logins.is_empty() {
        return Err(support::usage(format!(
            "no logins are recorded for {key}; run `gea auth login --host {key}` first"
        )));
    }

    let login = match globals.login.as_deref() {
        Some(u) => setup.hosts.resolve_login(&key, Some(u))?,
        None => support::interact::choose(
            support::interact::may_prompt_for(&setup.config, Some(key.as_str())),
            &format!("Which login on {key}?"),
            "--login",
            logins,
        )?,
    };

    setup.hosts.set_active(&key)?;
    setup.hosts.select_login(&key, &login)?;
    setup.hosts.save_if_dirty()?;

    let mut out = support::writer(globals)?;
    writeln!(out, "Active account is now {login} on {key}")?;
    out.flush()?;
    Ok(())
}

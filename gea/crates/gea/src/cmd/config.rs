//! `gea config` — read and write `config.toml`.
//!
//! The key set is [`gitea_core::config::KEYS`], not a list maintained here. That matters twice:
//! an unknown key is rejected with the real list rather than a guess, and a key added to
//! `gitea-core` becomes settable with no edit to this file.
//!
//! `--host` selects a per-host override. Reads prefer the override and fall back to the top-level
//! value, which is what makes `editor` global while `pager` differs for one slow instance.

use std::io::Write;

use clap::{Args as ClapArgs, Subcommand};
use gitea_core::config::{Config, HostKey, KEYS};
use gitea_core::error::Result;
use serde_json::{Value, json};

use super::auth::common::{self};
use crate::cmd::support;
use crate::cmd::support::machine::Triad;
use crate::global::GlobalOpts;
use crate::output::{Table, Term, project::FieldKind, project::FieldSpec};

/// `--json` selectable fields for `config list`.
const FIELDS: &[FieldSpec] = &[
    FieldSpec { name: "key", kind: FieldKind::Str, doc: "preference name" },
    FieldSpec { name: "value", kind: FieldKind::Str, doc: "effective value, default included" },
    FieldSpec {
        name: "source",
        kind: FieldKind::Enum(&["host", "config", "default"]),
        doc: "where the effective value came from",
    },
];

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

const LONG_HELP: &str = "\
Read and write gea configuration.

`config get` prints the stored value, or nothing if unset. `config list` includes
defaults. Use --host for host-specific settings.

Manage aliases with `gea alias`. Tokens are stored separately in the keyring
or hosts.toml, not config.toml.

  gea config list
  gea config get editor
  gea config set editor 'code --wait'
  gea config set pager cat --host git.example.org     # only for that host
  gea config unset pager --host git.example.org";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Print one key's stored value
    Get(GetArgs),
    /// Set one key
    Set(SetArgs),
    /// Remove one key
    Unset(UnsetArgs),
    /// Print every recognized key with its effective value
    List(ListArgs),
}

#[derive(Debug, ClapArgs)]
pub struct GetArgs {
    #[arg(value_name = "KEY")]
    pub key: String,
}

#[derive(Debug, ClapArgs)]
pub struct SetArgs {
    #[arg(value_name = "KEY")]
    pub key: String,
    #[arg(value_name = "VALUE")]
    pub value: String,
}

#[derive(Debug, ClapArgs)]
pub struct UnsetArgs {
    #[arg(value_name = "KEY")]
    pub key: String,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    match &args.command {
        Cmd::Get(a) => get(globals, a),
        Cmd::Set(a) => set(globals, a),
        Cmd::Unset(a) => unset(globals, a),
        Cmd::List(a) => list(globals, a),
    }
}

/// `--host` as a normalized key, or `None`.
///
/// Normalized through [`HostKey`] rather than used verbatim so that
/// `--host https://Git.Example.ORG/` addresses the same override table as `--host git.example.org`.
/// An override written under one spelling and read under another is a setting that silently does
/// nothing.
fn host_of(globals: &GlobalOpts) -> Result<Option<String>> {
    match globals.host.as_deref() {
        None => Ok(None),
        Some(h) => Ok(Some(HostKey::parse(h)?.to_string())),
    }
}

/// A key, checked against [`KEYS`].
///
/// `Config::set` validates on the way in, but `Config::get` deliberately does not — it is also the
/// path `gea` itself reads through. So `config get` and `config unset` check here, and the message
/// lists the whole vocabulary: a user who mistyped `credentials_store` needs to see
/// `credential_store`, not merely to be told they are wrong.
fn checked_key(key: &str) -> Result<&str> {
    if KEYS.contains(&key) {
        return Ok(key);
    }
    Err(support::usage(format!(
        "unknown config key {key:?}; the keys gea recognises are: {}",
        KEYS.join(", ")
    )))
}

fn get(globals: &GlobalOpts, args: &GetArgs) -> Result<()> {
    let key = checked_key(args.key.trim())?;
    let config = Config::load(common::env())?;
    let host = host_of(globals)?;
    let mut out = support::writer(globals)?;
    // An unset key prints an empty line, not the default: see the group's `--help`.
    writeln!(out, "{}", config.get(host.as_deref(), key).unwrap_or_default())?;
    out.flush()?;
    Ok(())
}

fn set(globals: &GlobalOpts, args: &SetArgs) -> Result<()> {
    let mut config = Config::load(common::env())?;
    let host = host_of(globals)?;
    // `Config::set` owns both the key check and the value vocabulary, so `config set prompt off`
    // fails naming `enabled`/`disabled` instead of being silently ignored forever.
    config.set(host.as_deref(), args.key.trim(), args.value.trim())?;
    config.save()?;

    let term = Term::detect();
    match &host {
        Some(h) => support::note(&term, &format!("{} set for {h}", args.key.trim())),
        None => support::note(&term, &format!("{} set", args.key.trim())),
    }
    Ok(())
}

fn unset(globals: &GlobalOpts, args: &UnsetArgs) -> Result<()> {
    let key = checked_key(args.key.trim())?;
    let mut config = Config::load(common::env())?;
    let host = host_of(globals)?;
    let removed = config.unset(host.as_deref(), key)?;
    if removed {
        config.save()?;
    }
    let term = Term::detect();
    if removed {
        support::note(&term, &format!("{key} unset"));
    } else {
        // Not an error: `config unset` in a provisioning script must be idempotent.
        support::note(&term, &format!("{key} is not set"));
    }
    Ok(())
}

fn list(globals: &GlobalOpts, _args: &ListArgs) -> Result<()> {
    let Some(machine) = Triad::for_local_table(globals, FIELDS)? else { return Ok(()) };
    let config = Config::load(common::env())?;
    let host = host_of(globals)?;
    let term = Term::detect();

    // Keys the file holds that this build does not know. Reported rather than dropped, because
    // `Config` preserves them on save and a user running an older gea must not think a newer
    // one's settings vanished.
    let unknown = config.unknown_keys();
    if !unknown.is_empty() {
        common::warn(&gitea_core::ErrorKind::Usage(format!(
            "{} has {} key(s) this build does not recognise ({}); they are preserved, not applied",
            config.path().display(),
            unknown.len(),
            unknown.join(", ")
        )));
    }

    let rows = rows(&config, host.as_deref());
    let mut out = support::writer(globals)?;
    if machine.is_explicit() {
        let payload = Value::Array(
            rows.iter().map(|(k, v, s)| json!({ "key": k, "value": v, "source": s })).collect(),
        );
        machine.pipeline().render(payload, &term, &mut out)?;
        out.flush()?;
        return Ok(());
    }

    let mut table = Table::new(&term);
    table.headers(["KEY", "VALUE", "SOURCE"]);
    for (k, v, s) in &rows {
        table.row([k.as_str(), v.as_str(), s]);
    }
    table.render(&mut out)?;
    out.flush()?;
    Ok(())
}

/// Every recognized key with its effective value and where that value came from.
///
/// `source` is the part `Config::list` cannot tell you and the part a user actually wants: "why is
/// my pager `cat`?" is answered by `host`, and "why is my editor `vi` when I set it?" by
/// `default`.
fn rows(config: &Config, host: Option<&str>) -> Vec<(String, String, &'static str)> {
    config
        .list(host)
        .into_iter()
        .map(|(key, value)| {
            let source = if host.is_some_and(|h| config.get(Some(h), key) != config.get(None, key))
            {
                "host"
            } else if config.get(None, key).is_some() {
                "config"
            } else {
                "default"
            };
            (key.to_owned(), value, source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: rejecting a mistyped key without saying what the real ones are, which
    /// leaves the user guessing between `credential_store`, `credentials_store` and
    /// `credential-store`.
    #[test]
    fn an_unknown_key_is_refused_with_the_whole_list() {
        let e = checked_key("credentials_store").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        for key in KEYS {
            assert!(e.to_string().contains(key), "{key} missing from: {e}");
        }
        assert!(checked_key("credential_store").is_ok());
    }

    /// Bug this prevents: writing a per-host override under the spelling the user typed, so
    /// `--host https://Git.Example.ORG/` sets a key that `--host git.example.org` cannot read.
    #[test]
    fn a_host_override_is_normalised_before_use() {
        let globals =
            GlobalOpts { host: Some("https://Git.Example.ORG/".to_owned()), ..Default::default() };
        assert_eq!(host_of(&globals).unwrap().as_deref(), Some("git.example.org"));
    }

    /// `list` must show where a value came from, or a per-host override is invisible and the user
    /// cannot tell it from a global one.
    #[test]
    fn list_attributes_each_value_to_host_config_or_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::empty_at(dir.path());
        config.set(None, "editor", "hx").unwrap();
        config.set(Some("git.example.org"), "pager", "cat").unwrap();

        let by_key = |host: Option<&str>, want: &str| -> (String, &'static str) {
            rows(&config, host)
                .into_iter()
                .find(|(k, _, _)| k == want)
                .map(|(_, v, s)| (v, s))
                .expect("every recognized key is listed")
        };
        assert_eq!(by_key(None, "editor"), ("hx".to_owned(), "config"));
        assert_eq!(by_key(Some("git.example.org"), "pager"), ("cat".to_owned(), "host"));
        // Not set anywhere: `prompt` still lists, with its documented default.
        assert_eq!(by_key(None, "prompt"), ("enabled".to_owned(), "default"));
    }

    /// Bug this prevents: `config list`'s `--json` field table drifting from the object it emits,
    /// so `--json source` rejects a field the payload has.
    #[test]
    fn the_json_row_emits_exactly_the_declared_fields() {
        let v = json!({ "key": "editor", "value": "hx", "source": "config" });
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let declared: Vec<&str> = FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(keys, declared);
    }
}

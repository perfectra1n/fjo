//! On-disk configuration: preferences ([`Config`]), identity ([`hosts::Hosts`]), and
//! credentials ([`secrets`]).
//!
//! Two files, deliberately separate, following `gh`:
//!
//! * `config.toml` — preferences. Non-secret, safe to commit to a dotfiles repo, safe to
//!   print in a bug report.
//! * `hosts.toml` — identity, and *possibly* tokens when the file credential store is in
//!   use. Mode 0600, never printed.
//!
//! Keeping them apart means `fjo config list` can be pasted into an issue without a
//! redaction pass, which is the whole reason `gh` splits them too.

// `crate::Error` is 168 bytes: `kind` is boxed, but `RequestCtx` is inline and holds seven
// owned fields. Clippy's 128-byte threshold therefore fires on every fallible function in
// the crate. The right fix is to shrink `Error` in `error/mod.rs` (box `RequestCtx`, or move
// it behind the boxed kind); until then, allowing it here beats sprinkling per-function
// attributes over a whole module tree.
#![allow(clippy::result_large_err)]

pub mod hosts;
pub mod secrets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

pub use hosts::{HostEntry, HostKey, Hosts, Login};
pub use secrets::{CredStore, CredentialStore, Credentials, Slot, Token};

/// The preferences file, relative to the config directory.
pub const CONFIG_FILE: &str = "config.toml";
/// The identity file, relative to the config directory.
pub const HOSTS_FILE: &str = "hosts.toml";

// ---------------------------------------------------------------------------- environment

/// Read-only access to the process environment.
///
/// This is a trait rather than direct [`std::env::var`] calls for one hard reason: in Rust
/// 2024 `std::env::set_var` is `unsafe`, because mutating the environment races with any
/// other thread reading it. Tests that need `FJO_TOKEN` or `FORGEJO_REPO` set therefore
/// cannot set them for real without being both unsafe and order-dependent under
/// `cargo test`'s thread pool. Injecting a [`MapEnv`] instead makes every test hermetic and
/// parallel-safe.
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    /// An empty variable counts as unset, matching `gh`. `FORGEJO_TOKEN=` in a CI script is
    /// a variable someone forgot to populate, not a request to authenticate with the empty
    /// string — and treating it as set produces a 401 whose cause is invisible.
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// A fixed environment for tests.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(BTreeMap<String, String>);

impl MapEnv {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.0.insert(key.to_owned(), value.to_owned());
        self
    }
}

impl Env for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|v| !v.is_empty()).cloned()
    }
}

// --------------------------------------------------------------------------------- paths

/// Where `config.toml` and `hosts.toml` live.
///
/// Resolution order — `$FJO_CONFIG_DIR`, then `$XDG_CONFIG_HOME/fjo`, then the platform
/// default via `etcetera`'s *base* strategy. The base strategy (not the native one) is
/// correct for a CLI: on macOS it yields `~/.config` rather than
/// `~/Library/Application Support`, which is what every other command-line tool a user has
/// installed does, and what `gh` does.
///
/// `$XDG_CONFIG_HOME` is honoured explicitly rather than left to `etcetera` so that it also
/// works on Windows, where someone running under MSYS/Git-Bash may well have set it.
pub fn config_dir(env: &dyn Env) -> Result<PathBuf> {
    if let Some(dir) = env.get("FJO_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = env.get("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("fjo"));
    }
    let base = etcetera::choose_base_strategy().map_err(|e| {
        Error::new(ErrorKind::Usage(format!(
            "cannot locate a configuration directory ({e}); set FJO_CONFIG_DIR"
        )))
    })?;
    Ok(etcetera::BaseStrategy::config_dir(&base).join("fjo"))
}

/// Writes `contents` to `path` via a same-directory temporary file plus `rename`.
///
/// The rename is what makes this safe: a crash or a full disk mid-write leaves the previous
/// file intact rather than a truncated `hosts.toml`, which would look exactly like "you are
/// logged out". `mode` is applied to the temporary file *before* any bytes are written, so a
/// 0600 file is never briefly world-readable — creating it 0644 and chmod-ing afterwards
/// leaves a window in which another local user can read the token.
pub(crate) fn write_atomic(path: &Path, contents: &str, mode: Option<u32>) -> Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("fjo.tmp");
    let tmp = dir.join(format!(".{}.{}.tmp", file_name, std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if let Some(m) = mode {
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, m);
    }
    #[cfg(not(unix))]
    let _ = mode;

    {
        let mut f = opts.open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ------------------------------------------------------------------------------ prefs

/// Whether interactive prompting is allowed.
///
/// A separate setting from "is stdout a TTY" on purpose: a user may be at a terminal and
/// still want every command to fail rather than block waiting for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prompt {
    #[default]
    Enabled,
    Disabled,
}

/// When to emit ANSI colour. `NO_COLOR` / `CLICOLOR_FORCE` are handled by the output layer;
/// this is the persisted preference it consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPref {
    #[default]
    Auto,
    Always,
    Never,
}

macro_rules! str_enum {
    ($ty:ty, $( $variant:ident => $text:literal ),+ $(,)?) => {
        impl $ty {
            pub const fn as_str(self) -> &'static str {
                match self { $( <$ty>::$variant => $text ),+ }
            }
            /// The accepted values, in the order `--help` should list them.
            pub const VALUES: &'static [&'static str] = &[ $( $text ),+ ];
        }
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::str::FromStr for $ty {
            type Err = ();
            fn from_str(s: &str) -> std::result::Result<Self, ()> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $( $text => Ok(<$ty>::$variant), )+
                    _ => Err(()),
                }
            }
        }
    };
}

str_enum!(Prompt, Enabled => "enabled", Disabled => "disabled");
str_enum!(ColorPref, Auto => "auto", Always => "always", Never => "never");

/// Recognized preference keys, following `gh`'s vocabulary wherever `gh` has one.
///
/// `aliases` is deliberately absent: it is a table, not a scalar, and is reached through
/// [`Config::aliases`] / [`Config::set_alias`] instead of `config set`.
pub const KEYS: &[&str] =
    &["browser", "color", "credential_store", "editor", "oauth_client_id", "pager", "prompt"];

// ------------------------------------------------------------------------------- Config

/// Preferences, backed by `config.toml`.
///
/// Held as a `toml::Table` rather than a typed struct so that **keys we do not recognise
/// survive a round-trip**. A user on a newer `fjo` who runs an older one — a stale copy in
/// `~/.local/bin`, a distro package a release behind — must not have their settings silently
/// deleted by the next `fjo config set`. Unknown keys are reported by
/// [`Config::unknown_keys`] so the caller can warn instead.
///
/// The cost of this choice is that comments and formatting are not preserved (that would
/// need `toml_edit`). Documented, and cheap next to eating someone's config.
#[derive(Debug, Clone)]
pub struct Config {
    dir: PathBuf,
    path: PathBuf,
    table: toml::Table,
}

impl Config {
    /// Loads `config.toml` from the resolved config directory.
    pub fn load(env: &dyn Env) -> Result<Self> {
        Self::load_from_dir(&config_dir(env)?)
    }

    /// Loads `config.toml` from an explicit directory.
    ///
    /// A missing file yields defaults and is **never** an error: a fresh install has no
    /// config, and `fjo --version` must not fail because of that. A file that exists but
    /// does not parse *is* an error — silently ignoring it would mean quietly discarding
    /// settings the user believes are in effect.
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE);
        let table = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<toml::Table>(&text).map_err(|e| {
                Error::new(ErrorKind::Usage(format!("{} is not valid TOML: {e}", path.display())))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { dir: dir.to_owned(), path, table })
    }

    /// An empty in-memory config rooted at `dir`. Used by tests and by `fjo config init`.
    pub fn empty_at(dir: &Path) -> Self {
        Self { dir: dir.to_owned(), path: dir.join(CONFIG_FILE), table: toml::Table::new() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The companion `hosts.toml` path, so callers never have to re-derive it.
    pub fn hosts_path(&self) -> PathBuf {
        self.dir.join(HOSTS_FILE)
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&self.table)
            .map_err(|e| Error::new(ErrorKind::Usage(format!("cannot serialize config: {e}"))))?;
        write_atomic(&self.path, &text, None)
    }

    // -------------------------------------------------------------- generic get/set/list

    /// Reads a key, preferring a `[hosts."<host>"]` override over the top-level value.
    ///
    /// Returns the *stored* value, not the default — `fjo config get pager` on a fresh
    /// install should print nothing rather than inventing `less`, so that a script can tell
    /// "unset" from "explicitly set to the default".
    pub fn get(&self, host: Option<&str>, key: &str) -> Option<String> {
        self.lookup(host, key).map(render_scalar)
    }

    /// Sets a top-level key, or a per-host override when `host` is given.
    ///
    /// Validates against [`KEYS`] and against each key's value vocabulary, so a typo like
    /// `fjo config set prompt off` fails immediately instead of being silently ignored
    /// until someone wonders why prompting still happens.
    pub fn set(&mut self, host: Option<&str>, key: &str, value: &str) -> Result<()> {
        let key = check_key(key)?;
        let value = check_value(key, value)?;
        match host {
            None => {
                self.table.insert(key.to_owned(), toml::Value::String(value));
            }
            Some(h) => {
                let hosts = self
                    .table
                    .entry("hosts".to_owned())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                let Some(hosts) = hosts.as_table_mut() else {
                    return Err(Error::new(ErrorKind::Usage(
                        "config.toml: `hosts` is not a table".into(),
                    )));
                };
                let entry = hosts
                    .entry(HostKey::parse(h)?.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                let Some(entry) = entry.as_table_mut() else {
                    return Err(Error::new(ErrorKind::Usage(format!(
                        "config.toml: `hosts.{h}` is not a table"
                    ))));
                };
                entry.insert(key.to_owned(), toml::Value::String(value));
            }
        }
        Ok(())
    }

    /// Removes a key. Returns whether anything was there.
    pub fn unset(&mut self, host: Option<&str>, key: &str) -> Result<bool> {
        let key = check_key(key)?;
        let Some(h) = host else {
            return Ok(self.table.remove(key).is_some());
        };
        let h = HostKey::parse(h)?.to_string();
        Ok(self
            .table
            .get_mut("hosts")
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.get_mut(&h))
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.remove(key))
            .is_some())
    }

    /// Every recognized key with its **effective** value — the stored value if present,
    /// otherwise the documented default. This is what `fjo config list` prints, and the
    /// reason it shows defaults is that "what is my editor?" is the actual question.
    pub fn list(&self, host: Option<&str>) -> Vec<(&'static str, String)> {
        KEYS.iter()
            .map(|&k| {
                let v = self.lookup(host, k).map(render_scalar).unwrap_or_else(|| default_for(k));
                (k, v)
            })
            .collect()
    }

    /// Keys present in the file that this build does not recognise. The caller warns; we do
    /// not fail, and [`Config::save`] preserves them.
    pub fn unknown_keys(&self) -> Vec<String> {
        self.table
            .keys()
            .filter(|k| k.as_str() != "aliases" && k.as_str() != "hosts")
            .filter(|k| !KEYS.contains(&k.as_str()))
            .cloned()
            .collect()
    }

    fn lookup(&self, host: Option<&str>, key: &str) -> Option<&toml::Value> {
        // Per-host override first. A parse failure on `host` is not worth an error here:
        // an unnormalizable host simply has no overrides.
        if let Some(h) = host
            && let Ok(k) = HostKey::parse(h)
            && let Some(v) = self
                .table
                .get("hosts")
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(&k.to_string()))
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(key))
        {
            return Some(v);
        }
        self.table.get(key)
    }

    // ------------------------------------------------------------------- typed getters

    /// The configured editor, if any. See [`Config::resolved_editor`] for the full chain.
    pub fn editor(&self, host: Option<&str>) -> Option<String> {
        self.string(host, "editor")
    }

    /// Editor to launch: config → `$VISUAL` → `$EDITOR` → `vi` (`notepad` on Windows).
    ///
    /// `git config core.editor` is deliberately *not* consulted here; that would make
    /// `Config` depend on [`crate::context::git`], and the binary can layer it in where it
    /// already has a `GitCtx`.
    pub fn resolved_editor(&self, host: Option<&str>, env: &dyn Env) -> String {
        self.editor(host)
            .or_else(|| env.get("VISUAL"))
            .or_else(|| env.get("EDITOR"))
            .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() })
    }

    pub fn pager(&self, host: Option<&str>) -> Option<String> {
        self.string(host, "pager")
    }

    /// Pager to launch: `$FJO_PAGER` → `$PAGER` → config → `less`.
    ///
    /// Environment before config is `gh`'s order and matters for one-off overrides
    /// (`FJO_PAGER=cat fjo pr list`). An empty `$PAGER` counts as unset, not as "no
    /// pager" — pass `--no-pager` for that.
    pub fn resolved_pager(&self, host: Option<&str>, env: &dyn Env) -> String {
        env.get("FJO_PAGER")
            .or_else(|| env.get("PAGER"))
            .or_else(|| self.pager(host))
            .unwrap_or_else(|| "less".into())
    }

    /// Default: [`Prompt::Enabled`]. An unrecognised stored value falls back to the default
    /// rather than erroring; [`Config::set`] already rejects bad values on the way in, so a
    /// bad value here means a hand-edited file, and refusing to run at all would be worse.
    pub fn prompt(&self, host: Option<&str>) -> Prompt {
        self.parsed(host, "prompt")
    }

    pub fn browser(&self, host: Option<&str>) -> Option<String> {
        self.string(host, "browser")
    }

    /// Browser to launch: config → `$BROWSER` → the platform's default handler (signalled by
    /// `None`, which the caller passes to the `open` crate).
    pub fn resolved_browser(&self, host: Option<&str>, env: &dyn Env) -> Option<String> {
        self.browser(host).or_else(|| env.get("BROWSER"))
    }

    /// Default: [`CredentialStore::Keyring`]. Note this is only the *preference*; the actual
    /// store is chosen by [`secrets::Credentials`], which also honours
    /// `FJO_CREDENTIAL_STORE`, `--insecure-storage`, and the cached per-host probe result.
    pub fn credential_store(&self, host: Option<&str>) -> CredentialStore {
        self.parsed(host, "credential_store")
    }

    /// The OAuth2 client id to log in with, when this instance registers its own application.
    ///
    /// Unset means [`crate::oauth::BUILTIN_CLIENT_ID`], which every stock Forgejo accepts. The
    /// per-host form is the one that matters: a user with one instance whose administrator
    /// disabled the built-in applications should not have to pass `--client-id` to every other
    /// instance as well.
    pub fn oauth_client_id(&self, host: Option<&str>) -> Option<String> {
        self.string(host, "oauth_client_id")
    }

    /// Default: [`ColorPref::Auto`].
    pub fn color(&self, host: Option<&str>) -> ColorPref {
        self.parsed(host, "color")
    }

    fn string(&self, host: Option<&str>, key: &str) -> Option<String> {
        self.lookup(host, key)?.as_str().map(str::to_owned).filter(|s| !s.is_empty())
    }

    fn parsed<T: std::str::FromStr + Default>(&self, host: Option<&str>, key: &str) -> T {
        self.string(host, key).and_then(|s| s.parse().ok()).unwrap_or_default()
    }

    // ------------------------------------------------------------------------ aliases

    /// User-defined command aliases, e.g. `co = "pr checkout"`.
    pub fn aliases(&self) -> BTreeMap<String, String> {
        self.table
            .get("aliases")
            .and_then(toml::Value::as_table)
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn alias(&self, name: &str) -> Option<String> {
        self.table.get("aliases")?.as_table()?.get(name)?.as_str().map(str::to_owned)
    }

    /// Rejects an alias that shadows nothing in particular — validation of the *expansion*
    /// belongs to the binary, which is the only place that knows the command tree.
    pub fn set_alias(&mut self, name: &str, expansion: &str) -> Result<()> {
        if name.trim().is_empty() || name.starts_with('-') {
            return Err(Error::new(ErrorKind::Usage(format!(
                "{name:?} is not a usable alias name"
            ))));
        }
        let t = self
            .table
            .entry("aliases".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(t) = t.as_table_mut() else {
            return Err(Error::new(ErrorKind::Usage(
                "config.toml: `aliases` is not a table".into(),
            )));
        };
        t.insert(name.to_owned(), toml::Value::String(expansion.to_owned()));
        Ok(())
    }

    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.table
            .get_mut("aliases")
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.remove(name))
            .is_some()
    }
}

fn check_key(key: &str) -> Result<&'static str> {
    KEYS.iter().find(|&&k| k == key).copied().ok_or_else(|| {
        Error::new(ErrorKind::Usage(format!(
            "unknown config key {key:?}; known keys: {}",
            KEYS.join(", ")
        )))
    })
}

fn check_value(key: &'static str, value: &str) -> Result<String> {
    let bad = |allowed: &[&str]| {
        Error::new(ErrorKind::Usage(format!(
            "{value:?} is not a valid value for {key}; expected one of: {}",
            allowed.join(", ")
        )))
    };
    match key {
        "prompt" => {
            value.parse::<Prompt>().map(|v| v.as_str().to_owned()).map_err(|()| bad(Prompt::VALUES))
        }
        "color" => value
            .parse::<ColorPref>()
            .map(|v| v.as_str().to_owned())
            .map_err(|()| bad(ColorPref::VALUES)),
        "credential_store" => value
            .parse::<CredentialStore>()
            .map(|v| v.as_str().to_owned())
            .map_err(|()| bad(CredentialStore::VALUES)),
        _ => Ok(value.to_owned()),
    }
}

/// The documented default for each key, as `list` should display it. An empty string means
/// "unset, and the fallback is described by the corresponding `resolved_*` method".
fn default_for(key: &str) -> String {
    match key {
        "prompt" => Prompt::default().as_str().to_owned(),
        "color" => ColorPref::default().as_str().to_owned(),
        "credential_store" => CredentialStore::default().as_str().to_owned(),
        _ => String::new(),
    }
}

/// Renders a TOML scalar the way `config get` should print it. Non-scalars (a table where a
/// string belongs) print as their TOML form rather than erroring, because `config get` is a
/// diagnostic tool and hiding the mess would make it useless.
fn render_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn missing_file_is_defaults_not_an_error() {
        // Regression net for: a fresh install where `fjo --version` fails because there
        // is no config.toml yet.
        let dir = tmp();
        let c = Config::load_from_dir(dir.path()).unwrap();
        assert_eq!(c.prompt(None), Prompt::Enabled);
        assert_eq!(c.color(None), ColorPref::Auto);
        assert_eq!(c.credential_store(None), CredentialStore::Keyring);
        assert_eq!(c.editor(None), None);
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = tmp();
        let mut c = Config::empty_at(dir.path());
        c.set(None, "editor", "hx").unwrap();
        c.set(None, "prompt", "disabled").unwrap();
        c.set(Some("git.example.org"), "pager", "cat").unwrap();
        c.set_alias("co", "pr checkout").unwrap();
        c.save().unwrap();

        let reread = Config::load_from_dir(dir.path()).unwrap();
        assert_eq!(reread.editor(None).as_deref(), Some("hx"));
        assert_eq!(reread.prompt(None), Prompt::Disabled);
        assert_eq!(reread.alias("co").as_deref(), Some("pr checkout"));
        // Per-host override wins over the (absent) global value...
        assert_eq!(reread.pager(Some("git.example.org")).as_deref(), Some("cat"));
        // ...and does not leak to other hosts.
        assert_eq!(reread.pager(Some("codeberg.org")), None);
    }

    #[test]
    fn per_host_override_beats_global() {
        let dir = tmp();
        let mut c = Config::empty_at(dir.path());
        c.set(None, "editor", "vim").unwrap();
        c.set(Some("git.example.org"), "editor", "hx").unwrap();
        assert_eq!(c.editor(None).as_deref(), Some("vim"));
        assert_eq!(c.editor(Some("git.example.org")).as_deref(), Some("hx"));
        assert_eq!(c.editor(Some("codeberg.org")).as_deref(), Some("vim"));
    }

    #[test]
    fn unknown_keys_survive_a_write() {
        // Regression net for: an older fjo deleting a newer fjo's settings on `config set`.
        let dir = tmp();
        std::fs::write(dir.path().join(CONFIG_FILE), "editor = \"hx\"\nfuture_thing = 7\n")
            .unwrap();
        let mut c = Config::load_from_dir(dir.path()).unwrap();
        assert_eq!(c.unknown_keys(), vec!["future_thing".to_owned()]);
        c.set(None, "pager", "less").unwrap();
        c.save().unwrap();
        let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(text.contains("future_thing"), "unknown key was dropped: {text}");
    }

    #[test]
    fn set_rejects_bad_keys_and_values() {
        let dir = tmp();
        let mut c = Config::empty_at(dir.path());
        let e = c.set(None, "promt", "enabled").unwrap_err();
        assert!(matches!(*e.kind, ErrorKind::Usage(_)));
        // "off" is the obvious guess and is wrong; failing loudly beats being ignored.
        let e = c.set(None, "prompt", "off").unwrap_err();
        match &*e.kind {
            ErrorKind::Usage(m) => assert!(m.contains("enabled"), "{m}"),
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tmp();
        std::fs::write(dir.path().join(CONFIG_FILE), "editor = ").unwrap();
        assert!(Config::load_from_dir(dir.path()).is_err());
    }

    #[test]
    fn pager_env_beats_config() {
        let dir = tmp();
        let mut c = Config::empty_at(dir.path());
        c.set(None, "pager", "less").unwrap();
        let env = MapEnv::new().with("FJO_PAGER", "cat");
        assert_eq!(c.resolved_pager(None, &env), "cat");
        assert_eq!(c.resolved_pager(None, &MapEnv::new()), "less");
    }

    #[test]
    fn config_dir_precedence() {
        let e = MapEnv::new().with("FJO_CONFIG_DIR", "/a").with("XDG_CONFIG_HOME", "/b");
        assert_eq!(config_dir(&e).unwrap(), PathBuf::from("/a"));
        let e = MapEnv::new().with("XDG_CONFIG_HOME", "/b");
        assert_eq!(config_dir(&e).unwrap(), PathBuf::from("/b/fjo"));
    }

    #[test]
    fn list_shows_effective_values() {
        let dir = tmp();
        let mut c = Config::empty_at(dir.path());
        c.set(None, "color", "never").unwrap();
        let listed: BTreeMap<_, _> = c.list(None).into_iter().collect();
        assert_eq!(listed["color"], "never");
        assert_eq!(listed["prompt"], "enabled");
        assert_eq!(listed["editor"], "");
    }
}

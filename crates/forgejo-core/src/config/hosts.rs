//! `hosts.toml` — which Forgejo instances we know about and who we are on each.
//!
//! **A host holds a named list of logins, not a single entry.** This follows `tea` rather
//! than `gh`, and it is the one place where `gh`'s data model is simply not expressive
//! enough: `gh`'s `hosts.yml` is a map keyed by hostname, so a second account on the same
//! host is unrepresentable. That is not an exotic case — a maintainer with a personal
//! account and a bot/CI account on the same Codeberg, or a work and a personal identity on
//! one self-hosted instance, hits it immediately.
//!
//! ```toml
//! active = "git.example.org"
//!
//! [[hosts]]
//! name = "git.example.org"
//! url = "https://git.example.org"
//! active_login = "perf3ct"
//! credential_store = "keyring"   # cached probe result, see `super::secrets`
//!
//!   [[hosts.logins]]
//!   user = "perf3ct"
//!   # the token lives in the credential store, NOT here — unless that store is `file`
//! ```

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::secrets::CredentialStore;
use super::{Env, HOSTS_FILE, write_atomic};
use crate::error::{Error, ErrorKind, Result};
use crate::types::Scope;

// ------------------------------------------------------------------------------ HostKey

/// A host's identity: `host[:port][/subpath]`, normalized.
///
/// The subpath is part of the identity, not decoration. Forgejo's `ROOT_URL` may carry a
/// path prefix (`https://example.org/forgejo/`), and two Forgejo instances behind one
/// reverse proxy at different prefixes are genuinely different hosts. Dropping the prefix
/// would make them collide and send requests to the wrong instance.
///
/// Normalization, and why each rule exists:
///
/// * scheme and credentials are stripped — `https://u:t@h/o/r` and `h` are the same host,
///   and retaining the credentials would risk them turning up in an error message;
/// * the authority is lowercased (DNS is case-insensitive) but the subpath is **not**
///   (URL paths are case-sensitive);
/// * ports 80 and 443 are dropped, since `host:443` and `host` are the same instance, while
///   `host:3000` is a different one and must be kept;
/// * a trailing `api/v1` is dropped, because pasting the API base URL into
///   `fjo auth login --host` is the single most common way to get this wrong.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostKey(String);

impl HostKey {
    /// Parses and normalizes user input: a bare host, `host:port`, `host/subpath`, or a
    /// full URL.
    pub fn parse(input: &str) -> Result<Self> {
        let t = input.trim();
        let bad = |why: &str| {
            Error::new(ErrorKind::Usage(format!("{input:?} is not a usable host: {why}")))
        };

        let rest = t.split_once("://").map_or(t, |(_scheme, r)| r);
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        // Userinfo ends at the last '@' of the authority; a password may contain ':' but
        // not an unescaped '@', so `rsplit_once` is the safe split.
        let authority = authority.rsplit_once('@').map_or(authority, |(_creds, a)| a);
        let authority = authority.to_ascii_lowercase();

        let (host, port) = split_port(&authority).map_err(bad)?;
        if host.is_empty() {
            return Err(bad("no host part"));
        }
        // Deliberately permissive beyond this: a hostname may be a bare intranet name, an
        // IP literal, or a `.local` mDNS name, so anything stricter than "no whitespace"
        // would reject setups that work. An unreachable host reports a DNS error, which is a
        // far clearer diagnosis than a validation message from here.
        if host.contains(' ') {
            return Err(bad("contains a space"));
        }

        let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() >= 2 && segments[segments.len() - 2..] == ["api", "v1"] {
            segments.truncate(segments.len() - 2);
        }

        let mut out = String::from(host);
        if let Some(p) = port
            && p != 80
            && p != 443
        {
            out.push(':');
            out.push_str(&p.to_string());
        }
        for s in segments {
            out.push('/');
            out.push_str(s);
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `host[:port]`, without the subpath.
    pub fn authority(&self) -> &str {
        self.0.split('/').next().unwrap_or(&self.0)
    }

    /// The hostname alone, without port or subpath.
    pub fn host(&self) -> &str {
        let a = self.authority();
        match a.strip_prefix('[') {
            // IPv6 literal: the port, if any, follows the closing bracket.
            Some(_) => a.split_once("]:").map_or(a, |(h, _)| &a[..h.len() + 1]),
            None => a.split_once(':').map_or(a, |(h, _)| h),
        }
    }

    pub fn port(&self) -> Option<u16> {
        split_port(self.authority()).ok().and_then(|(_, p)| p)
    }

    /// The path prefix, without leading or trailing slashes. Empty for a root install.
    pub fn subpath(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, p)| p)
    }
}

impl std::fmt::Display for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for HostKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl Serialize for HostKey {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HostKey {
    /// Normalizes on the way in, so a hand-edited `hosts.toml` with
    /// `name = "https://Git.Example.Org/"` still matches a remote URL.
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(D::Error::custom)
    }
}

/// Splits `host[:port]`, handling bracketed IPv6 literals.
fn split_port(authority: &str) -> std::result::Result<(&str, Option<u16>), &'static str> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or("unterminated IPv6 literal")?;
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return Err("junk after IPv6 literal"),
        };
        (&authority[..end + 2], port)
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        None => None,
        Some("") => return Err("empty port"),
        Some(p) => Some(p.parse::<u16>().map_err(|_| "port is not a number")?),
    };
    Ok((host, port))
}

// -------------------------------------------------------------------------------- Login

/// One identity on a host.
///
/// `token` is populated **only** when the file credential store is in use. With the keyring
/// or an environment variable, this field stays `None` and the secret never touches disk.
#[derive(Serialize, Deserialize, Default)]
pub struct Login {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_secret")]
    pub token: Option<SecretString>,
    /// The scopes the token was created with, recorded at login time.
    ///
    /// Forgejo does not report a token's scopes over the API, so this is the only way an
    /// `InsufficientScope` error can say `token has: read:repository` instead of
    /// `unknown`. It is advisory: a token created outside `fjo` has no record here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<Scope>,
    /// Which kind of credential is filed for this login: `"pat"` or `"oauth2"`.
    ///
    /// **Advisory, and never load-bearing.** The credential store is the single source of truth;
    /// whatever reads a credential parses it and decides. This exists so `auth status` can say
    /// "OAuth session" while the keyring is locked, and so someone reading this file can see why
    /// a login has no token line.
    ///
    /// Nothing may depend on it, because an older `fjo` will silently drop it. `Login` is a
    /// typed struct with no catch-all, unlike `Config`, which keeps a `toml::Table` precisely so
    /// unknown keys survive a round trip — so any field added here is lost the next time an
    /// older build saves the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The web-session credential document, when the file store holds it.
    ///
    /// Separate from `token` because the two authenticate different things and neither can
    /// substitute for the other: `token` is sent as `Authorization` to `/api/v1`, and this is a
    /// cookie sent to the web root, which is the only way to reach a route Forgejo never gave an
    /// API (see `forgejo_core::web`). A login may hold one, both, or neither.
    ///
    /// Carries the same caveat as `kind`: `Login` is a typed struct with no catch-all, so an
    /// older `fjo` that saves this file **drops this field**. The consequence is one re-login,
    /// not a corrupt file, and `auth status` reports the slot as empty rather than broken.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_secret")]
    pub web_session: Option<SecretString>,
}

impl Login {
    pub fn new(user: impl Into<String>) -> Self {
        Self { user: user.into(), token: None, scopes: Vec::new(), kind: None, web_session: None }
    }
}

/// Hand-written so a token can never reach a log line, a panic message, or `--debug`
/// output through a derived `Debug`. `SecretString` already redacts itself, but a derive
/// here would still be one `#[derive(Debug)]` away from leaking if the field type changed.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("user", &self.user)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("web_session", &self.web_session.as_ref().map(|_| "<redacted>"))
            .field("scopes", &self.scopes)
            .field("kind", &self.kind)
            .finish()
    }
}

/// `secrecy` 0.10's serde support is behind a feature we do not enable, and enabling it
/// would make *every* `SecretString` in the crate silently serializable. Doing it by hand
/// here keeps the blast radius to this one field.
mod opt_secret {
    use super::{ExposeSecret, SecretString};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        v: &Option<SecretString>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            Some(t) => s.serialize_str(t.expose_secret()),
            None => s.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<SecretString>, D::Error> {
        // serde hands us a plain `String`. Copy it into a `SecretString` and then wipe the
        // original: `String::into_boxed_str` may reallocate, which would leave the token
        // sitting in freed heap memory for the rest of the process's life.
        let mut plain = Option::<String>::deserialize(d)?;
        let out = plain.as_deref().filter(|s| !s.is_empty()).map(SecretString::from);
        if let Some(s) = plain.as_mut() {
            zeroize::Zeroize::zeroize(s);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------- HostEntry

/// A configured Forgejo instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: HostKey,
    /// The instance base URL including scheme, e.g. `https://example.org/forgejo`. Stored
    /// rather than derived because the scheme is not recoverable from [`HostKey`], and
    /// guessing `https` for a `http://localhost:3000` dev instance would break it.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_login: Option<String>,
    /// The credential store that last worked for this host — a cached probe result, not a
    /// preference. See [`super::secrets`] for why caching it matters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_store: Option<CredentialStore>,
    /// Serialized last so the TOML emitter puts the `[[hosts.logins]]` sub-tables after
    /// this host's scalar keys; TOML has no way to express a value after a sub-table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<Login>,
}

impl HostEntry {
    /// Builds an entry from anything the user might type for `--host`: `git.example.org`,
    /// `localhost:3000`, `https://example.org/forgejo`.
    pub fn from_input(input: &str) -> Result<Self> {
        let name = HostKey::parse(input)?;
        let scheme = scheme_for(input, &name);
        let url = format!("{scheme}://{name}");
        Ok(Self { name, url, active_login: None, credential_store: None, logins: Vec::new() })
    }

    /// The REST base, e.g. `https://example.org/forgejo/api/v1`.
    pub fn api_base(&self) -> String {
        format!("{}/api/v1", self.url.trim_end_matches('/'))
    }

    /// Where a user creates a token. Named in `TokenRejected` and `InsufficientScope`
    /// remedies, so it has to be a URL that can be pasted into a browser.
    pub fn token_settings_url(&self) -> String {
        format!("{}/user/settings/applications", self.url.trim_end_matches('/'))
    }

    pub fn login(&self, user: &str) -> Option<&Login> {
        self.logins.iter().find(|l| l.user == user)
    }

    pub fn login_mut(&mut self, user: &str) -> Option<&mut Login> {
        self.logins.iter_mut().find(|l| l.user == user)
    }
}

/// Picks a scheme when the input did not carry one.
///
/// `https` everywhere except loopback, where a bare `localhost:3000` is overwhelmingly a
/// development or test instance served over plain HTTP; guessing `https` there produces a
/// TLS handshake error that reads like a broken server.
fn scheme_for(input: &str, key: &HostKey) -> &'static str {
    if let Some((scheme, _)) = input.trim().split_once("://")
        && (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
    {
        return if scheme.eq_ignore_ascii_case("http") { "http" } else { "https" };
    }
    match key.host() {
        "localhost" | "127.0.0.1" | "[::1]" => "http",
        _ => "https",
    }
}

// -------------------------------------------------------------------------------- Hosts

#[derive(Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<HostKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hosts: Vec<HostEntry>,
}

/// The parsed `hosts.toml`, plus the warnings gathered while loading it.
pub struct Hosts {
    path: PathBuf,
    data: HostsFile,
    warnings: Vec<ErrorKind>,
    dirty: bool,
}

impl Hosts {
    pub fn load(env: &dyn Env) -> Result<Self> {
        Self::load_from_dir(&super::config_dir(env)?)
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        Self::load_at(&dir.join(HOSTS_FILE))
    }

    /// A missing file means "no hosts configured yet", not an error — the remedy for that
    /// is `fjo auth login`, which the [`ErrorKind::NoHostConfigured`] renderer prints.
    pub fn load_at(path: &Path) -> Result<Self> {
        let mut warnings = Vec::new();
        let data = match std::fs::read_to_string(path) {
            Ok(text) => {
                if let Some(w) = permission_warning(path) {
                    warnings.push(w);
                }
                toml::from_str::<HostsFile>(&text).map_err(|e| {
                    Error::new(ErrorKind::Usage(format!(
                        "{} is not valid TOML: {e}",
                        path.display()
                    )))
                })?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HostsFile::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: path.to_owned(), data, warnings, dirty: false })
    }

    /// An empty in-memory set of hosts, for tests and for the first `auth login`.
    pub fn empty_at(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            data: HostsFile::default(),
            warnings: Vec::new(),
            dirty: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Non-fatal problems found while loading — currently only
    /// [`ErrorKind::CredFilePermissions`]. The caller renders these as warnings; failing
    /// would lock a user out of their own tool over a file mode they can fix in one command.
    pub fn warnings(&self) -> &[ErrorKind] {
        &self.warnings
    }

    pub fn take_warnings(&mut self) -> Vec<ErrorKind> {
        std::mem::take(&mut self.warnings)
    }

    /// Whether anything changed since load. Lets the binary call
    /// [`Hosts::save_if_dirty`] unconditionally without rewriting the file (and resetting
    /// its mtime) on every invocation.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if self.dirty {
            self.save()?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Writes `hosts.toml` at mode 0600.
    ///
    /// Always 0600, even when no token is stored in the file: whether one *will* be is a
    /// function of a later `--insecure-storage`, and a file that is sometimes 0644 and
    /// sometimes 0600 is a file whose mode nobody can reason about.
    ///
    /// Because the write goes through a fresh 0600 temporary file plus `rename`, any save
    /// also *repairs* a file that had been left group- or world-readable — so the warning
    /// from [`Hosts::warnings`] has a one-command remedy (`fjo auth switch`, or any command
    /// that touches the file).
    pub fn save(&mut self) -> Result<()> {
        let text = toml::to_string_pretty(&self.data)
            .map_err(|e| Error::new(ErrorKind::Usage(format!("cannot serialize hosts: {e}"))))?;
        write_atomic(&self.path, &text, Some(0o600))?;
        self.dirty = false;
        Ok(())
    }

    // ---------------------------------------------------------------------------- hosts

    pub fn hosts(&self) -> &[HostEntry] {
        &self.data.hosts
    }

    pub fn keys(&self) -> Vec<HostKey> {
        self.data.hosts.iter().map(|h| h.name.clone()).collect()
    }

    /// The configured hosts as strings, for `UnknownHost { known }` — which exists so the
    /// error can suggest the host the user meant instead of just rejecting the one they typed.
    pub fn known(&self) -> Vec<String> {
        self.data.hosts.iter().map(|h| h.name.to_string()).collect()
    }

    pub fn get(&self, key: &HostKey) -> Option<&HostEntry> {
        self.data.hosts.iter().find(|h| &h.name == key)
    }

    pub fn get_mut(&mut self, key: &HostKey) -> Option<&mut HostEntry> {
        self.dirty = true;
        self.data.hosts.iter_mut().find(|h| &h.name == key)
    }

    pub fn contains(&self, key: &HostKey) -> bool {
        self.get(key).is_some()
    }

    /// Adds a host, or returns the existing one. Idempotent, because `auth login` against a
    /// host you already have must add a login rather than a duplicate host.
    pub fn add_host(&mut self, input: &str) -> Result<&mut HostEntry> {
        let entry = HostEntry::from_input(input)?;
        let name = entry.name.clone();
        self.dirty = true;
        if !self.data.hosts.iter().any(|h| h.name == name) {
            self.data.hosts.push(entry);
            // Sorted so the file's diff is stable across `auth login` runs.
            self.data.hosts.sort_by(|a, b| a.name.cmp(&b.name));
        }
        if self.data.active.is_none() {
            self.data.active = Some(name.clone());
        }
        let pos = self.data.hosts.iter().position(|h| h.name == name).expect("just inserted");
        Ok(&mut self.data.hosts[pos])
    }

    /// Removes a host and everything under it. Clears `active` if it pointed here, so the
    /// file can never reference a host that is gone.
    pub fn remove_host(&mut self, key: &HostKey) -> bool {
        let before = self.data.hosts.len();
        self.data.hosts.retain(|h| &h.name != key);
        let removed = self.data.hosts.len() != before;
        if removed {
            self.dirty = true;
            if self.data.active.as_ref() == Some(key) {
                self.data.active = self.data.hosts.first().map(|h| h.name.clone());
            }
        }
        removed
    }

    pub fn active(&self) -> Option<&HostKey> {
        self.data.active.as_ref()
    }

    pub fn set_active(&mut self, key: &HostKey) -> Result<()> {
        if !self.contains(key) {
            return Err(self.unknown_host(key.as_str()));
        }
        self.data.active = Some(key.clone());
        self.dirty = true;
        Ok(())
    }

    /// Records which credential store actually worked for this host, so the next invocation
    /// does not repeat a probe that failed.
    pub fn set_cached_store(&mut self, key: &HostKey, store: CredentialStore) {
        if let Some(h) = self.data.hosts.iter_mut().find(|h| &h.name == key)
            && h.credential_store != Some(store)
        {
            h.credential_store = Some(store);
            self.dirty = true;
        }
    }

    pub fn cached_store(&self, key: &HostKey) -> Option<CredentialStore> {
        self.get(key).and_then(|h| h.credential_store)
    }

    // --------------------------------------------------------------------------- logins

    /// Adds or updates a login. The first login on a host becomes its active one.
    /// Write (or clear) the web-session document for a login that already exists.
    ///
    /// Separate from [`Hosts::add_login`] rather than a fifth parameter on it: that function is
    /// about establishing an identity and its API token, and every one of its existing callers
    /// would have to pass a `None` for a slot it has no opinion about.
    ///
    /// Returns `false` when the login is not present, which the caller turns into its own error;
    /// creating one here would let a failed web login leave an identity behind.
    pub fn set_web_session(
        &mut self,
        key: &HostKey,
        user: &str,
        session: Option<SecretString>,
    ) -> bool {
        let Some(host) = self.data.hosts.iter_mut().find(|h| &h.name == key) else {
            return false;
        };
        let Some(login) = host.logins.iter_mut().find(|l| l.user == user) else {
            return false;
        };
        login.web_session = session;
        self.dirty = true;
        true
    }

    pub fn add_login(
        &mut self,
        key: &HostKey,
        user: &str,
        token: Option<SecretString>,
        scopes: Vec<Scope>,
        kind: Option<&str>,
    ) -> Result<()> {
        let known = self.known();
        let Some(host) = self.data.hosts.iter_mut().find(|h| &h.name == key) else {
            return Err(Error::new(ErrorKind::UnknownHost { given: key.to_string(), known }));
        };
        match host.login_mut(user) {
            Some(l) => {
                if token.is_some() {
                    l.token = token;
                }
                if !scopes.is_empty() {
                    l.scopes = scopes;
                }
                if kind.is_some() {
                    l.kind = kind.map(str::to_owned);
                }
            }
            None => {
                host.logins.push(Login {
                    user: user.to_owned(),
                    token,
                    scopes,
                    kind: kind.map(str::to_owned),
                    web_session: None,
                });
                host.logins.sort_by(|a, b| a.user.cmp(&b.user));
            }
        }
        if host.active_login.is_none() {
            host.active_login = Some(user.to_owned());
        }
        self.dirty = true;
        Ok(())
    }

    /// Removes a login. If it was the active one, the next remaining login takes over — a
    /// host with logins but no active login would make every later command fail with a
    /// message about authentication rather than about the logout that caused it.
    pub fn remove_login(&mut self, key: &HostKey, user: &str) -> Result<bool> {
        let known = self.known();
        let Some(host) = self.data.hosts.iter_mut().find(|h| &h.name == key) else {
            return Err(Error::new(ErrorKind::UnknownHost { given: key.to_string(), known }));
        };
        let before = host.logins.len();
        host.logins.retain(|l| l.user != user);
        let removed = host.logins.len() != before;
        if removed {
            self.dirty = true;
            if host.active_login.as_deref() == Some(user) {
                host.active_login = host.logins.first().map(|l| l.user.clone());
            }
        }
        Ok(removed)
    }

    /// Makes `user` the active login on `key`.
    pub fn select_login(&mut self, key: &HostKey, user: &str) -> Result<()> {
        let known = self.known();
        let Some(host) = self.data.hosts.iter_mut().find(|h| &h.name == key) else {
            return Err(Error::new(ErrorKind::UnknownHost { given: key.to_string(), known }));
        };
        if host.login(user).is_none() {
            let have: Vec<&str> = host.logins.iter().map(|l| l.user.as_str()).collect();
            return Err(Error::new(ErrorKind::Usage(format!(
                "no login {user:?} on {key}; configured logins: {}",
                if have.is_empty() { "(none)".to_owned() } else { have.join(", ") }
            ))));
        }
        host.active_login = Some(user.to_owned());
        self.dirty = true;
        Ok(())
    }

    /// Which login to use: `--login`/`FORGEJO_USER` → the host's `active_login` → the only
    /// login if there is exactly one → [`ErrorKind::NotAuthenticated`].
    pub fn resolve_login(&self, key: &HostKey, requested: Option<&str>) -> Result<String> {
        let host = self.get(key).ok_or_else(|| self.unknown_host(key.as_str()))?;
        if let Some(u) = requested {
            return match host.login(u) {
                Some(l) => Ok(l.user.clone()),
                None => Err(Error::new(ErrorKind::Usage(format!(
                    "no login {u:?} on {key}; run `fjo auth login --host {key}`"
                )))),
            };
        }
        if let Some(active) = &host.active_login
            && host.login(active).is_some()
        {
            return Ok(active.clone());
        }
        match host.logins.as_slice() {
            [only] => Ok(only.user.clone()),
            _ => Err(Error::new(ErrorKind::NotAuthenticated { host: key.to_string() })),
        }
    }

    // -------------------------------------------------------------------------- resolve

    /// The effective host: `--host` → `$FJO_HOST` → `$FORGEJO_HOST` → `active` → the only
    /// configured host.
    ///
    /// `$FJO_HOST` is accepted alongside `$FORGEJO_HOST` for consistency with the
    /// `FJO_*`-then-`FORGEJO_*` pattern used for `FJO_TOKEN` and `FJO_REPO`.
    ///
    /// Falling back to the sole configured host when `active` is unset is safe precisely
    /// because it is unambiguous; with two or more hosts we refuse rather than guess, since
    /// guessing means running an admin command against the wrong instance.
    pub fn resolve_host(&self, flag: Option<&str>, env: &dyn Env) -> Result<HostKey> {
        let given = flag
            .map(str::to_owned)
            .or_else(|| env.get("FJO_HOST"))
            .or_else(|| env.get("FORGEJO_HOST"));

        if let Some(given) = given {
            let key = HostKey::parse(&given)?;
            return if self.contains(&key) { Ok(key) } else { Err(self.unknown_host(&given)) };
        }
        if let Some(a) = &self.data.active
            && self.contains(a)
        {
            return Ok(a.clone());
        }
        match self.data.hosts.as_slice() {
            [only] => Ok(only.name.clone()),
            _ => Err(Error::new(ErrorKind::NoHostConfigured)),
        }
    }

    /// Adopt an explicitly named host that is not in `hosts.toml`, when a token is available
    /// from the environment. Returns the adopted key, if any.
    ///
    /// Call this *before* [`Hosts::resolve_host`]. It exists to make the CI pattern work with no
    /// configuration file at all:
    ///
    /// ```text
    /// FORGEJO_HOST=git.example.org FORGEJO_TOKEN=... fjo api user
    /// ```
    ///
    /// `gh` supports exactly this via `GH_HOST` + `GH_TOKEN`, and it is how nearly every CI job
    /// authenticates. Without it, talking to a host named on the command line would first
    /// require `fjo auth login` to write a credential file inside a throwaway container —
    /// which is both absurd and, on a container with no keyring, another failure to explain.
    ///
    /// Two deliberate limits:
    ///
    /// - **A token must already be in the environment.** Adopting a host we have no credential
    ///   for would only convert a clear "not one of your configured hosts" into a confusing
    ///   401 later.
    /// - **The entry is never persisted.** `dirty` is left untouched, so a one-off `--host`
    ///   cannot silently accumulate junk in the user's `hosts.toml`.
    pub fn adopt_env_host(&mut self, flag: Option<&str>, env: &dyn Env) -> Result<Option<HostKey>> {
        let Some(given) = flag
            .map(str::to_owned)
            .or_else(|| env.get("FJO_HOST"))
            .or_else(|| env.get("FORGEJO_HOST"))
        else {
            return Ok(None);
        };

        let key = HostKey::parse(&given)?;
        if self.contains(&key) {
            return Ok(None);
        }
        if !super::secrets::TOKEN_VARS.iter().any(|v| env.get(v).is_some_and(|t| !t.is_empty())) {
            return Ok(None);
        }

        // `from_input` already chooses the scheme, and is tested for it, so that decision
        // stays in exactly one place.
        let mut entry = HostEntry::from_input(&given)?;
        // The login is unknown until we ask the API; the token carries its own identity.
        entry.active_login = None;
        entry.credential_store = Some(CredentialStore::Env);
        self.data.hosts.push(entry);
        // Deliberately NOT setting `dirty`: ephemeral means ephemeral.
        Ok(Some(key))
    }

    /// Longest-prefix match of `(authority, path)` against the configured hosts.
    ///
    /// Returns the matching host and the *remainder* of the path. Longest-prefix, not first
    /// match, because one authority can host several Forgejo instances at different
    /// prefixes; matching `example.org` before `example.org/forgejo` would route a request
    /// to whichever happened to be listed first.
    ///
    /// This runs **before** any `owner/name` split. The tempting shortcut — take the last
    /// two path segments — works by accident for subpath installs and breaks on trailing
    /// slashes and on nested groups.
    pub fn match_prefix<'a>(
        &'a self,
        authority: &str,
        path: &str,
    ) -> Option<(&'a HostEntry, String)> {
        let authority = authority.to_ascii_lowercase();
        let segments: Vec<&str> = split_segments(path);
        let candidates =
            self.data.hosts.iter().filter(|h| h.name.authority() == authority).map(|h| &h.name);

        let (key, skip) = longest_subpath_prefix(candidates, &segments)?;
        Some((self.get(key)?, segments[skip..].join("/")))
    }

    fn unknown_host(&self, given: &str) -> Error {
        Error::new(ErrorKind::UnknownHost { given: given.to_owned(), known: self.known() })
    }
}

/// Splits a URL path into non-empty segments.
///
/// Dropping empty segments is what makes a trailing slash, a leading slash, and a doubled
/// slash all harmless — the three shapes that break "last two path segments" parsers.
pub fn split_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Picks the candidate host whose subpath is the **longest** prefix of `segments`, and returns
/// how many segments it consumed.
///
/// Longest, not first: one authority can serve several Forgejo instances at different path
/// prefixes, and matching `example.org` ahead of `example.org/forgejo` would silently route
/// requests to the wrong instance. Shared by [`Hosts::match_prefix`] and
/// [`crate::context::remote_url::resolve`] so the two cannot drift — they differ only in how
/// they filter candidates by authority, which is the SSH-port question, not this one.
pub fn longest_subpath_prefix<'a, I>(
    candidates: I,
    segments: &[&str],
) -> Option<(&'a HostKey, usize)>
where
    I: IntoIterator<Item = &'a HostKey>,
{
    let mut best: Option<(&HostKey, usize)> = None;
    for key in candidates {
        let prefix = split_segments(key.subpath());
        if segments.len() < prefix.len() || segments[..prefix.len()] != prefix[..] {
            continue;
        }
        if best.is_none_or(|(_, n)| prefix.len() > n) {
            best = Some((key, prefix.len()));
        }
    }
    best
}

impl std::fmt::Debug for Hosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosts")
            .field("path", &self.path)
            .field("active", &self.data.active)
            .field("hosts", &self.data.hosts)
            .finish()
    }
}

/// Checks `hosts.toml`'s mode, and returns a warning rather than an error.
///
/// The condition is "any group or other bit is set", not "the mode is not exactly 0600".
/// A stricter mode such as 0400 is *safer*, and warning about it would train users to
/// ignore the warning that matters — the one that says another local account can read
/// their token.
fn permission_warning(path: &Path) -> Option<ErrorKind> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o7777;
        if mode & 0o077 != 0 {
            return Some(ErrorKind::CredFilePermissions { path: path.to_owned(), mode });
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_normalization() {
        let cases: &[(&str, &str)] = &[
            ("git.example.org", "git.example.org"),
            ("https://git.example.org/", "git.example.org"),
            // DNS is case-insensitive; the identity must not depend on typing.
            ("HTTPS://Git.Example.ORG", "git.example.org"),
            // A non-default port is part of the identity.
            ("http://localhost:3000", "localhost:3000"),
            // :443 and :80 are not.
            ("https://git.example.org:443", "git.example.org"),
            ("http://git.example.org:80", "git.example.org"),
            // Subpath installs: the prefix IS the identity.
            ("https://example.org/forgejo", "example.org/forgejo"),
            ("https://example.org/forgejo/", "example.org/forgejo"),
            // Credentials never survive.
            ("https://user:tok@git.example.org", "git.example.org"),
            // Pasting the API base is the most common --host mistake.
            ("https://git.example.org/api/v1", "git.example.org"),
            ("https://example.org/forgejo/api/v1", "example.org/forgejo"),
        ];
        for (input, want) in cases {
            let got = HostKey::parse(input).unwrap();
            assert_eq!(got.as_str(), *want, "input {input:?}");
            assert!(!got.as_str().contains("tok"), "credentials leaked from {input:?}");
        }
    }

    #[test]
    fn host_key_parts() {
        let k = HostKey::parse("https://example.org:3000/forgejo/sub").unwrap();
        assert_eq!(k.authority(), "example.org:3000");
        assert_eq!(k.host(), "example.org");
        assert_eq!(k.port(), Some(3000));
        assert_eq!(k.subpath(), "forgejo/sub");

        let k = HostKey::parse("[::1]:3000").unwrap();
        assert_eq!(k.host(), "[::1]");
        assert_eq!(k.port(), Some(3000));
    }

    #[test]
    fn host_key_rejects_junk() {
        assert!(HostKey::parse("").is_err());
        assert!(HostKey::parse("host:notaport").is_err());
        assert!(HostKey::parse("host name").is_err());
    }

    #[test]
    fn two_logins_on_one_host() {
        // The case gh's host-keyed map cannot represent, and the reason for the named list.
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        let key = h.add_host("https://git.example.org").unwrap().name.clone();
        h.add_login(&key, "perf3ct", None, vec![], None).unwrap();
        h.add_login(&key, "ci-bot", None, vec![], None).unwrap();

        assert_eq!(h.get(&key).unwrap().logins.len(), 2);
        // The first login added became active.
        assert_eq!(h.resolve_login(&key, None).unwrap(), "perf3ct");
        h.select_login(&key, "ci-bot").unwrap();
        assert_eq!(h.resolve_login(&key, None).unwrap(), "ci-bot");
        // An explicit --login still wins.
        assert_eq!(h.resolve_login(&key, Some("perf3ct")).unwrap(), "perf3ct");
        assert!(h.resolve_login(&key, Some("nobody")).is_err());
    }

    #[test]
    fn round_trip_preserves_two_logins_and_the_active_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOSTS_FILE);
        let mut h = Hosts::empty_at(&path);
        let key = h.add_host("localhost:3000").unwrap().name.clone();
        h.add_login(&key, "b", None, vec!["read:repository".into()], None).unwrap();
        h.add_login(&key, "a", None, vec![], None).unwrap();
        h.select_login(&key, "a").unwrap();
        h.set_cached_store(&key, CredentialStore::File);
        h.save().unwrap();

        let reread = Hosts::load_at(&path).unwrap();
        assert!(reread.warnings().is_empty(), "{:?}", reread.warnings());
        let entry = reread.get(&key).unwrap();
        assert_eq!(entry.url, "http://localhost:3000", "loopback should default to http");
        assert_eq!(entry.logins.len(), 2);
        assert_eq!(entry.active_login.as_deref(), Some("a"));
        assert_eq!(reread.cached_store(&key), Some(CredentialStore::File));
        assert_eq!(entry.login("b").unwrap().scopes.len(), 1);
    }

    #[test]
    fn saved_file_is_0600() {
        // Regression net for: a token in hosts.toml readable by every local account.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOSTS_FILE);
        let mut h = Hosts::empty_at(&path);
        h.add_host("git.example.org").unwrap();
        h.save().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "hosts.toml mode is {mode:o}");
        }
    }

    #[test]
    fn loose_permissions_warn_but_do_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOSTS_FILE);
        std::fs::write(&path, "active = \"git.example.org\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let h = Hosts::load_at(&path).unwrap();
            assert!(matches!(
                h.warnings().first(),
                Some(ErrorKind::CredFilePermissions { mode: 0o644, .. })
            ));
        }
    }

    #[test]
    fn unknown_host_lists_the_known_ones() {
        // The point of UnknownHost::known: the message can suggest what the user meant.
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        h.add_host("git.example.org").unwrap();
        h.add_host("codeberg.org").unwrap();
        let err =
            h.resolve_host(Some("git.exmaple.org"), &super::super::MapEnv::new()).unwrap_err();
        match &*err.kind {
            ErrorKind::UnknownHost { given, known } => {
                assert_eq!(given, "git.exmaple.org");
                assert_eq!(known, &["codeberg.org".to_owned(), "git.example.org".to_owned()]);
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }

    #[test]
    fn the_ci_pattern_works_with_no_config_file() {
        // FORGEJO_HOST + FORGEJO_TOKEN and nothing else must work. `gh` supports exactly this
        // via GH_HOST + GH_TOKEN, and it is how nearly every CI job authenticates. Requiring
        // `fjo auth login` first would mean writing a credential file inside a throwaway
        // container just to talk to a host we were handed on the command line.
        use super::super::MapEnv;
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));

        let env =
            MapEnv::new().with("FORGEJO_HOST", "git.example.org").with("FORGEJO_TOKEN", "tok");

        let adopted = h.adopt_env_host(None, &env).unwrap().expect("should adopt");
        assert_eq!(adopted.as_str(), "git.example.org");
        assert_eq!(h.resolve_host(None, &env).unwrap().as_str(), "git.example.org");
        assert_eq!(h.get(&adopted).unwrap().url, "https://git.example.org");
    }

    #[test]
    fn an_adopted_host_is_never_written_to_disk() {
        // A one-off `--host` must not accumulate junk in the user's hosts.toml.
        use super::super::MapEnv;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOSTS_FILE);
        let mut h = Hosts::empty_at(&path);
        let env = MapEnv::new().with("FORGEJO_TOKEN", "tok");

        h.adopt_env_host(Some("git.example.org"), &env).unwrap().expect("should adopt");
        h.save_if_dirty().unwrap();

        assert!(!path.exists(), "an ephemeral host must not create hosts.toml");
    }

    #[test]
    fn a_host_with_no_token_anywhere_is_not_adopted() {
        // Adopting a host we have no credential for would turn a clear "not one of your
        // configured hosts" into a confusing 401 one call later.
        use super::super::MapEnv;
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        let env = MapEnv::new().with("FORGEJO_HOST", "git.example.org");

        assert!(h.adopt_env_host(None, &env).unwrap().is_none());
        assert!(matches!(
            *h.resolve_host(None, &env).unwrap_err().kind,
            ErrorKind::UnknownHost { .. }
        ));
    }

    #[test]
    fn resolve_host_precedence() {
        use super::super::MapEnv;
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        h.add_host("git.example.org").unwrap();
        h.add_host("codeberg.org").unwrap();
        h.set_active(&HostKey::parse("codeberg.org").unwrap()).unwrap();

        let env = MapEnv::new().with("FORGEJO_HOST", "git.example.org");
        // --host beats the environment...
        assert_eq!(h.resolve_host(Some("codeberg.org"), &env).unwrap().as_str(), "codeberg.org");
        // ...the environment beats `active`...
        assert_eq!(h.resolve_host(None, &env).unwrap().as_str(), "git.example.org");
        // ...and `active` is the last resort.
        assert_eq!(h.resolve_host(None, &MapEnv::new()).unwrap().as_str(), "codeberg.org");
    }

    #[test]
    fn no_hosts_configured() {
        let dir = tempfile::tempdir().unwrap();
        let h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        assert!(matches!(
            *h.resolve_host(None, &super::super::MapEnv::new()).unwrap_err().kind,
            ErrorKind::NoHostConfigured
        ));
    }

    #[test]
    fn longest_prefix_wins_over_first_match() {
        // Regression net for: two Forgejo instances behind one proxy, where matching the
        // bare authority first would send requests to the wrong one.
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        h.add_host("https://example.org").unwrap();
        h.add_host("https://example.org/forgejo").unwrap();

        let (host, rest) = h.match_prefix("example.org", "/forgejo/owner/repo").unwrap();
        assert_eq!(host.name.as_str(), "example.org/forgejo");
        assert_eq!(rest, "owner/repo");

        let (host, rest) = h.match_prefix("example.org", "/owner/repo").unwrap();
        assert_eq!(host.name.as_str(), "example.org");
        assert_eq!(rest, "owner/repo");

        assert!(h.match_prefix("other.example", "/owner/repo").is_none());
    }

    #[test]
    fn removing_a_host_clears_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = Hosts::empty_at(&dir.path().join(HOSTS_FILE));
        let a = h.add_host("a.example").unwrap().name.clone();
        h.add_host("b.example").unwrap();
        h.set_active(&a).unwrap();
        assert!(h.remove_host(&a));
        // Never leave `active` pointing at a host that is gone.
        assert_ne!(h.active(), Some(&a));
        assert!(h.active().is_some());
    }

    #[test]
    fn login_debug_never_prints_the_token() {
        let l = Login {
            user: "u".into(),
            token: Some("s3cr3t".into()),
            scopes: vec![],
            kind: None,
            web_session: Some("c00k1e".into()),
        };
        let s = format!("{l:?}");
        assert!(!s.contains("s3cr3t"), "{s}");
        assert!(!s.contains("c00k1e"), "{s}");
        assert!(s.contains("redacted"), "{s}");
    }
}

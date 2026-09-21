//! Where tokens live: the OS keyring, `hosts.toml` at mode 0600, or an environment variable.
//!
//! ## Why the keyring needs this much care
//!
//! `keyring` 4.x's default backend on Linux is the D-Bus Secret Service. That backend
//! **fails on headless machines, inside containers, over SSH without a session, and in CI**
//! — which is precisely where a command-line tool spends most of its life. Worse, a broken
//! or half-started session bus does not fail fast: it can *block for seconds* while D-Bus
//! waits for a service activation that will never complete.
//!
//! So three mechanisms, each fixing a distinct failure:
//!
//! 1. **Keyring failure is a warning, never a crash.** [`Credentials::token`] falls through
//!    to the file and environment stores and records an [`ErrorKind::KeyringUnavailable`]
//!    for the caller to print. A CLI that refuses to run because the desktop keyring is
//!    absent is a CLI that cannot be used from a server.
//! 2. **A 2-second timeout** ([`KEYRING_TIMEOUT`]) around every keyring call, implemented by
//!    running it on a throwaway thread that is abandoned if it does not answer.
//! 3. **The outcome is cached per host** as `credential_store` in `hosts.toml`, so a machine
//!    with no Secret Service pays the failed round-trip once rather than on every invocation.
//!
//! `FJO_CREDENTIAL_STORE=env|file|keyring` is a hard override that skips all of the above,
//! which is what CI should set.
//!
//! ## The token is never logged
//!
//! Tokens are carried in [`secrecy::SecretString`], which redacts itself in `Debug` and
//! zeroes its buffer on drop. Any plain `String` a backend hands us (the keyring API returns
//! one, and so does `std::env::var`) is explicitly [`zeroize`]d after being copied, because
//! `String::into_boxed_str` may reallocate and leave the original bytes in freed heap memory.
//!
//! Every token is returned with a [`TokenSource`]. A 401 that can say *which* token was
//! rejected and *where it came from* — `FORGEJO_TOKEN` from the environment, versus the
//! keyring entry for a different login — is the difference between a one-line fix and an
//! afternoon.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

use super::Env;
use super::hosts::{HostKey, Hosts};
use crate::error::{Error, ErrorKind, KeyringCause, Result, TokenSource};
use crate::types::Scope;

/// The keyring service name. Stable forever: changing it would orphan every stored token.
pub const KEYRING_SERVICE: &str = "fjo";

/// How long any single keyring call may take before we give up on it.
///
/// Two seconds is chosen against human patience, not against D-Bus: a working Secret Service
/// answers in single-digit milliseconds, so anything approaching this is already broken. The
/// budget for `fjo`'s entire pre-network phase is ~20 ms, and a user staring at a hung
/// prompt will not guess that their keyring is the reason.
pub const KEYRING_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variables checked for a token, in order.
pub const TOKEN_VARS: &[&str] = &["FJO_TOKEN", "FORGEJO_TOKEN"];

/// Environment variables checked for a web-session document, in order.
///
/// Deliberately not `FORGEJO_*`: the document's shape is this tool's, not Forgejo's, and a
/// name in Forgejo's namespace would suggest the server has a concept it does not have.
pub const WEB_SESSION_VARS: &[&str] = &["FJO_WEB_SESSION"];

/// The variable that hard-overrides store selection.
pub const STORE_VAR: &str = "FJO_CREDENTIAL_STORE";

// -------------------------------------------------------------------------- store kinds

/// Which backend holds a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum CredentialStore {
    /// The OS keyring: Keychain on macOS, Credential Manager on Windows, Secret Service on
    /// other unix.
    #[default]
    Keyring,
    /// `hosts.toml`, mode 0600. Chosen knowingly via `--insecure-storage`, or automatically
    /// when the keyring is unavailable.
    File,
    /// `FJO_TOKEN` / `FORGEJO_TOKEN`. Read-only by nature.
    Env,
}

impl CredentialStore {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keyring => "keyring",
            Self::File => "file",
            Self::Env => "env",
        }
    }

    pub const VALUES: &'static [&'static str] = &["keyring", "file", "env"];
}

impl std::fmt::Display for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CredentialStore {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        match s.trim().to_ascii_lowercase().as_str() {
            "keyring" => Ok(Self::Keyring),
            "file" | "plaintext" | "insecure" => Ok(Self::File),
            "env" | "environment" => Ok(Self::Env),
            _ => Err(()),
        }
    }
}

impl Serialize for CredentialStore {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CredentialStore {
    /// An unrecognised value in `hosts.toml` deserializes to the default rather than
    /// failing: this field is a *cache*, and a cache miss must never lock a user out.
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(s.parse().unwrap_or_default())
    }
}

// -------------------------------------------------------------------------------- Token

/// A token plus where it came from.
///
/// The secret is zeroed when this value drops (`SecretString`'s guarantee). There is no
/// `Clone`, no `Display`, and the `Debug` impl below is hand-written.
pub struct Token {
    secret: SecretString,
    source: TokenSource,
}

impl Token {
    pub fn new(secret: SecretString, source: TokenSource) -> Self {
        Self { secret, source }
    }

    /// The one place the plaintext escapes. Callers should pass it straight into an
    /// `Authorization` header and never bind it to a named variable.
    pub fn expose(&self) -> &str {
        self.secret.expose_secret()
    }

    pub fn secret(&self) -> &SecretString {
        &self.secret
    }

    pub fn source(&self) -> &TokenSource {
        &self.source
    }

    /// Hands the secret to the HTTP layer, which wraps it in its own auth type.
    pub fn into_parts(self) -> (SecretString, TokenSource) {
        (self.secret, self.source)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("secret", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

// ---------------------------------------------------------------------------- CredStore

/// One credential backend.
///
/// Every method takes `hosts` even though only [`FileStore`] uses it. The alternative —
/// giving `FileStore` a borrow of `Hosts` — would put a lifetime on the trait object and
/// force `Credentials` to juggle a `&mut Hosts` it also needs elsewhere. Passing it in
/// keeps the trait object-safe and lifetime-free, which is worth one unused parameter.
/// Which of a login's credentials a store operation is about.
///
/// # Why a second slot, when the OAuth document deliberately is not one
///
/// `oauth/stored.rs` packs an OAuth session's three values into a single document precisely so
/// they cannot be written non-atomically, and records that "a second entry for the refresh token
/// would be one more thing to orphan on logout". That reasoning is about values which rotate
/// **together**, and it still stands. It does not extend to these two: an API token and a web
/// session authenticate different transports — `Authorization` against `/api/v1`, a cookie
/// against the web root — neither can be derived from the other, and a login may hold either,
/// both, or neither. Packing them into one document would mean rewriting a working API token
/// every time a session is re-minted, which is the *opposite* of what that note protects.
///
/// The orphaning objection is real and is answered structurally rather than by remembering:
/// `Slot` is a closed enum, [`Slot::ALL`] enumerates it, and [`Credentials::forget`] iterates
/// that. A slot added here is a slot logout already clears, and a variant added without
/// updating `ALL` fails the test that asserts their lengths agree.
///
/// The keyring layout promise is kept intact: `{login}@{host}` remains the API entry's account
/// key forever. A web entry is a *prefixed* key beside it, never a change to that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Slot {
    /// The API token, sent as `Authorization` to `/api/v1`. The overwhelmingly common one, and
    /// the only one the tool had before layer 0 existed.
    #[default]
    Api,
    /// The web-session document — see `forgejo_core::web::WebCredential` — holding the
    /// long-lived remember token and the short-lived session cookie minted from it.
    Web,
}

impl Slot {
    /// Every slot, in the order `forget` clears them.
    ///
    /// This is what makes an orphaned credential impossible rather than merely unlikely, so it
    /// must stay exhaustive; `every_slot_is_in_all` asserts that it is.
    pub const ALL: [Slot; 2] = [Slot::Api, Slot::Web];

    /// The prefix distinguishing this slot's keyring account and environment variable.
    ///
    /// Empty for [`Slot::Api`], which is what preserves the documented `{login}@{host}` layout
    /// for every credential that existed before this enum did.
    fn prefix(self) -> &'static str {
        match self {
            Slot::Api => "",
            Slot::Web => "web:",
        }
    }
}

/// One credential backend.
pub trait CredStore {
    fn kind(&self) -> CredentialStore;

    /// `Ok(None)` means "this backend works and has no token for that login" —
    /// distinct from `Err`, which means the backend itself is unusable. Conflating the two
    /// is how a missing token turns into a scary D-Bus error, and how a broken keyring turns
    /// into a silent "not logged in".
    fn get(&self, host: &HostKey, login: &str, slot: Slot, hosts: &Hosts) -> Result<Option<Token>>;

    fn set(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        hosts: &mut Hosts,
    ) -> Result<TokenSource>;

    fn delete(&self, host: &HostKey, login: &str, slot: Slot, hosts: &mut Hosts) -> Result<()>;
}

// ------------------------------------------------------------------------ keyring store

/// Runs a keyring call on a throwaway thread and gives up after `timeout`.
///
/// A blocked D-Bus method call cannot be cancelled, so on timeout the worker thread is
/// **deliberately abandoned**. That is safe here: it holds no lock we need, it will finish or
/// block forever without affecting us, and a detached thread does not prevent process exit.
/// The alternative — waiting for it — is the hang this function exists to prevent.
fn with_timeout<T, F>(timeout: Duration, f: F) -> std::result::Result<T, KeyringCause>
where
    T: Send + 'static,
    F: FnOnce() -> keyring::Result<T> + Send + 'static,
{
    use std::sync::mpsc::RecvTimeoutError;

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("fjo-keyring".to_owned())
        .spawn(move || {
            // A send failure means we already timed out and nobody is listening.
            let _ = tx.send(f());
        })
        .is_err()
    {
        return Err(KeyringCause::Other("could not spawn a thread for the keyring".into()));
    }

    match rx.recv_timeout(timeout) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(classify_keyring(&e)),
        Err(RecvTimeoutError::Timeout) => Err(KeyringCause::Timeout),
        // The worker panicked. Treat it as a dead backend, not as a bug worth aborting for.
        Err(RecvTimeoutError::Disconnected) => {
            Err(KeyringCause::Other("the keyring backend panicked".into()))
        }
    }
}

/// Maps a `keyring` error onto the [`KeyringCause`] the renderer knows how to advise on.
///
/// The string sniffing is unavoidable: the interesting distinctions (no bus at all versus a
/// locked collection versus the user dismissing the unlock prompt) all arrive as
/// `PlatformFailure` with a backend-specific message. Getting it wrong costs a slightly less
/// specific remedy, never correctness — every branch still degrades to the file store.
fn classify_keyring(e: &keyring::Error) -> KeyringCause {
    use keyring::Error as K;
    let text = e.to_string().to_ascii_lowercase();
    let says = |needle: &str| text.contains(needle);

    match e {
        K::NoDefaultStore | K::NotSupportedByStore(_) => KeyringCause::NoBackend,
        K::NoStorageAccess(_) if says("denied") || says("dismissed") => KeyringCause::Denied,
        K::NoStorageAccess(_) => KeyringCause::Locked,
        K::PlatformFailure(_) => {
            if says("dbus") && (says("not provided by any") || says("serviceunknown")) {
                // No Secret Service on the bus: the headless / container / CI case.
                KeyringCause::NoBackend
            } else if says("no such file or directory") && says("bus") {
                KeyringCause::NoBackend
            } else if says("locked") {
                KeyringCause::Locked
            } else if says("denied") || says("dismissed") || says("not permitted") {
                KeyringCause::Denied
            } else if says("timed out") || says("timeout") {
                KeyringCause::Timeout
            } else {
                KeyringCause::Other(e.to_string())
            }
        }
        K::BadEncoding(_) => KeyringCause::Other("the stored token is not valid UTF-8".to_owned()),
        _ => KeyringCause::Other(e.to_string()),
    }
}

/// The OS keyring.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
    timeout: Duration,
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self { service: KEYRING_SERVICE.to_owned(), timeout: KEYRING_TIMEOUT }
    }
}

impl KeyringStore {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `<login>@<host>` for the API token, `web:<login>@<host>` for the web session — the host
    /// key is included so two accounts on one instance, and one account on two instances, all
    /// get distinct entries.
    ///
    /// [`Slot::Api`] has an empty prefix, so the account key documented as stable forever is
    /// byte-for-byte what it always was; a credential written by an older `fjo` is found here
    /// unchanged.
    fn account(host: &HostKey, login: &str, slot: Slot) -> String {
        format!("{}{login}@{host}", slot.prefix())
    }

    fn entry_label(&self, host: &HostKey, login: &str, slot: Slot) -> String {
        format!("{}:{}", self.service, Self::account(host, login, slot))
    }
}

impl CredStore for KeyringStore {
    fn kind(&self) -> CredentialStore {
        CredentialStore::Keyring
    }

    fn get(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        _hosts: &Hosts,
    ) -> Result<Option<Token>> {
        let service = self.service.clone();
        let account = Self::account(host, login, slot);
        // `Entry::new` is where the backend is lazily initialised, so it must be inside the
        // timeout too — that call is the one that blocks on a broken bus.
        let got = with_timeout(self.timeout, move || {
            match keyring::Entry::new(&service, &account)?.get_password() {
                Ok(p) => Ok(Some(p)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .map_err(|cause| Error::new(ErrorKind::KeyringUnavailable { cause }))?;

        Ok(got.map(|mut plain| {
            let secret = SecretString::from(plain.as_str());
            plain.zeroize();
            Token::new(secret, TokenSource::Keyring { entry: self.entry_label(host, login, slot) })
        }))
    }

    fn set(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        _hosts: &mut Hosts,
    ) -> Result<TokenSource> {
        let service = self.service.clone();
        let account = Self::account(host, login, slot);
        let plain = token.expose_secret().to_owned();
        with_timeout(self.timeout, move || {
            keyring::Entry::new(&service, &account)?.set_password(&plain)
        })
        .map_err(|cause| Error::new(ErrorKind::KeyringUnavailable { cause }))?;
        Ok(TokenSource::Keyring { entry: self.entry_label(host, login, slot) })
    }

    fn delete(&self, host: &HostKey, login: &str, slot: Slot, _hosts: &mut Hosts) -> Result<()> {
        let service = self.service.clone();
        let account = Self::account(host, login, slot);
        with_timeout(self.timeout, move || {
            match keyring::Entry::new(&service, &account)?.delete_credential() {
                // Already gone is success: `auth logout` must be idempotent.
                Err(keyring::Error::NoEntry) => Ok(()),
                other => other,
            }
        })
        .map_err(|cause| Error::new(ErrorKind::KeyringUnavailable { cause }))?;
        Ok(())
    }
}

/// An in-memory keyring for tests, and the reason no test in this crate touches the real
/// one. `fail` makes it behave like a headless machine.
#[derive(Debug, Default)]
pub struct FakeKeyring {
    entries: RefCell<BTreeMap<String, String>>,
    fail: Option<KeyringCause>,
}

impl FakeKeyring {
    pub fn new() -> Self {
        Self::default()
    }

    /// A keyring that is not there — the headless / container / CI / SSH case.
    pub fn unavailable(cause: KeyringCause) -> Self {
        Self { entries: RefCell::default(), fail: Some(cause) }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }
}

impl CredStore for FakeKeyring {
    fn kind(&self) -> CredentialStore {
        CredentialStore::Keyring
    }

    fn get(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        _hosts: &Hosts,
    ) -> Result<Option<Token>> {
        if let Some(cause) = &self.fail {
            return Err(Error::new(ErrorKind::KeyringUnavailable { cause: cause.clone() }));
        }
        let key = format!("{}{login}@{host}", slot.prefix());
        Ok(self.entries.borrow().get(&key).map(|v| {
            Token::new(
                SecretString::from(v.as_str()),
                TokenSource::Keyring { entry: format!("{KEYRING_SERVICE}:{key}") },
            )
        }))
    }

    fn set(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        _hosts: &mut Hosts,
    ) -> Result<TokenSource> {
        if let Some(cause) = &self.fail {
            return Err(Error::new(ErrorKind::KeyringUnavailable { cause: cause.clone() }));
        }
        let key = format!("{}{login}@{host}", slot.prefix());
        self.entries.borrow_mut().insert(key.clone(), token.expose_secret().to_owned());
        Ok(TokenSource::Keyring { entry: format!("{KEYRING_SERVICE}:{key}") })
    }

    fn delete(&self, host: &HostKey, login: &str, slot: Slot, _hosts: &mut Hosts) -> Result<()> {
        if let Some(cause) = &self.fail {
            return Err(Error::new(ErrorKind::KeyringUnavailable { cause: cause.clone() }));
        }
        self.entries.borrow_mut().remove(&format!("{}{login}@{host}", slot.prefix()));
        Ok(())
    }
}

// --------------------------------------------------------------------------- file store

/// Tokens in `hosts.toml`, mode 0600.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileStore;

impl CredStore for FileStore {
    fn kind(&self) -> CredentialStore {
        CredentialStore::File
    }

    fn get(&self, host: &HostKey, login: &str, slot: Slot, hosts: &Hosts) -> Result<Option<Token>> {
        let Some(secret) = hosts.get(host).and_then(|h| h.login(login)).and_then(|l| match slot {
            Slot::Api => l.token.as_ref(),
            Slot::Web => l.web_session.as_ref(),
        }) else {
            return Ok(None);
        };
        Ok(Some(Token::new(
            SecretString::from(secret.expose_secret()),
            TokenSource::File { path: hosts.path().to_owned() },
        )))
    }

    /// Does not write the file — it marks [`Hosts`] dirty and leaves the single 0600 write
    /// to [`Hosts::save`], so a login that also updates `active_login` produces one atomic
    /// rename rather than two.
    fn set(
        &self,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        hosts: &mut Hosts,
    ) -> Result<TokenSource> {
        let copy = SecretString::from(token.expose_secret());
        match slot {
            Slot::Api => hosts.add_login(host, login, Some(copy), Vec::new(), None)?,
            // `add_login` first, so a web session written for a host whose login is not yet
            // recorded establishes the identity exactly as an API token would, rather than
            // being silently dropped by `set_web_session`'s "login not present" arm.
            Slot::Web => {
                hosts.add_login(host, login, None, Vec::new(), None)?;
                if !hosts.set_web_session(host, login, Some(copy)) {
                    return Err(Error::new(ErrorKind::UnknownHost {
                        given: host.to_string(),
                        known: Vec::new(),
                    }));
                }
            }
        }
        Ok(TokenSource::File { path: hosts.path().to_owned() })
    }

    fn delete(&self, host: &HostKey, login: &str, slot: Slot, hosts: &mut Hosts) -> Result<()> {
        if let Some(l) = hosts.get_mut(host).and_then(|h| h.login_mut(login)) {
            match slot {
                Slot::Api => l.token = None,
                Slot::Web => l.web_session = None,
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------- env store

/// `FJO_TOKEN`, then `FORGEJO_TOKEN`.
///
/// An environment token is **not host-scoped**: it is used for whichever host the command
/// targets. That is exactly what CI wants and exactly what surprises someone who exported a
/// token for one instance and then talked to another — which is why the resulting
/// [`TokenSource::Env`] names the variable, so the 401 can say so.
pub struct EnvStore<'a> {
    env: &'a dyn Env,
}

impl<'a> EnvStore<'a> {
    pub fn new(env: &'a dyn Env) -> Self {
        Self { env }
    }
}

impl CredStore for EnvStore<'_> {
    fn kind(&self) -> CredentialStore {
        CredentialStore::Env
    }

    fn get(
        &self,
        _host: &HostKey,
        _login: &str,
        slot: Slot,
        _hosts: &Hosts,
    ) -> Result<Option<Token>> {
        let vars = match slot {
            Slot::Api => TOKEN_VARS,
            Slot::Web => WEB_SESSION_VARS,
        };
        for var in vars {
            if let Some(mut plain) = self.env.get(var) {
                let secret = SecretString::from(plain.as_str());
                plain.zeroize();
                return Ok(Some(Token::new(secret, TokenSource::Env { var: (*var).to_owned() })));
            }
        }
        Ok(None)
    }

    fn set(
        &self,
        _host: &HostKey,
        _login: &str,
        _slot: Slot,
        _token: &SecretString,
        _hosts: &mut Hosts,
    ) -> Result<TokenSource> {
        Err(Error::new(ErrorKind::Usage(format!(
            "cannot save a token into an environment variable; unset {STORE_VAR}=env, or pass \
             --insecure-storage to write it to hosts.toml"
        ))))
    }

    fn delete(&self, _host: &HostKey, _login: &str, _slot: Slot, _hosts: &mut Hosts) -> Result<()> {
        // Nothing to delete, and nothing to complain about: `auth logout` should not fail
        // because a variable is exported in the caller's shell. The caller is told to unset
        // it by `auth status`.
        Ok(())
    }
}

// -------------------------------------------------------------------------- Credentials

/// Chooses among the backends and degrades gracefully.
///
/// Read order is **keyring → file → env**, minus any head the cache or the configured
/// preference rules out. `FJO_CREDENTIAL_STORE` replaces the chain with a single entry.
pub struct Credentials<'a> {
    env: &'a dyn Env,
    keyring: Box<dyn CredStore + 'a>,
    file: FileStore,
    forced: Option<CredentialStore>,
    preference: CredentialStore,
    insecure: bool,
    warnings: Vec<ErrorKind>,
}

impl<'a> Credentials<'a> {
    /// Reads `FJO_CREDENTIAL_STORE` from `env`. An unrecognised value is ignored with a
    /// warning rather than being fatal — a typo in a CI variable should not stop the job
    /// before it can even print why.
    pub fn new(env: &'a dyn Env) -> Self {
        let mut warnings = Vec::new();
        let forced = match env.get(STORE_VAR) {
            None => None,
            Some(v) => match v.parse::<CredentialStore>() {
                Ok(s) => Some(s),
                Err(()) => {
                    warnings.push(ErrorKind::Usage(format!(
                        "ignoring {STORE_VAR}={v:?}; expected one of: {}",
                        CredentialStore::VALUES.join(", ")
                    )));
                    None
                }
            },
        };
        Self {
            env,
            keyring: Box::new(KeyringStore::default()),
            file: FileStore,
            forced,
            preference: CredentialStore::default(),
            insecure: false,
            warnings,
        }
    }

    /// The `config.toml` `credential_store` preference. Loses to
    /// `FJO_CREDENTIAL_STORE` and to `--insecure-storage`.
    #[must_use]
    pub fn with_preference(mut self, pref: CredentialStore) -> Self {
        self.preference = pref;
        self
    }

    /// `--insecure-storage`: the user has knowingly chosen `hosts.toml`.
    #[must_use]
    pub fn insecure_storage(mut self, yes: bool) -> Self {
        self.insecure = yes;
        self
    }

    /// Swaps the keyring implementation. The test seam that keeps every test in this crate
    /// away from the developer's real login keyring.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Box<dyn CredStore + 'a>) -> Self {
        self.keyring = keyring;
        self
    }

    /// Non-fatal problems: an unusable keyring, a bad `FJO_CREDENTIAL_STORE`. The caller
    /// prints these once, after the command's own output.
    pub fn warnings(&self) -> &[ErrorKind] {
        &self.warnings
    }

    pub fn take_warnings(&mut self) -> Vec<ErrorKind> {
        std::mem::take(&mut self.warnings)
    }

    /// The store this host will read from first, for `fjo auth status`.
    pub fn effective_store(&self, hosts: &Hosts, host: &HostKey) -> CredentialStore {
        *self.read_order(hosts, host).first().unwrap_or(&CredentialStore::Env)
    }

    fn read_order(&self, hosts: &Hosts, host: &HostKey) -> Vec<CredentialStore> {
        use CredentialStore::{Env as E, File as F, Keyring as K};
        if let Some(forced) = self.forced {
            // A hard override means *only* that store. Falling back would defeat the point:
            // CI sets this to get a deterministic, explainable failure.
            return vec![forced];
        }
        if self.insecure {
            return vec![F, E];
        }
        match hosts.cached_store(host).unwrap_or(self.preference) {
            K => vec![K, F, E],
            F => vec![F, E],
            E => vec![E],
        }
    }

    /// Dispatches to one backend. Three tiny helpers rather than a `Box<dyn CredStore>`
    /// factory, because a boxed view borrowing `&self` would collide with the `&mut
    /// self.warnings` push that follows every failure.
    fn get_from(
        &self,
        kind: CredentialStore,
        host: &HostKey,
        login: &str,
        slot: Slot,
        hosts: &Hosts,
    ) -> Result<Option<Token>> {
        match kind {
            CredentialStore::Keyring => self.keyring.get(host, login, slot, hosts),
            CredentialStore::File => self.file.get(host, login, slot, hosts),
            CredentialStore::Env => EnvStore::new(self.env).get(host, login, slot, hosts),
        }
    }

    fn set_in(
        &self,
        kind: CredentialStore,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        hosts: &mut Hosts,
    ) -> Result<TokenSource> {
        match kind {
            CredentialStore::Keyring => self.keyring.set(host, login, slot, token, hosts),
            CredentialStore::File => self.file.set(host, login, slot, token, hosts),
            CredentialStore::Env => EnvStore::new(self.env).set(host, login, slot, token, hosts),
        }
    }

    fn delete_in(
        &self,
        kind: CredentialStore,
        host: &HostKey,
        login: &str,
        slot: Slot,
        hosts: &mut Hosts,
    ) -> Result<()> {
        match kind {
            CredentialStore::Keyring => self.keyring.delete(host, login, slot, hosts),
            CredentialStore::File => self.file.delete(host, login, slot, hosts),
            CredentialStore::Env => EnvStore::new(self.env).delete(host, login, slot, hosts),
        }
    }

    /// Finds a token for `login` on `host`.
    ///
    /// `Ok(None)` means "no token anywhere", which the caller turns into
    /// [`ErrorKind::NotAuthenticated`] with a `fjo auth login` remedy. A keyring that
    /// cannot be reached becomes a warning and the search continues; `hosts` is updated with
    /// the probe result so the next invocation skips it.
    pub fn token(
        &mut self,
        hosts: &mut Hosts,
        host: &HostKey,
        login: &str,
    ) -> Result<Option<Token>> {
        self.secret(hosts, host, login, Slot::Api)
    }

    /// [`Credentials::token`], for any slot.
    ///
    /// The store-selection and warning behaviour is identical for every slot; only the key
    /// differs. Keeping one implementation means a web session cannot acquire a subtly
    /// different fallback order from the API token beside it.
    pub fn secret(
        &mut self,
        hosts: &mut Hosts,
        host: &HostKey,
        login: &str,
        slot: Slot,
    ) -> Result<Option<Token>> {
        // A store chosen explicitly for this one invocation must never be written back.
        //
        // The cache exists for one purpose: to stop us repeating a *probe that failed*, so a
        // machine with no D-Bus session does not pay a doomed keyring round trip every time.
        // `FJO_CREDENTIAL_STORE` and `--insecure-storage` are not probe results — they are
        // instructions about this run.
        //
        // Persisting them is actively destructive. A single
        // `FJO_CREDENTIAL_STORE=env fjo api user` would rewrite `credential_store = "env"`
        // into `hosts.toml`, and every later run — with that variable now unset — would consult
        // only the environment, find nothing, and report "you are not logged in" while the
        // token sat in the file one line below. Found exactly that way.
        let may_cache = self.forced.is_none() && !self.insecure;

        for kind in self.read_order(hosts, host) {
            let got = self.get_from(kind, host, login, slot, hosts);
            match got {
                Ok(Some(t)) => {
                    if may_cache {
                        hosts.set_cached_store(host, kind);
                    }
                    return Ok(Some(t));
                }
                // The backend works, it just has nothing for this login. Remember that it
                // works (so we keep using it) and keep looking.
                Ok(None) => {
                    if may_cache && kind == CredentialStore::Keyring {
                        hosts.set_cached_store(host, kind);
                    }
                }
                Err(e) => {
                    if !matches!(*e.kind, ErrorKind::KeyringUnavailable { .. }) {
                        return Err(e);
                    }
                    self.warnings.push(*e.kind);
                    // Cache the *fallback*, not the failure, so we stop paying for a D-Bus
                    // round trip that will not work. `store` re-probes, so plugging in a
                    // desktop session and running `fjo auth login` recovers.
                    //
                    // Still gated: if the user forced `keyring` for this run and it was
                    // unavailable, silently recording `file` would override the preference they
                    // just stated.
                    if may_cache {
                        hosts.set_cached_store(host, CredentialStore::File);
                    }
                }
            }
        }
        Ok(None)
    }

    /// Saves a token, recording the login in `hosts.toml` either way.
    ///
    /// Deliberately ignores the cached probe result and tries the keyring again: `auth login`
    /// is the moment a user would notice and fix a keyring problem, and a stale "file" cache
    /// must not permanently downgrade someone who has since started a desktop session.
    pub fn store(
        &mut self,
        hosts: &mut Hosts,
        host: &HostKey,
        login: &str,
        token: &SecretString,
        scopes: Vec<Scope>,
        kind: Option<&str>,
    ) -> Result<TokenSource> {
        self.store_in(hosts, host, login, Slot::Api, token, scopes, kind)
    }

    /// [`Credentials::store`], for any slot.
    ///
    /// `scopes` and `kind` describe an API token and are recorded on the login regardless of
    /// slot, because they are properties of the identity rather than of the secret; a web
    /// session passes an empty `scopes` and `None`, which leaves whatever the API token
    /// already recorded untouched (`add_login` only overwrites what it is given).
    #[allow(clippy::too_many_arguments)]
    pub fn store_in(
        &mut self,
        hosts: &mut Hosts,
        host: &HostKey,
        login: &str,
        slot: Slot,
        token: &SecretString,
        scopes: Vec<Scope>,
        kind: Option<&str>,
    ) -> Result<TokenSource> {
        // Record the identity first so `hosts.toml` is consistent even if the secret write
        // fails; being listed without a token yields "not authenticated", which is true.
        hosts.add_login(host, login, None, scopes, kind)?;

        let target = match (self.forced, self.insecure) {
            (Some(forced), _) => forced,
            (None, true) => CredentialStore::File,
            (None, false) => self.preference,
        };

        if target == CredentialStore::Keyring {
            let wrote = self.set_in(CredentialStore::Keyring, host, login, slot, token, hosts);
            match wrote {
                Ok(source) => {
                    hosts.set_cached_store(host, CredentialStore::Keyring);
                    hosts.save_if_dirty()?;
                    return Ok(source);
                }
                // Fall through to the file store. Losing the login entirely because the
                // keyring is absent would make `fjo` unusable on a server.
                Err(e) if matches!(*e.kind, ErrorKind::KeyringUnavailable { .. }) => {
                    self.warnings.push(*e.kind);
                }
                Err(e) => return Err(e),
            }
        }

        let kind = if target == CredentialStore::Env {
            // Storing into an environment variable is impossible; say so rather than
            // silently writing the token somewhere the user did not ask for.
            CredentialStore::Env
        } else {
            CredentialStore::File
        };
        let source = self.set_in(kind, host, login, slot, token, hosts)?;
        hosts.set_cached_store(host, kind);
        hosts.save_if_dirty()?;
        Ok(source)
    }

    /// Removes **every** credential for a login, from every store that could hold one. Keyring
    /// failures are warnings: `auth logout` must still clear `hosts.toml`.
    ///
    /// Iterating [`Slot::ALL`] rather than naming the slots is what makes an orphaned
    /// credential structurally impossible: a slot added to that array is a slot logout already
    /// clears, and one added without it fails `every_slot_is_in_all`. `oauth/stored.rs` records
    /// the orphaning risk that made a second keyring entry unattractive; this is the answer to
    /// it.
    pub fn forget(&mut self, hosts: &mut Hosts, host: &HostKey, login: &str) -> Result<()> {
        for (kind, slot) in [CredentialStore::Keyring, CredentialStore::File, CredentialStore::Env]
            .into_iter()
            .flat_map(|k| Slot::ALL.map(move |s| (k, s)))
        {
            let r = self.delete_in(kind, host, login, slot, hosts);
            if let Err(e) = r {
                if matches!(*e.kind, ErrorKind::KeyringUnavailable { .. }) {
                    self.warnings.push(*e.kind);
                } else {
                    return Err(e);
                }
            }
        }
        hosts.save_if_dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MapEnv;

    fn fixture() -> (tempfile::TempDir, Hosts, HostKey) {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = Hosts::empty_at(&dir.path().join("hosts.toml"));
        let key = hosts.add_host("https://git.example.org").unwrap().name.clone();
        hosts.add_login(&key, "perf3ct", None, vec![], None).unwrap();
        (dir, hosts, key)
    }

    /// Bug this prevents: a slot added to the enum but not to `ALL`, which would make
    /// `forget` silently leave that credential behind — exactly the orphaning that
    /// `oauth/stored.rs` warns a second entry invites. The match is exhaustive, so adding a
    /// variant fails to compile here until `ALL` is extended.
    #[test]
    fn every_slot_is_in_all() {
        for slot in Slot::ALL {
            // Exhaustive by construction: a new variant makes this arm-less match an error.
            match slot {
                Slot::Api | Slot::Web => {}
            }
        }
        assert_eq!(Slot::ALL.len(), 2, "a new Slot variant must be added to Slot::ALL");
        // Distinct keys, or one credential would overwrite the other.
        assert_ne!(Slot::Api.prefix(), Slot::Web.prefix());
        // The documented account layout for the API token is unchanged, forever.
        assert_eq!(Slot::Api.prefix(), "");
    }

    /// Bug this prevents: a web session and an API token colliding in one store, so that
    /// logging in one way silently destroys the other.
    #[test]
    fn the_two_slots_do_not_share_a_keyring_entry() {
        let (_d, mut hosts, key) = fixture();
        let ring = FakeKeyring::new();
        ring.set(&key, "perf3ct", Slot::Api, &"pat".into(), &mut hosts).unwrap();
        ring.set(&key, "perf3ct", Slot::Web, &"web".into(), &mut hosts).unwrap();

        let api = ring.get(&key, "perf3ct", Slot::Api, &hosts).unwrap().unwrap();
        let web = ring.get(&key, "perf3ct", Slot::Web, &hosts).unwrap().unwrap();
        assert_eq!(api.expose(), "pat");
        assert_eq!(web.expose(), "web");

        // And deleting one leaves the other.
        ring.delete(&key, "perf3ct", Slot::Web, &mut hosts).unwrap();
        assert!(ring.get(&key, "perf3ct", Slot::Web, &hosts).unwrap().is_none());
        assert!(ring.get(&key, "perf3ct", Slot::Api, &hosts).unwrap().is_some());
    }

    #[test]
    fn keyring_round_trip() {
        let (_d, mut hosts, key) = fixture();
        let env = MapEnv::new();
        let mut creds = Credentials::new(&env).with_keyring(Box::new(FakeKeyring::new()));

        let source = creds
            .store(
                &mut hosts,
                &key,
                "perf3ct",
                &"tok-123".into(),
                vec!["read:repository".into()],
                None,
            )
            .unwrap();
        assert!(matches!(source, TokenSource::Keyring { .. }));
        // The token must NOT be in hosts.toml when the keyring is in use.
        let on_disk = std::fs::read_to_string(hosts.path()).unwrap();
        assert!(!on_disk.contains("tok-123"), "{on_disk}");

        let got = creds.token(&mut hosts, &key, "perf3ct").unwrap().unwrap();
        assert_eq!(got.expose(), "tok-123");
        assert!(matches!(got.source(), TokenSource::Keyring { .. }));
        assert!(creds.warnings().is_empty());
    }

    #[test]
    fn keyring_unavailable_degrades_to_the_file_store() {
        // Regression net for the headless / container / CI / SSH case: a missing D-Bus
        // Secret Service must be a warning, not a failure.
        let (_d, mut hosts, key) = fixture();
        hosts.add_login(&key, "perf3ct", Some("file-tok".into()), vec![], None).unwrap();
        let env = MapEnv::new();
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::unavailable(KeyringCause::NoBackend)));

        let got = creds.token(&mut hosts, &key, "perf3ct").unwrap().unwrap();
        assert_eq!(got.expose(), "file-tok");
        assert!(matches!(got.source(), TokenSource::File { .. }));
        assert!(matches!(
            creds.warnings().first(),
            Some(ErrorKind::KeyringUnavailable { cause: KeyringCause::NoBackend })
        ));
        // ...and the failure is cached so the next run does not retry the dead bus.
        assert_eq!(hosts.cached_store(&key), Some(CredentialStore::File));
    }

    #[test]
    fn keyring_unavailable_degrades_to_env() {
        let (_d, mut hosts, key) = fixture();
        let env = MapEnv::new().with("FORGEJO_TOKEN", "env-tok");
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::unavailable(KeyringCause::Timeout)));

        let got = creds.token(&mut hosts, &key, "perf3ct").unwrap().unwrap();
        assert_eq!(got.expose(), "env-tok");
        assert_eq!(got.source(), &TokenSource::Env { var: "FORGEJO_TOKEN".into() });
    }

    #[test]
    fn keyring_unavailable_on_store_falls_back_to_the_file() {
        // Losing the login entirely because a server has no keyring would make the tool
        // unusable exactly where it is used most.
        let (_d, mut hosts, key) = fixture();
        let env = MapEnv::new();
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::unavailable(KeyringCause::NoBackend)));
        let source = creds.store(&mut hosts, &key, "perf3ct", &"tok".into(), vec![], None).unwrap();
        assert!(matches!(source, TokenSource::File { .. }));
        assert!(!creds.warnings().is_empty());
        let on_disk = std::fs::read_to_string(hosts.path()).unwrap();
        assert!(on_disk.contains("tok"));
    }

    #[test]
    fn fjo_token_beats_forgejo_token() {
        let (_d, hosts, key) = fixture();
        let env = MapEnv::new().with("FJO_TOKEN", "a").with("FORGEJO_TOKEN", "b");
        let got = EnvStore::new(&env).get(&key, "perf3ct", Slot::Api, &hosts).unwrap().unwrap();
        assert_eq!(got.expose(), "a");
        assert_eq!(got.source(), &TokenSource::Env { var: "FJO_TOKEN".into() });
    }

    #[test]
    fn credential_store_env_is_a_hard_override() {
        // FJO_CREDENTIAL_STORE=env must not silently fall back to a keyring token: CI
        // wants a deterministic answer, including a deterministic failure.
        let (_d, mut hosts, key) = fixture();
        let env = MapEnv::new().with(STORE_VAR, "env");
        let fake = FakeKeyring::new();
        let mut creds = Credentials::new(&env).with_keyring(Box::new(fake));
        assert!(creds.token(&mut hosts, &key, "perf3ct").unwrap().is_none());
        assert_eq!(creds.effective_store(&hosts, &key), CredentialStore::Env);
        // And it refuses to pretend it saved anything.
        assert!(creds.store(&mut hosts, &key, "perf3ct", &"x".into(), vec![], None).is_err());
    }

    #[test]
    fn a_one_off_store_override_is_never_written_back() {
        // Found by running the real binary: a single
        //   FJO_CREDENTIAL_STORE=env fjo api user
        // persisted `credential_store = "env"` into hosts.toml. Every later run — with the
        // variable now unset — consulted only the environment, found nothing, and reported
        // "you are not logged in" while the token sat in the file one line below.
        //
        // The cache is for remembering a probe that FAILED, not an instruction about one run.
        let (_d, mut hosts, key) = fixture();

        // A token really is present in the file, and reachable via the env for this one run.
        let empty = MapEnv::new();
        let mut writer = Credentials::new(&empty)
            .with_keyring(Box::new(FakeKeyring::new()))
            .insecure_storage(true);
        writer.store(&mut hosts, &key, "perf3ct", &"filetok".into(), vec![], None).unwrap();
        let before = hosts.cached_store(&key);

        let env = MapEnv::new().with(STORE_VAR, "env").with("FORGEJO_TOKEN", "envtok");
        let mut creds = Credentials::new(&env).with_keyring(Box::new(FakeKeyring::new()));
        let got = creds.token(&mut hosts, &key, "perf3ct").unwrap().unwrap();
        assert_eq!(got.expose(), "envtok", "the override should be honoured for this run");

        assert_eq!(
            hosts.cached_store(&key),
            before,
            "FJO_CREDENTIAL_STORE must not change the cached store"
        );

        // And with the override gone, the file token is found again.
        let mut plain = Credentials::new(&empty)
            .with_keyring(Box::new(FakeKeyring::new()))
            .insecure_storage(true);
        assert_eq!(
            plain.token(&mut hosts, &key, "perf3ct").unwrap().unwrap().expose(),
            "filetok",
            "the stored token must still be reachable after an overridden run"
        );
    }

    #[test]
    fn insecure_storage_selects_the_file() {
        let (_d, mut hosts, key) = fixture();
        let env = MapEnv::new();
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::new()))
            .insecure_storage(true);
        let source = creds.store(&mut hosts, &key, "perf3ct", &"tok".into(), vec![], None).unwrap();
        assert!(matches!(source, TokenSource::File { .. }));
        assert!(std::fs::read_to_string(hosts.path()).unwrap().contains("tok"));
    }

    #[test]
    fn bad_store_var_warns_and_is_ignored() {
        let env = MapEnv::new().with(STORE_VAR, "vault");
        let creds = Credentials::new(&env);
        assert!(matches!(creds.warnings().first(), Some(ErrorKind::Usage(_))));
        assert!(creds.forced.is_none());
    }

    #[test]
    fn cached_probe_result_skips_the_keyring() {
        // The whole point of caching: no D-Bus round trip on every invocation.
        let (_d, mut hosts, key) = fixture();
        hosts.set_cached_store(&key, CredentialStore::File);
        hosts.add_login(&key, "perf3ct", Some("file-tok".into()), vec![], None).unwrap();
        let env = MapEnv::new();
        // A keyring that would panic the test if consulted.
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::unavailable(KeyringCause::NoBackend)));
        let got = creds.token(&mut hosts, &key, "perf3ct").unwrap().unwrap();
        assert_eq!(got.expose(), "file-tok");
        assert!(creds.warnings().is_empty(), "keyring was consulted despite the cache");
    }

    #[test]
    fn token_debug_is_redacted() {
        let t = Token::new("s3cr3t".into(), TokenSource::Flag);
        assert!(!format!("{t:?}").contains("s3cr3t"));
    }

    #[test]
    fn timeout_gives_up_rather_than_hanging() {
        // Stands in for a session bus that never answers.
        let start = std::time::Instant::now();
        let r = with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(30));
            Ok::<(), keyring::Error>(())
        });
        assert_eq!(r.unwrap_err(), KeyringCause::Timeout);
        assert!(start.elapsed() < Duration::from_secs(5), "waited {:?}", start.elapsed());
    }

    #[test]
    fn forget_clears_the_file_even_if_the_keyring_is_broken() {
        let (_d, mut hosts, key) = fixture();
        hosts.add_login(&key, "perf3ct", Some("tok".into()), vec![], None).unwrap();
        let env = MapEnv::new();
        let mut creds = Credentials::new(&env)
            .with_keyring(Box::new(FakeKeyring::unavailable(KeyringCause::Denied)));
        creds.forget(&mut hosts, &key, "perf3ct").unwrap();
        assert!(hosts.get(&key).unwrap().login("perf3ct").unwrap().token.is_none());
        assert!(!creds.warnings().is_empty());
    }
}

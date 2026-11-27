//! Shared plumbing for the setup surface (`auth`, `config`, `alias`, `completion`, `status`).
//!
//! It lives under `auth` because `auth login` is the command that forces the shape: every other
//! group may assume a configured host and reach for [`crate::runtime::Runtime`], while `auth
//! login` runs *before* a host exists and therefore has to assemble `Config`, `Hosts` and
//! `Credentials` by hand. Rather than have two ways to read the configuration directory, the
//! by-hand path is written once here and the rest of the setup surface uses it too.
//!
//! The general plumbing this once carried — the stderr note, the prompting gate, the
//! confirmation, the compiled `--json`/`--jq`/`--template` triad — now lives in
//! [`crate::cmd::support`]. What is left is the part that is genuinely about authentication.
//!
//! # The token never becomes a named `String`
//!
//! `secrecy` is deliberately not a dependency of this crate, so `SecretString` cannot be *named*
//! here. That is a feature: every place a plaintext token exists is a `&str` passed straight into
//! either an `Authorization` header or [`gitea_core::config::Credentials::store`], and there is
//! no local binding for a `grep` to find or a `Debug` to print.

use gitea_core::config::secrets::{CredentialStore, Token};
use gitea_core::config::{Config, Credentials, Env, HostEntry, HostKey, Hosts, SystemEnv};
use gitea_core::error::{ErrorKind, Result, TokenSource};
use gitea_core::http::{Auth, Client, Credentials as HttpCredentials};
use gitea_core::oauth::StoredOauth;

/// The process environment as a `'static`, so [`Credentials`] can borrow it for the life of the
/// command.
///
/// Mirrors [`crate::runtime`]'s own static rather than sharing it: that one is private, and
/// `SystemEnv` is a zero-sized type, so a second one costs nothing.
static SYS_ENV: SystemEnv = SystemEnv;

pub fn env() -> &'static dyn Env {
    &SYS_ENV
}

/// `config.toml` plus `hosts.toml`, with `hosts.toml`'s load-time warnings already reported.
///
/// Deliberately *not* [`crate::runtime::Runtime`]: `Runtime::new` resolves a host and fails with
/// `no Gitea host is set up yet`, which is exactly the state `auth login` exists to leave.
pub struct Setup {
    pub config: Config,
    pub hosts: Hosts,
}

impl Setup {
    pub fn load() -> Result<Self> {
        let config = Config::load(env())?;
        let mut hosts = Hosts::load_at(&config.hosts_path())?;
        for kind in hosts.take_warnings() {
            warn(&kind);
        }
        Ok(Self { config, hosts })
    }

    /// A fresh [`Credentials`] honouring the stored `credential_store` preference for `host`.
    pub fn credentials(&self, host: Option<&HostKey>) -> Credentials<'static> {
        Credentials::new(env())
            .with_preference(self.config.credential_store(host.map(HostKey::as_str)))
    }
}

/// Report a survivable problem on stderr, keeping the remedy the renderer attaches to it.
pub fn warn(kind: &ErrorKind) {
    crate::exit::warn(kind, crate::runtime::diagnostic_color());
}

// ------------------------------------------------------------------------------ token check

/// A client for one host and one candidate token.
///
/// `source` is threaded through so a 401 can name *where* the rejected token came from — the
/// whole difference between "your token was rejected" and "the token in your keyring under
/// `gea:me@git.example.org` was rejected".
pub fn client_for(entry: &HostEntry, token: &str, source: TokenSource) -> Result<Client> {
    Client::builder(&entry.url, HttpCredentials::new(Auth::token(token)))
        .user_agent(crate::runtime::user_agent())
        .token_source(source)
        .build()
}

/// A client for a resolved [`Credential`], sending the scheme that credential actually is.
///
/// The bug this prevents: checking an OAuth session with `Authorization: token <jwt>`. Gitea
/// happens to accept that today because its parser tries a JWT parse first, so the mistake would
/// not show up until the day it stopped being true.
pub fn client_for_kind(
    entry: &HostEntry,
    credential: &Credential,
    source: TokenSource,
) -> Result<Client> {
    let auth = match credential {
        Credential::Pat(_) => Auth::token(credential.expose()),
        Credential::Oauth(_) => Auth::bearer(credential.expose()),
    };
    Client::builder(&entry.url, HttpCredentials::new(auth))
        .user_agent(crate::runtime::user_agent())
        .token_source(source)
        .build()
}

/// `GET /user`, which is the only honest test of a token.
///
/// Used by `login` to discover the login a token *actually* belongs to, and by `status` to answer
/// "does this still work?".
pub async fn whoami(client: &Client) -> Result<gitea_model::User> {
    gitea_client::ops::User::new(client).get_current().await
}

/// A resolved credential, whichever kind the store held.
///
/// Both `auth token` and `auth git-credential` read a credential and emit a secret, and both
/// must emit the *access token* for an OAuth session rather than the stored document — which
/// also carries the refresh token. Having one type with one `expose` is what keeps that from
/// being two independent chances to print the wrong thing.
pub enum Credential {
    Pat(Token),
    /// Boxed: `StoredOauth` is much the larger variant, and this is returned by value.
    Oauth(Box<StoredOauth>),
}

impl Credential {
    /// Classify a stored value. A personal access token does not parse as a document.
    pub fn new(token: Token) -> Self {
        match StoredOauth::parse(token.expose()) {
            Some(s) => Self::Oauth(Box::new(s)),
            None => Self::Pat(token),
        }
    }

    /// The secret to send, which for an OAuth session is the access token and nothing else.
    pub fn expose(&self) -> &str {
        match self {
            Self::Pat(t) => t.expose(),
            Self::Oauth(s) => s.expose_access(),
        }
    }

    pub fn session(&self) -> Option<&StoredOauth> {
        match self {
            Self::Oauth(s) => Some(s),
            Self::Pat(_) => None,
        }
    }
}

/// A human label for a credential store, as `auth status` and `auth login` print it.
///
/// The file store's mode is part of the label on purpose: a user who silently fell back to it
/// needs to know the token is on disk, and that it is not world-readable.
pub fn store_label(store: CredentialStore) -> &'static str {
    match store {
        CredentialStore::Keyring => "the operating system keyring",
        CredentialStore::File => "hosts.toml (mode 0600)",
        CredentialStore::Env => "an environment variable",
    }
}

/// Which store a [`TokenSource`] came out of.
///
/// `Credentials::store` returns the source rather than the store kind, and the source is the
/// authoritative answer to "where did it actually end up?" — the *cached* `credential_store` in
/// `hosts.toml` can lag behind a fallback that has just happened.
pub fn store_of(source: &TokenSource) -> CredentialStore {
    match source {
        TokenSource::Keyring { .. } => CredentialStore::Keyring,
        TokenSource::File { .. } => CredentialStore::File,
        // `Flag` cannot be a *stored* source; treat it as the environment, which is the other
        // read-only one. Reachable only if a future `TokenSource` variant is added.
        TokenSource::Env { .. } | TokenSource::Flag => CredentialStore::Env,
    }
}

/// Where a token was just put, as a full sentence fragment for `auth login`'s report.
///
/// Separate from [`store_label`] because the useful detail differs per store: for the keyring it is
/// the entry name (so `secret-tool` can find it), and for the file it is the path *and* the mode
/// (so a user who fell back knows the token is on disk and who can read it).
pub fn stored_in(source: &TokenSource) -> String {
    match source {
        TokenSource::Keyring { entry } => {
            format!("the operating system keyring, entry {entry}")
        }
        TokenSource::File { path } => format!("{} at mode 0600", path.display()),
        TokenSource::Env { var } => format!("${var}"),
        TokenSource::Flag => "a command-line flag".to_owned(),
    }
}

/// Where a token came from, as a short phrase. Never contains the token itself.
pub fn source_label(source: &TokenSource) -> String {
    match source {
        TokenSource::Keyring { entry } => format!("keyring entry {entry}"),
        TokenSource::File { path } => path.display().to_string(),
        TokenSource::Env { var } => format!("${var}"),
        TokenSource::Flag => "a command-line flag".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: a store label that names the file but not its mode, so a user who fell
    /// back to the file store has no idea whether their token is world-readable.
    #[test]
    fn every_store_label_says_where_the_token_is() {
        assert!(store_label(CredentialStore::File).contains("0600"));
        assert!(store_label(CredentialStore::Keyring).contains("keyring"));
        assert!(store_label(CredentialStore::Env).contains("environment"));
    }

    /// The token must never appear in a source label; only the place that holds it.
    #[test]
    fn a_source_label_names_the_place_not_the_secret() {
        assert_eq!(
            source_label(&TokenSource::Env { var: "GITEA_TOKEN".to_owned() }),
            "$GITEA_TOKEN"
        );
        assert_eq!(
            source_label(&TokenSource::Keyring { entry: "gea:me@h".to_owned() }),
            "keyring entry gea:me@h"
        );
    }
}

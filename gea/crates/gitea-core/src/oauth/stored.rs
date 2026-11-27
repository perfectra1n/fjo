//! How an OAuth2 credential is written to the credential store.
//!
//! # Why this is one document and not three fields
//!
//! An OAuth session is three values that must agree: the access token, the refresh token that
//! will replace it, and when the first one lapses. Gitea rotates the refresh token on **every**
//! refresh, so those three change together, perhaps once an hour, for the life of the session.
//!
//! [`CredStore`](crate::config::CredStore) holds exactly one secret per `(host, login)`. Splitting
//! the three across it and `hosts.toml` would make each rotation two writes to two backends with
//! no transaction between them, and a crash in the gap leaves a refresh token the server has
//! already invalidated next to an access token that still works — a session that appears healthy
//! for an hour and then cannot be recovered. Keeping them in one value means there is no gap:
//! `KeyringStore::set` is a single call, and the file store goes through `write_atomic`, which is
//! a temp file and a rename.
//!
//! It also leaves the keyring layout alone. The service name and the `{login}@{host}` account key
//! are documented as stable forever, and a second entry for the refresh token would be one more
//! thing to orphan on logout.
//!
//! # Compatibility
//!
//! An older `gea` reading one of these sends `Authorization: token {"v":1,...}` and gets a 401
//! whose existing advice is to log in again. That is deliberate. The alternative — shaping the
//! document so an old build half-works — trades a loud, correct error for a credential that
//! functions for an hour and then fails with no explanation.

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{CredentialKind, Error, ErrorKind, Result};
use crate::oauth::TokenResponse;

/// Schema version. Bumped only if the shape changes incompatibly; an unknown version parses as
/// "not ours" and produces the same clean 401 as any other unusable credential.
const VERSION: u8 = 1;

/// The discriminator. A personal access token is an opaque hex string, so nothing else in the
/// store can be mistaken for one of these.
const KIND: &str = "oauth2";

/// An OAuth2 session, as stored.
pub struct StoredOauth {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    /// When the access token lapses. Absolute rather than a duration, because the value sits on
    /// disk between runs and "3600 seconds from when?" has no answer there.
    pub expires_at: Timestamp,
    /// Recorded so a refresh does not have to guess which application the session belongs to.
    pub client_id: String,
    /// Recorded so a refresh is one request rather than a rediscovery followed by one.
    pub token_endpoint: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u8,
    kind: String,
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    expires_at: Timestamp,
    client_id: String,
    token_endpoint: String,
}

impl StoredOauth {
    /// Build from a fresh token response.
    pub fn from_response(
        r: &TokenResponse,
        client_id: &str,
        token_endpoint: &str,
        now: Timestamp,
    ) -> Self {
        Self {
            access_token: r.access_token.clone(),
            refresh_token: r.refresh_token.clone(),
            expires_at: now + SignedDuration::from_secs(r.expires_in),
            client_id: client_id.to_owned(),
            token_endpoint: token_endpoint.to_owned(),
        }
    }

    /// Read a stored credential, or `None` when it is not one of these.
    ///
    /// `None` is the ordinary answer, not an error: the overwhelmingly common credential is a
    /// personal access token, and this runs on every command that authenticates. The `{` check
    /// keeps that path to one byte comparison rather than a failed JSON parse.
    pub fn parse(raw: &str) -> Option<Self> {
        if !raw.trim_start().starts_with('{') {
            return None;
        }
        let mut w: Wire = serde_json::from_str(raw).ok()?;
        if w.v != VERSION || w.kind != KIND {
            return None;
        }
        let out = Self {
            access_token: SecretString::from(w.access_token.clone()),
            refresh_token: SecretString::from(w.refresh_token.clone()),
            expires_at: w.expires_at,
            client_id: w.client_id.clone(),
            token_endpoint: w.token_endpoint.clone(),
        };
        w.access_token.zeroize();
        w.refresh_token.zeroize();
        Some(out)
    }

    /// Serialise for the credential store.
    pub fn to_json(&self) -> Result<SecretString> {
        let w = Wire {
            v: VERSION,
            kind: KIND.to_owned(),
            access_token: self.access_token.expose_secret().to_owned(),
            refresh_token: self.refresh_token.expose_secret().to_owned(),
            expires_at: self.expires_at,
            client_id: self.client_id.clone(),
            token_endpoint: self.token_endpoint.clone(),
        };
        let json = serde_json::to_string(&w).map_err(|e| {
            // Serialising our own struct cannot fail on user input, so this is our bug.
            Error::new(ErrorKind::Usage(format!("could not store the OAuth session: {e}")))
        })?;
        Ok(SecretString::from(json))
    }

    /// Whether the access token lapses within `skew`.
    ///
    /// Refreshing early costs a fraction of the token's hour and buys immunity to clock drift
    /// between this machine and the server, plus the whole duration of whatever command is about
    /// to run. Refreshing late means a 401 the user sees.
    pub fn is_expiring(&self, skew: Duration, now: Timestamp) -> bool {
        self.expires_at.duration_since(now) <= SignedDuration::from_secs(skew.as_secs() as i64)
    }

    /// The access token as a `&str`, for the two places that must actually send it.
    ///
    /// Mirrors `Token::expose`, and exists for the same reason: the `gea` crate deliberately
    /// does not depend on `secrecy`, so exposing has to happen behind a method here rather than
    /// through an `ExposeSecret` call at a call site that could too easily bind the result to a
    /// named `String`.
    pub fn expose_access(&self) -> &str {
        self.access_token.expose_secret()
    }

    /// Whether there is anything to refresh with.
    pub fn can_refresh(&self) -> bool {
        !self.refresh_token.expose_secret().is_empty()
    }
}

/// Hand-written: two secrets, and a derive here would print both.
impl std::fmt::Debug for StoredOauth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredOauth")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .field("token_endpoint", &self.token_endpoint)
            .finish()
    }
}

/// What kind of credential a stored value is, without committing to parsing it fully.
pub fn kind_of(raw: &str) -> CredentialKind {
    match StoredOauth::parse(raw) {
        Some(_) => CredentialKind::Oauth2,
        None => CredentialKind::Pat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoredOauth {
        StoredOauth {
            access_token: SecretString::from("eyJhbGciOiJIUzUxMiJ9.e30.sig".to_owned()),
            refresh_token: SecretString::from("the-refresh-token".to_owned()),
            expires_at: "2026-09-18T12:00:00Z".parse().expect("a valid timestamp"),
            client_id: "cid".to_owned(),
            token_endpoint: "https://git.example.org/login/oauth/access_token".to_owned(),
        }
    }

    /// Bug this prevents: a personal access token being mistaken for a stored session, which
    /// would make every existing login stop working on upgrade.
    #[test]
    fn a_personal_access_token_does_not_parse_as_an_oauth_document() {
        for raw in [
            "65eaa9c8ef52460d22a93307fe0aee76289dc675",
            "eyJhbGciOiJIUzUxMiJ9.e30.sig",
            "",
            "   ",
            r#"{"not":"ours"}"#,
            // A future version must not be read with today's meaning.
            r#"{"v":99,"kind":"oauth2","access_token":"a","refresh_token":"r","expires_at":"2026-09-18T12:00:00Z","client_id":"c","token_endpoint":"t"}"#,
        ] {
            assert!(StoredOauth::parse(raw).is_none(), "{raw:?} should not parse");
            assert_eq!(kind_of(raw), CredentialKind::Pat, "{raw:?}");
        }
    }

    #[test]
    fn a_stored_session_round_trips() {
        let json = sample().to_json().expect("serialisable");
        let back = StoredOauth::parse(json.expose_secret()).expect("our own document");
        assert_eq!(back.access_token.expose_secret(), sample().access_token.expose_secret());
        assert_eq!(back.refresh_token.expose_secret(), sample().refresh_token.expose_secret());
        assert_eq!(back.expires_at, sample().expires_at);
        assert_eq!(back.client_id, "cid");
        assert_eq!(back.token_endpoint, sample().token_endpoint);
        assert_eq!(kind_of(json.expose_secret()), CredentialKind::Oauth2);
    }

    #[test]
    fn an_expiring_token_is_detected_before_it_lapses() {
        let s = sample();
        let skew = Duration::from_secs(300);
        let at: Timestamp = "2026-09-18T12:00:00Z".parse().expect("valid");

        let comfortable: Timestamp = "2026-09-18T11:50:00Z".parse().expect("valid");
        assert!(!s.is_expiring(skew, comfortable), "ten minutes out is not expiring");

        let inside: Timestamp = "2026-09-18T11:58:00Z".parse().expect("valid");
        assert!(s.is_expiring(skew, inside), "two minutes out is inside the skew");

        assert!(s.is_expiring(skew, at), "the moment it lapses counts as expiring");
        let after: Timestamp = "2026-09-18T13:00:00Z".parse().expect("valid");
        assert!(s.is_expiring(skew, after), "already lapsed counts as expiring");
    }

    /// A session with no refresh token is usable until it lapses and then gone. Attempting a
    /// refresh with an empty string would earn a confusing server error instead.
    #[test]
    fn a_session_without_a_refresh_token_knows_it_cannot_refresh() {
        let mut s = sample();
        assert!(s.can_refresh());
        s.refresh_token = SecretString::from(String::new());
        assert!(!s.can_refresh());
    }

    #[test]
    fn a_stored_session_debug_prints_neither_token() {
        let d = format!("{:?}", sample());
        assert!(!d.contains("the-refresh-token"), "{d}");
        assert!(!d.contains("eyJhbGciOiJIUzUxMiJ9"), "{d}");
        assert!(d.contains("cid"), "the client id is not a secret and is useful: {d}");
    }
}

//! How a web credential is written to the credential store.
//!
//! # Why this is one document, and why it is not the *same* document as the API token
//!
//! A web credential is two values that must agree: the remember token, and the session minted
//! from it. They change together — every re-mint replaces the session — so they are one document
//! for exactly the reason [`crate::oauth::StoredOauth`] is one: splitting values that rotate
//! together across two backends means a crash in the gap leaves a pair that does not match.
//!
//! They are *not* stored with the API token, because that pair does **not** rotate together. An
//! API token is untouched for months while a session may be re-minted several times an hour;
//! packing them together would mean rewriting a perfectly good token on every renewal, which is
//! the opposite of what the one-document rule protects. They live in separate
//! [`crate::config::Slot`]s instead.
//!
//! # Compatibility
//!
//! An older `fjo` never reads this: it is in a slot that build does not know about. The worst it
//! can do is drop the `hosts.toml` field when it saves the file, which costs one re-login and
//! says so — see `Login::web_session`.

use jiff::Timestamp;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};

/// Schema version. An unknown version parses as "not ours", producing the same clean
/// "log in again" as any other unusable credential rather than a confusing parse error.
const VERSION: u8 = 1;

/// The discriminator, so nothing else in a credential store can be mistaken for one of these.
const KIND: &str = "web-session";

/// A web sign-in, as stored.
#[derive(Serialize, Deserialize)]
pub struct WebCredential {
    v: u8,
    kind: String,
    /// The account this belongs to, so `auth status` can name it without a request.
    pub user: String,
    /// The long-term authorization token from the `persistent` cookie. The only thing a
    /// password is ever exchanged for.
    #[serde(with = "secret")]
    pub remember: SecretString,
    /// When the server said the remember token lapses.
    ///
    /// Taken from the `Max-Age` on the `Set-Cookie`, never from Forgejo's documented default:
    /// `LOGIN_REMEMBER_DAYS` is configurable, and a client that assumed 31 days would report a
    /// confident wrong date on any instance that changed it.
    pub remember_expires_at: Timestamp,
    /// The current session cookie, when one has been minted.
    ///
    /// `None` is normal and not an error: it means the next request will mint one. There is no
    /// expiry recorded beside it **on purpose** — a client cannot know the server's
    /// `SESSION_LIFE_TIME`, and guessing would either re-mint needlessly or trust a dead
    /// session. The server's `303` is the only signal used.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_secret")]
    pub session: Option<SecretString>,
}

impl std::fmt::Debug for WebCredential {
    /// Hand-written: a derive would print both secrets, and this is what `--debug` shows.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebCredential")
            .field("user", &self.user)
            .field("remember", &"<redacted>")
            .field("remember_expires_at", &self.remember_expires_at)
            .field("session", &self.session.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl WebCredential {
    pub fn new(user: impl Into<String>, remember: SecretString, expires: Timestamp) -> Self {
        Self {
            v: VERSION,
            kind: KIND.to_owned(),
            user: user.into(),
            remember,
            remember_expires_at: expires,
            session: None,
        }
    }

    /// Parse a stored value, or `None` when it is not one of these.
    ///
    /// `None` rather than an error, so that a slot holding something unexpected — an API token
    /// written there by hand, a document from a future version — is treated as "no session" and
    /// resolved by logging in, rather than failing the command with a parse error.
    pub fn parse(raw: &str) -> Option<Self> {
        let doc: Self = serde_json::from_str(raw).ok()?;
        (doc.v == VERSION && doc.kind == KIND).then_some(doc)
    }

    pub fn to_json(&self) -> Result<SecretString> {
        serde_json::to_string(self)
            .map(SecretString::from)
            .map_err(|e| Error::new(ErrorKind::Usage(format!("could not store the session: {e}"))))
    }

    /// The stored document as a plain `String`, for deliberate export.
    ///
    /// Deliberately not a `SecretString`: the one caller is `fjo auth export --web`, whose whole
    /// job is to put this on stdout, and `secrecy` is not a dependency of the `fjo` crate. A
    /// name this explicit is also the point — `to_export_json` reads as a decision at the call
    /// site in a way that `.expose_secret()` on a general accessor would not, so the guards that
    /// must accompany it (refuse a terminal, warn on stderr) are visibly attached to it.
    pub fn to_export_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| Error::new(ErrorKind::Usage(format!("could not export the session: {e}"))))
    }

    /// Whether the remember token is within `skew` of lapsing.
    ///
    /// Used only to warn: this is the one failure that cannot be recovered without a password,
    /// so a scheduled job deserves to see it coming rather than to discover it at 3am.
    pub fn is_expiring(&self, skew: jiff::SignedDuration, now: Timestamp) -> bool {
        // `checked_add` on `now`, not `checked_sub` on the expiry: subtracting from a timestamp
        // near the start of the representable range is the fallible direction, and a skew that
        // overflowed would silently answer "not expiring" for a credential that is.
        now.checked_add(skew).is_ok_and(|reach| reach >= self.remember_expires_at)
    }

    pub fn is_expired(&self, now: Timestamp) -> bool {
        self.remember_expires_at <= now
    }

    pub fn expose_remember(&self) -> &str {
        self.remember.expose_secret()
    }
}

/// `Max-Age=2592000` out of a `Set-Cookie` attribute string, as an absolute instant.
///
/// Returns `None` when the server sent no `Max-Age` — a session cookie, which is what
/// `persistent` looks like if the sign-in did not ask to be remembered. The caller treats that
/// as "this sign-in cannot be renewed" rather than inventing a lifetime for it.
pub fn max_age_from(attrs: &str, now: Timestamp) -> Option<Timestamp> {
    for part in attrs.split(';') {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("max-age") {
            let secs: i64 = v.trim().parse().ok()?;
            if secs <= 0 {
                return None;
            }
            return now.checked_add(jiff::SignedDuration::from_secs(secs)).ok();
        }
    }
    None
}

mod secret {
    use secrecy::{ExposeSecret, SecretString};
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(v: &SecretString, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.expose_secret())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SecretString, D::Error> {
        Ok(SecretString::from(String::deserialize(d)?))
    }
}

mod opt_secret {
    use secrecy::{ExposeSecret, SecretString};
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
        Ok(Option::<String>::deserialize(d)?.map(SecretString::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WebCredential {
        WebCredential::new(
            "perf3ct",
            SecretString::from("remember-me"),
            "2026-10-22T08:00:00Z".parse().expect("a valid timestamp"),
        )
    }

    #[test]
    fn a_document_round_trips() {
        let mut c = sample();
        c.session = Some(SecretString::from("sess-abc"));
        let raw = c.to_json().expect("serialises");
        let back = WebCredential::parse(raw.expose_secret()).expect("parses");
        assert_eq!(back.user, "perf3ct");
        assert_eq!(back.expose_remember(), "remember-me");
        assert_eq!(back.session.as_ref().map(|s| s.expose_secret()), Some("sess-abc"));
    }

    /// Bug this prevents: an API token in the web slot parsing as a session, which would send a
    /// personal access token as a cookie and fail with a baffling 303.
    #[test]
    fn nothing_else_parses_as_a_web_credential() {
        for raw in [
            "65eaa9c8ef52460d22a93307fe0aee76289dc675",
            "",
            "   ",
            r#"{"not":"ours"}"#,
            // A stored OAuth document, which lives in the other slot but must never be confused.
            r#"{"v":1,"kind":"oauth2","access_token":"a","refresh_token":"r"}"#,
            // A future version.
            r#"{"v":2,"kind":"web-session","user":"u","remember":"r","remember_expires_at":"2026-10-22T08:00:00Z"}"#,
        ] {
            assert!(WebCredential::parse(raw).is_none(), "parsed: {raw}");
        }
    }

    #[test]
    fn neither_secret_reaches_a_debug_line() {
        let mut c = sample();
        c.session = Some(SecretString::from("sess-abc"));
        let s = format!("{c:?}");
        assert!(!s.contains("remember-me"), "{s}");
        assert!(!s.contains("sess-abc"), "{s}");
        assert!(s.contains("redacted"), "{s}");
    }

    #[test]
    fn max_age_is_read_from_the_server_rather_than_assumed() {
        let now: Timestamp = "2026-09-21T08:00:00Z".parse().unwrap();
        let got = max_age_from("Path=/; Max-Age=2592000; HttpOnly; Secure", now).unwrap();
        assert_eq!(got, "2026-10-21T08:00:00Z".parse::<Timestamp>().unwrap());
        // Case and spacing as servers actually send them.
        assert!(max_age_from("path=/; max-age=60", now).is_some());
        // A cookie with no Max-Age is a session cookie: not renewable, so not a lifetime.
        assert!(max_age_from("Path=/; HttpOnly", now).is_none());
        assert!(max_age_from("Path=/; Max-Age=0", now).is_none());
    }

    #[test]
    fn expiry_warns_before_it_fails() {
        let c = sample();
        let day = jiff::SignedDuration::from_hours(24);
        let two_days_before: Timestamp = "2026-10-20T08:00:00Z".parse().unwrap();
        assert!(!c.is_expired(two_days_before));
        assert!(c.is_expiring(jiff::SignedDuration::from_hours(72), two_days_before));
        assert!(!c.is_expiring(day, two_days_before));
        assert!(c.is_expired("2026-10-23T08:00:00Z".parse().unwrap()));
    }
}

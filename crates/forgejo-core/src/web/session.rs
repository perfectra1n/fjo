//! Minting a session from a remember token, and recognising when one has lapsed.
//!
//! # The mechanism, and where the decision lives
//!
//! `GET /user/login` carrying the `persistent` cookie runs Forgejo's `autoSignIn`, which issues
//! a fresh `session` cookie and redirects away from the login page. That is the whole renewal:
//! no password, no second factor, no user.
//!
//! This module performs it and nothing else. Deciding *when* to renew, persisting the result and
//! retrying the original request belong to the caller, exactly as [`crate::oauth::refresh`]
//! leaves persistence to `fjo`'s `oauth_refresh`. The rule that module records applies here
//! unchanged: **write the new credential before relying on it**.
//!
//! # Why there is no expiry check
//!
//! A client cannot know a server's `SESSION_LIFE_TIME`, and Forgejo does not tell it. Any local
//! guess is wrong in one of two damaging ways: too short and every command pays a needless
//! round trip, too long and a dead session produces a `303` the caller was not expecting. So
//! the session is simply *used*, and [`WebResponse::redirects_to_login`] — the server's own
//! answer — is the only thing that triggers a renewal.

use http::Method;
use jiff::Timestamp;
use secrecy::SecretString;

use crate::error::{Error, ErrorKind, Result};
use crate::web::client::{Cookie, WebBody, WebClient, WebResponse};
use crate::web::stored::{WebCredential, max_age_from};
use crate::web::{REMEMBER_COOKIE, SESSION_COOKIE};

/// Exchange a remember token for a fresh session cookie.
///
/// Fails with [`ErrorKind::WebSessionExpired`] when the server declines, which is what a lapsed,
/// revoked, or password-change-invalidated remember token looks like: Forgejo answers
/// `GET /user/login` with the login page itself rather than a redirect away from it.
pub async fn remint(client: &WebClient, remember: &SecretString) -> Result<SecretString> {
    let cookie = Cookie::new(REMEMBER_COOKIE, remember.clone());
    let resp = client
        .send(Method::GET, "/user/login", WebBody::None, std::slice::from_ref(&cookie))
        .await?;

    // A successful auto-sign-in redirects AWAY from the login page and sets the session. A dead
    // token either renders the login page (200) or redirects back to it.
    if let Some(session) = resp.set_cookie(SESSION_COOKIE)
        && !resp.redirects_to_login()
    {
        return Ok(session);
    }
    Err(expired(client))
}

/// Whether a response means "this session is no longer signed in".
///
/// One function rather than an inline check at each call site, so that every caller agrees on
/// what the signal is — and so that a future Forgejo that answers `401` on some route can be
/// taught here once.
pub fn is_lapsed(resp: &WebResponse) -> bool {
    resp.redirects_to_login() || resp.status == http::StatusCode::UNAUTHORIZED
}

/// Update a credential in place with a freshly minted session.
///
/// Returns the document to persist. The caller writes it **before** issuing the retried
/// request: a session used but not stored is one the next invocation has to mint again, and a
/// server that rate-limits sign-ins would turn that into a failure that looks intermittent.
pub async fn renew(client: &WebClient, mut cred: WebCredential) -> Result<WebCredential> {
    let session = remint(client, &cred.remember).await?;
    cred.session = Some(session);
    Ok(cred)
}

/// Read the remember token and its lifetime out of a sign-in response.
pub(crate) fn remember_from(
    resp: &WebResponse,
    now: Timestamp,
) -> Option<(SecretString, Timestamp)> {
    let (value, attrs) = resp.set_cookie_attrs(REMEMBER_COOKIE)?;
    let expires = max_age_from(&attrs, now)?;
    Some((value, expires))
}

fn expired(client: &WebClient) -> Error {
    Error::new(ErrorKind::WebSessionExpired { host: host_of(client.web_base()) })
}

pub(crate) fn host_of(web_base: &str) -> String {
    let rest = web_base.split_once("://").map_or(web_base, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{Canned, FakeTransport};

    fn client(t: FakeTransport) -> WebClient {
        WebClient::with_transport("https://forge.test", t)
    }

    #[tokio::test]
    async fn a_live_remember_token_mints_a_session() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/")
                .with_header("set-cookie", "session=fresh-abc; Path=/; HttpOnly"),
        );
        let got = remint(&client(t), &SecretString::from("remember")).await.expect("mints");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&got), "fresh-abc");
    }

    /// The bug this exists for, with the header sequence a real Forgejo 16.0.5 sends.
    ///
    /// `RegenerateSession` issues a fresh id *after* writing the user id, so the response
    /// carries two `session` cookies. Taking the first stores a valid but **anonymous** session:
    /// public routes then work, private ones answer 404 rather than 401 or a redirect, and
    /// nothing in the session-lapsed path ever fires because the server never says "signed
    /// out". It presents as "the feature is broken for private repositories".
    #[tokio::test]
    async fn the_session_taken_is_the_one_issued_after_regeneration() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/")
                .with_header("set-cookie", "session=pre-regeneration; Path=/; HttpOnly")
                .with_header("set-cookie", "lang=en-US; Path=/; HttpOnly")
                .with_header("set-cookie", "session=after-regeneration; Path=/; HttpOnly"),
        );
        let got = remint(&client(t), &SecretString::from("remember")).await.expect("mints");
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&got),
            "after-regeneration",
            "the last Set-Cookie for a name is the one a browser keeps, and the only one that \
             is actually signed in"
        );
    }

    /// A later clear genuinely overrides an earlier set, so the order rule cuts both ways.
    #[tokio::test]
    async fn a_session_cleared_later_in_the_same_response_is_not_taken() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/")
                .with_header("set-cookie", "session=transient; Path=/")
                .with_header("set-cookie", "session=; Path=/; Max-Age=0"),
        );
        let err = remint(&client(t), &SecretString::from("remember")).await.expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebSessionExpired { .. }), "{err:?}");
    }

    /// Bug this prevents: treating a bounce back to the login page as success, which would store
    /// an empty or stale session and fail every later request with no explanation.
    #[tokio::test]
    async fn a_dead_remember_token_is_reported_as_expired_not_as_success() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::new(303).with_header("location", "/user/login"),
        );
        let err = remint(&client(t), &SecretString::from("stale")).await.expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebSessionExpired { .. }), "{err:?}");
    }

    /// The other shape a dead token takes: the login page rendered with 200.
    #[tokio::test]
    async fn a_rendered_login_page_is_also_expired() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::html(200, "<form action=\"/user/login\">"),
        );
        let err = remint(&client(t), &SecretString::from("stale")).await.expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebSessionExpired { .. }), "{err:?}");
    }
}

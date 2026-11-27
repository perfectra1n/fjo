//! Minting a session from a remember token, and recognising when one has lapsed.
//!
//! # The mechanism, and where the decision lives
//!
//! `GET /user/login` carrying the `gitea_incredible` cookie runs Gitea's `autoSignIn`, which issues
//! a fresh `i_like_gitea` session cookie and redirects away from the login page. That is the
//! whole renewal: no password, no second factor, no user.
//!
//! # The remember token is single-use
//!
//! Gitea **rotates** the remember token on every auto-sign-in: the same response carries a new
//! `gitea_incredible`, and the one that was presented stops working (measured against 1.27.3 —
//! presenting it a second time renders the login page). So a renewal yields two credentials,
//! and keeping only the session would leave a remember token that the *next* renewal is
//! guaranteed to find dead. [`Reminted::remember`] carries the replacement, and [`renew`] writes
//! it into the credential beside the session.
//!
//! This module performs it and nothing else. Deciding *when* to renew, persisting the result and
//! retrying the original request belong to the caller, exactly as [`crate::oauth::refresh`]
//! leaves persistence to `gea`'s `oauth_refresh`. The rule that module records applies here
//! unchanged: **write the new credential before relying on it**.
//!
//! # Why there is no expiry check
//!
//! A client cannot know a server's `SESSION_LIFE_TIME`, and Gitea does not tell it. Any local
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

/// What one auto-sign-in hands back.
#[derive(Debug)]
pub struct Reminted {
    /// The signed-in session.
    pub session: SecretString,
    /// The remember token that replaces the one presented, and when it lapses — `None` only if
    /// the server did not rotate it.
    pub remember: Option<(SecretString, Timestamp)>,
}

/// Exchange a remember token for a fresh session cookie (and, on Gitea, a fresh remember token).
///
/// Fails with [`ErrorKind::WebSessionExpired`] when the server declines, which is what a lapsed,
/// revoked, or already-used remember token looks like: Gitea answers `GET /user/login` with the
/// login page itself rather than a redirect away from it.
pub async fn remint(client: &WebClient, remember: &SecretString) -> Result<Reminted> {
    let cookie = Cookie::new(REMEMBER_COOKIE, remember.clone());
    let resp = client
        .send(Method::GET, "/user/login", WebBody::None, std::slice::from_ref(&cookie))
        .await?;

    // A successful auto-sign-in *redirects* away from the login page and sets the session. The
    // redirect is required, not incidental: a dead token gets the login page rendered with 200,
    // and Gitea sets a fresh — anonymous — session cookie on that page too. Accepting any
    // response that set a session stored that anonymous one, and every private route then
    // answered 404 for the rest of the session's life.
    if resp.status.is_redirection()
        && !resp.redirects_to_login()
        && let Some(session) = resp.set_cookie(SESSION_COOKIE)
    {
        return Ok(Reminted { session, remember: remember_from(&resp, Timestamp::now()) });
    }
    Err(expired(client))
}

/// Whether a response means "this session is no longer signed in".
///
/// One function rather than an inline check at each call site, so that every caller agrees on
/// what the signal is — and so that a future Gitea that answers `401` on some route can be
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
    let fresh = remint(client, &cred.remember).await?;
    cred.session = Some(fresh.session);
    if let Some((remember, expires)) = fresh.remember {
        cred.remember = remember;
        cred.remember_expires_at = expires;
    }
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
                .with_header("set-cookie", "i_like_gitea=fresh-abc; Path=/; HttpOnly"),
        );
        let got = remint(&client(t), &SecretString::from("remember")).await.expect("mints");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&got.session), "fresh-abc");
    }

    /// The bug this exists for, with the header sequence a real Gitea 1.27.3 sends.
    ///
    /// `RegenerateSession` issues a fresh id *after* writing the user id, so the response
    /// carries two `i_like_gitea` cookies. Taking the first stores a valid but **anonymous** session:
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
                .with_header("set-cookie", "i_like_gitea=pre-regeneration; Path=/; HttpOnly")
                .with_header("set-cookie", "lang=en-US; Path=/; HttpOnly")
                .with_header("set-cookie", "i_like_gitea=after-regeneration; Path=/; HttpOnly"),
        );
        let got = remint(&client(t), &SecretString::from("remember")).await.expect("mints");
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&got.session),
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
                .with_header("set-cookie", "i_like_gitea=transient; Path=/")
                .with_header("set-cookie", "i_like_gitea=; Path=/; Max-Age=0"),
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

    /// Bug this prevents: a dead remember token "renewing" into an anonymous session. Gitea
    /// renders the login page for it with 200 **and a fresh `i_like_gitea`**, so a check for
    /// "a session was set and it was not a redirect to the login page" accepted it, stored it,
    /// and every private board then answered 404 instead of `gea auth login`'s advice.
    #[tokio::test]
    async fn a_login_page_that_sets_an_anonymous_session_is_still_expired() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::html(200, "<form action=\"/user/login\">")
                .with_header("set-cookie", "i_like_gitea=anonymous; Path=/; HttpOnly")
                .with_header("set-cookie", "gitea_incredible=; Path=/; Max-Age=0; HttpOnly"),
        );
        let err = remint(&client(t), &SecretString::from("used")).await.expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebSessionExpired { .. }), "{err:?}");
    }

    /// Bug this prevents: the second renewal always failing. Gitea rotates the remember token on
    /// every auto-sign-in and retires the one presented, so a renewal that kept the old token
    /// stored one that the next renewal was certain to find dead.
    #[tokio::test]
    async fn a_renewal_keeps_the_rotated_remember_token() {
        let t = FakeTransport::new().on(
            Method::GET,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/")
                .with_header("set-cookie", "i_like_gitea=pre; Path=/; HttpOnly")
                .with_header("set-cookie", "gitea_incredible=r1; Path=/; Max-Age=2678400; HttpOnly")
                .with_header("set-cookie", "i_like_gitea=post; Path=/; HttpOnly"),
        );
        let before = Timestamp::now();
        let cred = WebCredential::new("ada", SecretString::from("r0"), before);
        let cred = renew(&client(t), cred).await.expect("renews");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&cred.remember), "r1");
        assert_eq!(cred.session.as_ref().map(secrecy::ExposeSecret::expose_secret), Some("post"));
        assert!(cred.remember_expires_at > before, "the new token's own lifetime is recorded");
    }
}

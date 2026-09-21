//! Exchanging a password for a remember token.
//!
//! # The one place a password is used
//!
//! A password is sent to `POST /user/login` and never stored, never logged, and never bound to
//! a named `String` in this file — the rule `fjo`'s `auth/common.rs` already applies to tokens.
//! What is kept is the remember token the server returns, which is revocable from the web UI and
//! lapses on its own.
//!
//! # Why `remember=on` is not optional
//!
//! Without it Forgejo issues only a session cookie, with no `Max-Age` and no way to renew it.
//! Every later command would then need the password again, which for a tool that runs in cron is
//! the same as not working. Asking to be remembered is what makes the credential storable.
//!
//! # The four outcomes
//!
//! Forgejo answers a sign-in in four distinguishable ways, and conflating any two of them
//! produces a bad error message:
//!
//! * a redirect that sets `persistent` — signed in;
//! * a redirect to `/user/two_factor` — TOTP, which [`totp`] completes;
//! * a redirect to `/user/webauthn` — no headless completion exists, so this is refused with
//!   advice rather than retried;
//! * `200`, re-rendering the form with a flash message — the credentials were refused, and the
//!   server's own words are better than any this module could invent.

use http::Method;
use jiff::Timestamp;
use secrecy::SecretString;

use crate::error::{Error, ErrorKind, Result};
use crate::web::SESSION_COOKIE;
use crate::web::client::{Cookie, SecondFactor, WebBody, WebClient, WebResponse};
use crate::web::session::{host_of, remember_from};
use crate::web::stored::WebCredential;

/// Where a sign-in got to.
///
/// `Debug` is hand-written rather than derived: the TOTP state is a live session cookie, and a
/// derive would print it into any test failure or `--debug` line that touched this value.
pub enum LoginStep {
    /// Signed in; the credential is ready to store.
    Done(Box<WebCredential>),
    /// The account has TOTP. Carry `state` into [`totp`] along with the code: Forgejo keeps the
    /// half-finished sign-in in the session it just issued, so dropping this cookie would
    /// restart the sign-in rather than continue it.
    TotpRequired { state: SecretString },
}

impl std::fmt::Debug for LoginStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Done(c) => f.debug_tuple("Done").field(c).finish(),
            Self::TotpRequired { .. } => {
                f.debug_struct("TotpRequired").field("state", &"<redacted>").finish()
            }
        }
    }
}

/// First leg: username and password.
pub async fn password(
    client: &WebClient,
    user: &str,
    password: &SecretString,
    now: Timestamp,
) -> Result<LoginStep> {
    let form = WebBody::Form(vec![
        ("user_name".to_owned(), user.to_owned()),
        // The only place the plaintext appears. Moved, not copied into a binding.
        ("password".to_owned(), secrecy::ExposeSecret::expose_secret(password).to_owned()),
        // See the module comment: without this there is nothing storable to keep.
        ("remember".to_owned(), "on".to_owned()),
    ]);
    let resp = client.send(Method::POST, "/user/login", form, &[]).await?;
    classify(client, user, resp, now)
}

/// Second leg: the TOTP code, carrying the session the first leg established.
pub async fn totp(
    client: &WebClient,
    user: &str,
    code: &str,
    state: &SecretString,
    now: Timestamp,
) -> Result<LoginStep> {
    let form = WebBody::Form(vec![("passcode".to_owned(), code.to_owned())]);
    let cookie = Cookie::new(SESSION_COOKIE, state.clone());
    let resp =
        client.send(Method::POST, "/user/two_factor", form, std::slice::from_ref(&cookie)).await?;
    match classify(client, user, resp, now)? {
        // A second TOTP prompt means the code was wrong. Reporting it as "enter your code"
        // again would loop; it is a refused credential.
        LoginStep::TotpRequired { .. } => Err(Error::new(ErrorKind::WebLoginFailed {
            host: host_of(client.web_base()),
            reason: Some("the two-factor code was not accepted".to_owned()),
        })),
        done => Ok(done),
    }
}

fn classify(
    client: &WebClient,
    user: &str,
    resp: WebResponse,
    now: Timestamp,
) -> Result<LoginStep> {
    let host = host_of(client.web_base());

    match resp.second_factor() {
        Some(SecondFactor::WebAuthn) => {
            return Err(Error::new(ErrorKind::WebAuthnRequired { host }));
        }
        Some(SecondFactor::Totp) => {
            // Forgejo stores `twofaUid` in the session it issues here; without carrying that
            // cookie the second leg has nothing to complete.
            let Some(state) = resp.set_cookie(SESSION_COOKIE) else {
                return Err(Error::new(ErrorKind::WebLoginFailed {
                    host,
                    reason: Some(
                        "the server asked for a two-factor code but issued no session to \
                         complete it with"
                            .to_owned(),
                    ),
                }));
            };
            return Ok(LoginStep::TotpRequired { state });
        }
        None => {}
    }

    if let Some((remember, expires)) = remember_from(&resp, now) {
        let mut cred = WebCredential::new(user, remember, expires);
        // The sign-in response already carries a usable session; keeping it saves the very
        // first command a round trip.
        cred.session = resp.set_cookie(SESSION_COOKIE);
        return Ok(LoginStep::Done(Box::new(cred)));
    }

    // Anything else is a refusal. A redirect with no `persistent` cookie means the sign-in did
    // not take; a 200 means the form came back with a message in it.
    Err(Error::new(ErrorKind::WebLoginFailed { host, reason: flash_error(&resp.text()) }))
}

/// Forgejo's own flash message, when the re-rendered page carries one.
///
/// Deliberately narrow: it looks only for the message element Forgejo renders, and gives up
/// rather than scraping something that might be another part of the page. A wrong "server said"
/// line is worse than none, because a user will believe it.
fn flash_error(html: &str) -> Option<String> {
    // `<div ... class="... flash-error ...">` then the text up to the closing tag. Matching the
    // class rather than a phrase keeps this independent of the instance's language.
    let at = html.find("flash-error")?;
    let rest = &html[at..];
    let open_end = rest.find('>')?;
    let text = &rest[open_end + 1..];
    let close = text.find("</")?;
    let msg = decode_entities(text[..close].trim());
    let msg = msg.trim();
    (!msg.is_empty() && msg.len() < 300).then(|| msg.to_owned())
}

/// The five named entities and numeric references Go's `html/template` emits.
///
/// Shared with the board parser's needs but kept here rather than exported: a second caller
/// should move it to a common place rather than grow this one.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(semi) = tail.find(';').filter(|&n| n <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => ent
                .strip_prefix('#')
                .and_then(|n| {
                    n.strip_prefix('x')
                        .or_else(|| n.strip_prefix('X'))
                        .map_or_else(|| n.parse::<u32>().ok(), |h| u32::from_str_radix(h, 16).ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{Canned, FakeTransport};

    fn now() -> Timestamp {
        "2026-09-21T08:00:00Z".parse().unwrap()
    }

    fn client(t: FakeTransport) -> WebClient {
        WebClient::with_transport("https://forge.test", t)
    }

    #[tokio::test]
    async fn a_good_password_yields_a_storable_credential() {
        let t = FakeTransport::new().on(
            Method::POST,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/")
                .with_header("set-cookie", "persistent=remember-me; Path=/; Max-Age=2592000")
                .with_header("set-cookie", "session=sess-1; Path=/; HttpOnly"),
        );
        let step = password(&client(t), "perf3ct", &SecretString::from("pw"), now())
            .await
            .expect("signs in");
        let LoginStep::Done(cred) = step else { panic!("expected a completed sign-in") };
        assert_eq!(cred.user, "perf3ct");
        assert_eq!(cred.expose_remember(), "remember-me");
        // The expiry came from Max-Age, not from a hard-coded 31 days.
        assert_eq!(cred.remember_expires_at, "2026-10-21T08:00:00Z".parse::<Timestamp>().unwrap());
        assert!(cred.session.is_some(), "the sign-in's own session should be kept");
    }

    #[tokio::test]
    async fn a_bad_password_reports_the_servers_own_message() {
        let t = FakeTransport::new().on(
            Method::POST,
            "/user/login",
            Canned::html(
                200,
                r#"<div class="ui negative message flash-message flash-error">Username or password is incorrect.</div>"#,
            ),
        );
        let err = password(&client(t), "perf3ct", &SecretString::from("nope"), now())
            .await
            .expect_err("refuses");
        match *err.kind {
            ErrorKind::WebLoginFailed { ref reason, .. } => {
                assert_eq!(reason.as_deref(), Some("Username or password is incorrect."));
            }
            ref other => panic!("wrong kind: {other:?}"),
        }
    }

    #[tokio::test]
    async fn webauthn_is_refused_with_advice_rather_than_retried() {
        let t = FakeTransport::new().on(
            Method::POST,
            "/user/login",
            Canned::new(303).with_header("location", "/user/webauthn"),
        );
        let err = password(&client(t), "perf3ct", &SecretString::from("pw"), now())
            .await
            .expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebAuthnRequired { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn totp_carries_the_half_finished_sign_in() {
        let t = FakeTransport::new().on(
            Method::POST,
            "/user/login",
            Canned::new(303)
                .with_header("location", "/user/two_factor")
                .with_header("set-cookie", "session=half-done; Path=/; HttpOnly"),
        );
        let step = password(&client(t), "perf3ct", &SecretString::from("pw"), now())
            .await
            .expect("first leg");
        let LoginStep::TotpRequired { state } = step else { panic!("expected a TOTP prompt") };
        assert_eq!(secrecy::ExposeSecret::expose_secret(&state), "half-done");
    }

    /// Bug this prevents: a wrong TOTP code looping forever, because a second prompt looks
    /// exactly like the first one.
    #[tokio::test]
    async fn a_rejected_totp_code_is_a_failure_not_another_prompt() {
        let t = FakeTransport::new().on(
            Method::POST,
            "/user/two_factor",
            Canned::new(303)
                .with_header("location", "/user/two_factor")
                .with_header("set-cookie", "session=still-half; Path=/"),
        );
        let err = totp(&client(t), "perf3ct", "000000", &SecretString::from("half"), now())
            .await
            .expect_err("refuses");
        assert!(matches!(*err.kind, ErrorKind::WebLoginFailed { .. }), "{err:?}");
    }

    #[test]
    fn entities_are_decoded_the_way_go_emits_them() {
        assert_eq!(
            decode_entities("Fix &lt;script&gt; &amp; &#34;quotes&#34;"),
            r#"Fix <script> & "quotes""#
        );
        assert_eq!(decode_entities("plain"), "plain");
        // A bare ampersand is left alone rather than eating the rest of the string.
        assert_eq!(decode_entities("a & b"), "a & b");
    }
}

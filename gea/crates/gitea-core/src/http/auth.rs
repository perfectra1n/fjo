//! Credentials and the one place they are turned into headers.
//!
//! Header injection happens exactly once, in the client's send path ([`Credentials::apply`]).
//! Nothing else in the crate — and nothing in the 42k lines of generated client — may
//! construct an `Authorization` header. A second injection site is how you end up with one
//! code path that forgets the `token ` prefix.
//!
//! Gitea declares five security schemes; all five are represented here:
//! `AuthorizationHeaderToken`, `BasicAuth`, `SudoHeader` (`Sudo`), `SudoParam` (`?sudo=`), and
//! `TOTPHeader` (`X-GITEA-OTP`).
//!
//! [`Auth::Bearer`] is a sixth that the spec does not declare, because Gitea's OAuth2
//! endpoints live under the web root rather than `/api/v1` and so never appear in the Swagger
//! document. It is no less real: an OAuth2 access token authenticates the same `/api/v1`
//! routes, it just wears a different scheme.

use std::fmt;

use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, ErrorKind, Result};
use crate::http::base64;

/// The `Authorization` scheme prefix Gitea requires for API tokens.
///
/// **This prefix is mandatory.** The spec's own `AuthorizationHeaderToken` description spells
/// it out: the header value is `token <your-token>`. Gitea does *not* accept a bare token and
/// does *not* accept `Bearer`. Omitting it produces a plain `401`, indistinguishable from an
/// expired or revoked credential — so the user is sent to regenerate a token that was fine all
/// along. That failure is silent, self-inflicted, and costs an afternoon, which is why the
/// prefix is a named constant with this comment attached rather than an inline string literal
/// someone can "clean up".
pub const TOKEN_PREFIX: &str = "token ";

/// The `Authorization` scheme prefix for an OAuth2 access token.
///
/// Not interchangeable with [`TOKEN_PREFIX`], and the distinction is the entire reason there
/// are two constants. A Gitea personal access token is an opaque hex string and travels as
/// `token <hex>`; an OAuth2 access token is a JWT and travels as `Bearer <jwt>`.
///
/// Gitea's header parser happens to accept either prefix, trying a JWT parse first and
/// falling back to a token lookup — so sending the wrong one mostly works. "Mostly" is exactly
/// how a bug survives to production: it is an accident of the current implementation, not a
/// documented guarantee, and the day it stops holding the failure is the same silent `401`
/// described above. Send the scheme the credential actually is.
pub const BEARER_PREFIX: &str = "Bearer ";

/// The TOTP header. Gitea kept Gitea's `X-GITEA-OTP` as an accepted alias, but sends and
/// documents the Gitea-branded name, so that is what we send.
pub const OTP_HEADER: &str = "x-gitea-otp";

/// The `Sudo` header, for instance admins acting as another user.
pub const SUDO_HEADER: &str = "sudo";

/// How the caller authenticates.
///
/// `Default` is [`Auth::None`]: unauthenticated requests are legitimate (`/version`, public
/// repository reads), and defaulting to "no credentials" means a missing credential surfaces
/// as a clean `401` we can classify rather than as a panic or an empty header.
#[derive(Clone, Default)]
pub enum Auth {
    #[default]
    None,
    /// A Gitea API token. Sent as `Authorization: token <t>`.
    Token(SecretString),
    /// An OAuth2 access token (a JWT). Sent as `Authorization: Bearer <t>`.
    ///
    /// Short-lived — Gitea issues these with `expires_in: 3600` — so a client holding one is
    /// expected to refresh it. See [`crate::oauth`].
    Bearer(SecretString),
    /// HTTP Basic. Gitea accepts it, and it is the only way to use a password + TOTP to
    /// *create* a token in the first place, which `gea auth login` needs.
    Basic { user: String, pass: SecretString },
}

impl Auth {
    pub fn token(t: impl Into<String>) -> Self {
        Auth::Token(SecretString::from(t.into()))
    }

    pub fn bearer(t: impl Into<String>) -> Self {
        Auth::Bearer(SecretString::from(t.into()))
    }

    pub fn basic(user: impl Into<String>, pass: impl Into<String>) -> Self {
        Auth::Basic { user: user.into(), pass: SecretString::from(pass.into()) }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Auth::None)
    }

    /// Whether a credential was actually presented. A `401` means "your token was rejected"
    /// when this is true and "you are not logged in" when it is false — two different errors
    /// with two different remedies.
    pub fn has_credential(&self) -> bool {
        !self.is_none()
    }

    /// The secret material, for the token-leak test and for [`super::redact::text`]. Callers
    /// outside those two uses should not need this.
    pub fn secret(&self) -> Option<&str> {
        match self {
            Auth::None => None,
            Auth::Token(t) | Auth::Bearer(t) => Some(t.expose_secret()),
            Auth::Basic { pass, .. } => Some(pass.expose_secret()),
        }
    }
}

/// Hand-written so that a `#[derive(Debug)]` on any enclosing struct — `Client`, `Request`,
/// an `ErrorKind` — cannot print the token. `secrecy` already redacts `SecretString`, but
/// relying on that leaves the guarantee one refactor away from being lost the moment someone
/// stores a `String` here "temporarily".
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::None => f.write_str("None"),
            Auth::Token(_) => f.write_str("Token(<redacted>)"),
            Auth::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Auth::Basic { user, .. } => {
                f.debug_struct("Basic").field("user", user).field("pass", &"<redacted>").finish()
            }
        }
    }
}

/// Whether `Sudo` travels as a header or as a query parameter.
///
/// The header is correct and is what we default to. The query form exists because some
/// reverse proxies strip unknown request headers, and a stripped `Sudo` header fails *open* —
/// the request succeeds as yourself instead of as the target user, which for an admin script
/// is a wrong-target write rather than an error. `?sudo=` is the escape hatch for those hosts,
/// never the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SudoStyle {
    #[default]
    Header,
    Query,
}

/// Everything the send path needs to authenticate one request.
///
/// Separate from [`Auth`] because `sudo` and `otp` are orthogonal to *how* you authenticate:
/// you can sudo with a token or with basic auth, and a TOTP code accompanies either.
#[derive(Clone, Default)]
pub struct Credentials {
    pub auth: Auth,
    /// Act as this user. Requires an admin token.
    pub sudo: Option<String>,
    pub sudo_style: SudoStyle,
    /// A TOTP code. Single-use and short-lived, but still a credential, so still a
    /// `SecretString`.
    pub otp: Option<SecretString>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("auth", &self.auth)
            .field("sudo", &self.sudo)
            .field("sudo_style", &self.sudo_style)
            .field("otp", &self.otp.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// So `Client::new(base, Auth::token(t))` works: the common case names no sudo and no OTP.
impl From<Auth> for Credentials {
    fn from(auth: Auth) -> Self {
        Self { auth, ..Self::default() }
    }
}

impl Credentials {
    pub fn new(auth: Auth) -> Self {
        auth.into()
    }

    pub fn with_sudo(mut self, user: impl Into<String>) -> Self {
        self.sudo = Some(user.into());
        self
    }

    pub fn with_sudo_style(mut self, style: SudoStyle) -> Self {
        self.sudo_style = style;
        self
    }

    pub fn with_otp(mut self, code: impl Into<String>) -> Self {
        self.otp = Some(SecretString::from(code.into()));
        self
    }

    /// Inject every credential header for one request, and return the extra query parameters
    /// (only ever `sudo`, and only under [`SudoStyle::Query`]).
    ///
    /// Called exactly once per attempt, from the client's send path.
    pub fn apply(&self, headers: &mut HeaderMap) -> Result<Vec<(&'static str, String)>> {
        match &self.auth {
            Auth::None => {}
            Auth::Token(t) => {
                // See TOKEN_PREFIX: the `token ` prefix is not optional.
                let v = format!("{TOKEN_PREFIX}{}", t.expose_secret());
                headers.insert(http::header::AUTHORIZATION, sensitive(&v, "token")?);
            }
            Auth::Bearer(t) => {
                // See BEARER_PREFIX: an OAuth2 access token is not a personal access token.
                let v = format!("{BEARER_PREFIX}{}", t.expose_secret());
                headers.insert(http::header::AUTHORIZATION, sensitive(&v, "access token")?);
            }
            Auth::Basic { user, pass } => {
                let encoded = base64::encode(format!("{user}:{}", pass.expose_secret()));
                let v = format!("Basic {encoded}");
                headers.insert(http::header::AUTHORIZATION, sensitive(&v, "basic credentials")?);
            }
        }

        if let Some(code) = &self.otp {
            headers.insert(name(OTP_HEADER), sensitive(code.expose_secret(), "OTP code")?);
        }

        let mut query = Vec::new();
        if let Some(user) = &self.sudo {
            match self.sudo_style {
                SudoStyle::Header => {
                    headers.insert(name(SUDO_HEADER), sensitive(user, "sudo user")?);
                }
                SudoStyle::Query => query.push(("sudo", user.clone())),
            }
        }
        Ok(query)
    }
}

fn name(n: &'static str) -> HeaderName {
    // Every constant above is a valid lowercase header name; a typo is a bug in this file, not
    // a runtime condition, so `expect` is the honest response.
    HeaderName::from_static(n)
}

/// Build a header value, marking it sensitive so `http`/`hyper` keep it out of their own
/// `Debug` output, and reporting a *credential-shaped* error without echoing the value.
///
/// The `what` argument names the credential instead of quoting it: a token containing a
/// newline (a stray copy-paste out of a terminal) must produce "your token contains a
/// character that cannot be sent in a header", not a log line with the token in it.
fn sensitive(value: &str, what: &str) -> Result<HeaderValue> {
    let mut v = HeaderValue::from_str(value).map_err(|_| {
        Error::new(ErrorKind::Usage(format!(
            "your {what} contains a character that cannot be sent in an HTTP header \
             (a newline or a non-ASCII byte); re-copy it without surrounding whitespace"
        )))
    })?;
    v.set_sensitive(true);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(c: &Credentials) -> (HeaderMap, Vec<(&'static str, String)>) {
        let mut h = HeaderMap::new();
        let q = c.apply(&mut h).expect("valid credentials");
        (h, q)
    }

    /// THE silent-401 bug. Gitea requires the literal `token ` prefix; a bare token, or
    /// `Bearer`, is rejected with a 401 that looks exactly like an expired credential.
    #[test]
    fn token_header_carries_the_mandatory_token_prefix() {
        let (h, _) = applied(&Credentials::new(Auth::token("abc123")));
        let v = h.get(http::header::AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(v, "token abc123");
        assert!(!v.starts_with("Bearer"), "Gitea does not accept Bearer");
    }

    /// The other half of the same bug, and the reason the assertion above says "Gitea does
    /// not accept Bearer" without that being the whole truth. It is true of the *personal
    /// access token* that test covers. An OAuth2 access token is a JWT and wants `Bearer`;
    /// sending `token <jwt>` happens to work against today's Gitea, which is precisely the
    /// kind of accident that stops working without warning.
    #[test]
    fn an_oauth_access_token_uses_the_bearer_scheme() {
        let (h, _) = applied(&Credentials::new(Auth::bearer("eyJhbGciOiJIUzUxMiJ9.e30.sig")));
        let v = h.get(http::header::AUTHORIZATION).unwrap().to_str().unwrap();
        assert_eq!(v, "Bearer eyJhbGciOiJIUzUxMiJ9.e30.sig");
        assert!(!v.starts_with(TOKEN_PREFIX), "an OAuth2 access token is not a PAT");
    }

    #[test]
    fn basic_auth_is_base64_of_user_colon_pass() {
        let (h, _) = applied(&Credentials::new(Auth::basic("alice", "hunter2")));
        // `echo -n 'alice:hunter2' | base64` => YWxpY2U6aHVudGVyMg==
        assert_eq!(
            h.get(http::header::AUTHORIZATION).unwrap().to_str().unwrap(),
            "Basic YWxpY2U6aHVudGVyMg=="
        );
    }

    #[test]
    fn otp_uses_the_gitea_header_name() {
        let (h, _) = applied(&Credentials::new(Auth::token("t")).with_otp("123456"));
        assert_eq!(h.get(OTP_HEADER).unwrap().to_str().unwrap(), "123456");
    }

    /// The header is the default; a stripped header would fail *open* and act as the wrong
    /// user, so opting into `?sudo=` has to be explicit.
    #[test]
    fn sudo_prefers_the_header_and_uses_query_only_when_asked() {
        let (h, q) = applied(&Credentials::new(Auth::token("t")).with_sudo("bob"));
        assert_eq!(h.get(SUDO_HEADER).unwrap().to_str().unwrap(), "bob");
        assert!(q.is_empty());

        let (h, q) = applied(
            &Credentials::new(Auth::token("t")).with_sudo("bob").with_sudo_style(SudoStyle::Query),
        );
        assert!(h.get(SUDO_HEADER).is_none());
        assert_eq!(q, vec![("sudo", "bob".to_owned())]);
    }

    #[test]
    fn unauthenticated_requests_send_no_authorization_header() {
        let (h, q) = applied(&Credentials::default());
        assert!(h.is_empty());
        assert!(q.is_empty());
    }

    /// The token-leak guarantee, asserted at the type level rather than trusted.
    #[test]
    fn debug_never_prints_the_secret() {
        let a = Auth::token("supersecret-token-value");
        let d = format!("{a:?}");
        assert_eq!(d, "Token(<redacted>)");
        assert!(!d.contains("supersecret"));

        let a = Auth::bearer("supersecret-access-token");
        let d = format!("{a:?}");
        assert_eq!(d, "Bearer(<redacted>)");
        assert!(!d.contains("supersecret"));

        let c = Credentials::new(Auth::basic("alice", "supersecret")).with_otp("123456");
        let d = format!("{c:?}");
        assert!(!d.contains("supersecret"), "{d}");
        assert!(!d.contains("123456"), "{d}");
        assert!(d.contains("alice"), "the username is not a secret and is useful: {d}");
    }

    /// A token copy-pasted with a trailing newline must produce advice, not a panic and not a
    /// log line containing the token.
    #[test]
    fn a_token_with_a_newline_reports_without_echoing_it() {
        let c = Credentials::new(Auth::token("abc\ndef"));
        let mut h = HeaderMap::new();
        let e = c.apply(&mut h).expect_err("a newline is not a legal header value");
        let msg = format!("{:?}", e.kind());
        assert!(!msg.contains("abc"), "{msg}");
        assert!(msg.contains("token"), "{msg}");
    }
}

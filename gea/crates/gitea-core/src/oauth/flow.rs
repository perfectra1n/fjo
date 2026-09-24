//! Building the authorize URL, and trading a code or a refresh token for an access token.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use zeroize::Zeroize;

use crate::error::{Error, ErrorKind, Result};
use crate::http::{Client, encode};
use crate::oauth::Endpoints;

/// Gitea's default `ACCESS_TOKEN_EXPIRATION_TIME`, used only when a response omits
/// `expires_in`. Treating a missing lifetime as an error would fail a login over a field we can
/// reasonably assume; treating it as infinite would mean never refreshing.
const DEFAULT_EXPIRES_IN: i64 = 3600;

/// Everything the authorize URL carries besides the fixed parameters.
pub struct AuthorizeParams<'a> {
    pub client_id: &'a str,
    /// Must be `http://127.0.0.1:<port>` with no path. See the module documentation.
    pub redirect_uri: &'a str,
    pub state: &'a str,
    /// The PKCE `code_challenge`, already S256-derived.
    pub challenge: &'a str,
}

/// The URL to open in a browser.
///
/// Pure, so the exact query string is testable without a server or a socket.
///
/// No `scope` parameter. It is optional, and omitting it means no `openid`, hence no `id_token`
/// to validate and no JWKS machinery to carry. Gitea does not enforce OAuth scopes anyway, so
/// naming one would describe a restriction that will not be applied.
pub fn authorize_url(ep: &Endpoints, p: &AuthorizeParams<'_>) -> String {
    let query = encode::query_string([
        ("client_id", p.client_id),
        ("redirect_uri", p.redirect_uri),
        ("response_type", "code"),
        ("state", p.state),
        ("code_challenge", p.challenge),
        ("code_challenge_method", "S256"),
    ]);
    format!("{}?{}", ep.authorize, query)
}

/// A successful token response.
pub struct TokenResponse {
    pub access_token: SecretString,
    /// Gitea always sends one. Empty only if a future version stops, in which case the session
    /// simply cannot be refreshed and the user logs in again when it lapses.
    pub refresh_token: SecretString,
    /// Seconds from now.
    pub expires_in: i64,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawToken {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    expires_in: Option<i64>,
}

/// RFC 6749 §5.2. Not Gitea's usual API error body, which is why the token endpoint is read
/// through the unclassified path and interpreted here.
#[derive(Deserialize, Default)]
struct RawError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Exchange an authorization code for tokens.
pub async fn exchange_code(
    client: &Client,
    ep: &Endpoints,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &SecretString,
) -> Result<TokenResponse> {
    let fields = vec![
        ("grant_type".to_owned(), "authorization_code".to_owned()),
        ("client_id".to_owned(), client_id.to_owned()),
        // Gitea re-checks this against the one sent to /authorize, so it is not redundant.
        ("redirect_uri".to_owned(), redirect_uri.to_owned()),
        ("code".to_owned(), code.to_owned()),
        ("code_verifier".to_owned(), verifier.expose_secret().to_owned()),
    ];
    post_token(client, &ep.token, fields, TokenFailure::Exchange).await
}

/// Trade a refresh token for a new access token — and a new refresh token.
///
/// No `client_secret`: gea is a public client, and Gitea skips the secret check entirely when
/// the application is not confidential.
pub async fn refresh(
    client: &Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &SecretString,
) -> Result<TokenResponse> {
    let fields = vec![
        ("grant_type".to_owned(), "refresh_token".to_owned()),
        ("client_id".to_owned(), client_id.to_owned()),
        ("refresh_token".to_owned(), refresh_token.expose_secret().to_owned()),
    ];
    post_token(client, token_endpoint, fields, TokenFailure::Refresh).await
}

/// Which error a refused token request becomes. The two have different remedies: a failed
/// exchange means try the login again, a failed refresh means the session is gone.
#[derive(Clone, Copy)]
enum TokenFailure {
    Exchange,
    Refresh,
}

async fn post_token(
    client: &Client,
    url: &str,
    fields: Vec<(String, String)>,
    on_failure: TokenFailure,
) -> Result<TokenResponse> {
    // Anonymous: the exchange authenticates by the client_id in the body, and presenting a
    // stale Authorization header alongside invites the server to believe the wrong one.
    let resp = client.anonymous().web_form_raw(url, fields).await?;
    let host = client.host().to_owned();

    if !resp.is_success() {
        let e: RawError = serde_json::from_slice(&resp.body).unwrap_or_default();
        let error =
            if e.error.is_empty() { format!("HTTP {}", resp.status) } else { e.error.clone() };
        return Err(Error::new(match on_failure {
            TokenFailure::Exchange => ErrorKind::OauthTokenExchangeFailed {
                host,
                error,
                description: e.error_description,
            },
            TokenFailure::Refresh => ErrorKind::OauthRefreshFailed {
                host,
                login: client.login().unwrap_or_default().to_owned(),
                reason: e.error_description.or(Some(error)),
            },
        }));
    }

    let mut raw: RawToken = serde_json::from_slice(&resp.body).map_err(|e| {
        Error::new(ErrorKind::OauthTokenExchangeFailed {
            host: host.clone(),
            error: "unreadable response".to_owned(),
            description: Some(e.to_string()),
        })
    })?;
    if raw.access_token.is_empty() {
        return Err(Error::new(ErrorKind::OauthTokenExchangeFailed {
            host,
            error: "no access token".to_owned(),
            description: Some("the server answered 200 with no access_token".to_owned()),
        }));
    }

    let out = TokenResponse {
        access_token: SecretString::from(raw.access_token.clone()),
        refresh_token: SecretString::from(raw.refresh_token.clone()),
        expires_in: raw.expires_in.unwrap_or(DEFAULT_EXPIRES_IN),
    };
    // The plaintext copies served their purpose at `from`; wipe them rather than leaving two
    // more heap allocations holding a live credential until the allocator reuses them.
    raw.access_token.zeroize();
    raw.refresh_token.zeroize();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::error::ErrorKind;
    use crate::http::transport::{Canned, FakeTransport, RecordedCall};
    use crate::http::{Auth, Method};

    const TOKEN_PATH: &str = "/login/oauth/access_token";

    fn client(canned: Canned) -> (Client, Arc<FakeTransport>) {
        let t = Arc::new(FakeTransport::new().on(Method::POST, TOKEN_PATH, canned));
        let c = Client::builder("https://git.example.org", Auth::token("pat"))
            .transport(t.clone())
            .login("perf3ct")
            .build()
            .expect("a valid base URL");
        (c, t)
    }

    fn ok_body(body: &'static str) -> Canned {
        Canned::new(200).with_header("content-type", "application/json").with_body(body)
    }

    fn body_of(call: &RecordedCall) -> String {
        String::from_utf8(call.body.clone().unwrap_or_default().to_vec())
            .expect("a utf-8 form body")
    }

    fn endpoints() -> Endpoints {
        Endpoints::fixed("https://git.example.org")
    }

    /// THE protocol trap, and the reason this test names a path rather than a behaviour.
    ///
    /// Gitea matches redirect URIs by exact string, after stripping the port for a public
    /// client on a loopback address. The built-in applications register `http://127.0.0.1`, so
    /// the port is forgiven and the path is not. Appending `/callback` — which looks tidier and
    /// is what every OAuth tutorial does — makes every login fail with `redirect_uri_mismatch`.
    #[test]
    fn the_authorize_url_carries_s256_and_a_redirect_uri_with_no_path() {
        let url = authorize_url(
            &endpoints(),
            &AuthorizeParams {
                client_id: "cid",
                redirect_uri: "http://127.0.0.1:45231",
                state: "st",
                challenge: "ch",
            },
        );
        assert!(url.starts_with("https://git.example.org/login/oauth/authorize?"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A45231"), "{url}");
        assert!(!url.contains("callback"), "a loopback redirect URI must carry no path: {url}");
        // No scope: requesting `openid` would return an id_token we have no way to validate and
        // no need for, and Gitea enforces no scopes on an OAuth token regardless.
        assert!(!url.contains("scope="), "{url}");
    }

    /// Gitea's token endpoint is `/login/oauth/access_token`. A reasonable guess of `/token`
    /// gets a 404 that says nothing about why.
    #[tokio::test]
    async fn the_token_exchange_posts_a_form_to_access_token_not_token() {
        let (c, t) =
            client(ok_body(r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#));
        let got = exchange_code(
            &c,
            &endpoints(),
            "cid",
            "http://127.0.0.1:45231",
            "the-code",
            &SecretString::from("the-verifier".to_owned()),
        )
        .await
        .expect("a 200 with an access token");

        assert_eq!(got.access_token.expose_secret(), "at");
        assert_eq!(got.refresh_token.expose_secret(), "rt");
        assert_eq!(got.expires_in, 3600);

        let call = &t.calls()[0];
        assert_eq!(call.method, Method::POST);
        assert_eq!(call.url, "https://git.example.org/login/oauth/access_token");
        let body = body_of(call);
        for expected in [
            "grant_type=authorization_code",
            "client_id=cid",
            "code=the-code",
            "code_verifier=the-verifier",
        ] {
            assert!(body.contains(expected), "{expected} missing from {body}");
        }
    }

    /// The bug this prevents: exchanging a code while still presenting the credential that is
    /// being replaced. Gitea authenticates the request by the client_id in the body.
    #[tokio::test]
    async fn the_token_exchange_sends_no_authorization_header() {
        let (c, t) = client(ok_body(r#"{"access_token":"at","refresh_token":"rt"}"#));
        exchange_code(
            &c,
            &endpoints(),
            "cid",
            "http://127.0.0.1:45231",
            "code",
            &SecretString::from("v".to_owned()),
        )
        .await
        .expect("a 200");
        assert!(
            !t.calls()[0].headers.iter().any(|(n, _)| n == "authorization"),
            "{:?}",
            t.calls()[0].headers
        );
    }

    /// A missing `expires_in` must not fail the login, and must not be read as "never expires".
    #[tokio::test]
    async fn a_response_without_expires_in_falls_back_to_an_hour() {
        let (c, _) = client(ok_body(r#"{"access_token":"at","refresh_token":"rt"}"#));
        let got = exchange_code(
            &c,
            &endpoints(),
            "cid",
            "http://127.0.0.1:45231",
            "code",
            &SecretString::from("v".to_owned()),
        )
        .await
        .expect("a 200");
        assert_eq!(got.expires_in, DEFAULT_EXPIRES_IN);
    }

    /// Never swallow a server message. `error_description` is the half written for a human —
    /// "PKCE is required for public clients" says what to fix, `invalid_request` does not.
    #[tokio::test]
    async fn a_refused_exchange_reports_the_servers_own_error_description() {
        let (c, _) = client(
            Canned::new(400).with_header("content-type", "application/json").with_body(
                r#"{"error":"invalid_request","error_description":"PKCE is required for public clients"}"#,
            ),
        );
        let e = exchange_code(
            &c,
            &endpoints(),
            "cid",
            "http://127.0.0.1:45231",
            "code",
            &SecretString::from("v".to_owned()),
        )
        .await
        .expect_err("a 400 is not a successful exchange");

        match e.kind() {
            ErrorKind::OauthTokenExchangeFailed { error, description, .. } => {
                assert_eq!(error, "invalid_request");
                assert_eq!(description.as_deref(), Some("PKCE is required for public clients"));
            }
            other => panic!("expected a token exchange failure, got {other:?}"),
        }
    }

    /// gea is a public client. Gitea skips the secret check entirely for one, and sending an
    /// empty `client_secret` is not the same as sending none.
    #[tokio::test]
    async fn a_refresh_sends_client_id_and_no_client_secret() {
        let (c, t) = client(ok_body(r#"{"access_token":"at2","refresh_token":"rt2"}"#));
        let got = refresh(
            &c,
            "https://git.example.org/login/oauth/access_token",
            "cid",
            &SecretString::from("old-rt".to_owned()),
        )
        .await
        .expect("a 200");

        assert_eq!(got.refresh_token.expose_secret(), "rt2");
        let body = body_of(&t.calls()[0]);
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("client_id=cid"), "{body}");
        assert!(body.contains("refresh_token=old-rt"), "{body}");
        assert!(!body.contains("client_secret"), "{body}");
    }

    /// A refused refresh is a different situation from a refused exchange: the session is gone
    /// and cannot be recovered, so it must not tell the user to retry the same thing.
    #[tokio::test]
    async fn a_refused_refresh_is_reported_as_an_expired_session() {
        let (c, _) =
            client(Canned::new(400).with_header("content-type", "application/json").with_body(
                r#"{"error":"invalid_grant","error_description":"token was already used"}"#,
            ));
        let e = refresh(
            &c,
            "https://git.example.org/login/oauth/access_token",
            "cid",
            &SecretString::from("rt".to_owned()),
        )
        .await
        .expect_err("a 400 is not a successful refresh");

        match e.kind() {
            ErrorKind::OauthRefreshFailed { login, reason, .. } => {
                assert_eq!(login, "perf3ct");
                assert_eq!(reason.as_deref(), Some("token was already used"));
            }
            other => panic!("expected a refresh failure, got {other:?}"),
        }
    }

    /// A 200 carrying no token is a broken server, not a successful login. Accepting it would
    /// store an empty credential and fail confusingly on the next request.
    #[tokio::test]
    async fn a_success_with_no_access_token_is_still_a_failure() {
        let (c, _) = client(ok_body(r#"{"token_type":"bearer"}"#));
        let e = exchange_code(
            &c,
            &endpoints(),
            "cid",
            "http://127.0.0.1:45231",
            "code",
            &SecretString::from("v".to_owned()),
        )
        .await
        .expect_err("a body with no access_token is not usable");
        assert!(matches!(e.kind(), ErrorKind::OauthTokenExchangeFailed { .. }));
    }
}

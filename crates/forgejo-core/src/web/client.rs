//! An HTTP client for the instance's web root, authenticated by cookie.
//!
//! # Why this is not [`crate::http::Client`] with another [`crate::http::Auth`] variant
//!
//! Three reasons, all structural rather than stylistic.
//!
//! **Redirects must not be followed.** A web route's answer to "you are not signed in" is a
//! `303` to `/user/login`, never a `401`. A client that follows redirects turns that into a
//! `200` carrying the login page, so every unauthenticated command would appear to succeed and
//! return HTML nobody asked for. reqwest's redirect policy is fixed per client, so this needs a
//! client of its own — the API client must keep following redirects, because pagination and
//! attachment downloads depend on it.
//!
//! **The credential must be absent, not merely unused.** `Credentials::apply` writes an
//! `Authorization` header for whatever `Auth` it holds. A web client that held one could send a
//! token to a redirect target, and the API token is the more valuable of the two secrets. This
//! type has no `Credentials` field at all, so there is nothing to leak: the same reasoning as
//! [`crate::http::Client::anonymous`], applied by construction.
//!
//! **A session is minted, not attached.** `apply` is a one-header-per-request contract with no
//! room for "and if the server says the session lapsed, get another one and try again". That
//! belongs a layer up, in [`super::session`].
//!
//! # Cross-origin protection
//!
//! Forgejo 16.0.5 guards state-changing web routes with Go's `http.CrossOriginProtection`, which
//! decides from the `Sec-Fetch-Site` and `Origin` headers rather than from a token in the page.
//! A request carrying neither is treated as same-origin or non-browser and is allowed. **This
//! client must therefore never send either header**, which is why it composes its headers
//! explicitly instead of letting a caller pass arbitrary ones into the auth path;
//! `no_browser_headers_are_ever_sent` asserts it.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, ErrorKind, Result};
use crate::http::transport::{HttpRequest, OutBody, ReqwestTransport, Transport};

/// One cookie to present. The value is secret: a session cookie is a bearer credential.
#[derive(Clone)]
pub struct Cookie {
    pub name: &'static str,
    pub value: SecretString,
}

impl Cookie {
    pub fn new(name: &'static str, value: SecretString) -> Self {
        Self { name, value }
    }
}

/// What to send as a request body.
#[derive(Clone)]
pub enum WebBody {
    None,
    /// `application/x-www-form-urlencoded` — what Forgejo's HTML forms take.
    Form(Vec<(String, String)>),
    /// `application/json` — what the board's own JavaScript sends for moves.
    Json(Vec<u8>),
}

/// A response, fully buffered.
///
/// Web pages are small and are parsed whole; there is no streaming case here, and buffering
/// means [`WebResponse::redirects_to_login`] can be answered without the caller holding a
/// half-read body.
pub struct WebResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl WebResponse {
    /// The value of a `Set-Cookie` for `name`, if the response sets one.
    ///
    /// Reads **every** `Set-Cookie` header, not just the first: one sign-in response sets the
    /// session, the remember token and a locale cookie together, and `HeaderMap::get` would
    /// silently return whichever happened to be first.
    pub fn set_cookie(&self, name: &str) -> Option<SecretString> {
        self.set_cookie_attrs(name).map(|(v, _)| v)
    }

    /// The cookie's value and its raw attribute string (`Path=/; Max-Age=2592000; …`).
    pub fn set_cookie_attrs(&self, name: &str) -> Option<(SecretString, String)> {
        for raw in self.headers.get_all(header::SET_COOKIE) {
            let Ok(text) = raw.to_str() else { continue };
            let (pair, attrs) = text.split_once(';').unwrap_or((text, ""));
            let Some((k, v)) = pair.split_once('=') else { continue };
            if k.trim() == name {
                let v = v.trim();
                // An empty value is how a cookie is *cleared*. Treating that as a credential
                // would store the empty string and fail every later request with no clue why.
                if v.is_empty() {
                    return None;
                }
                return Some((SecretString::from(v), attrs.trim().to_owned()));
            }
        }
        None
    }

    pub fn location(&self) -> Option<&str> {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok())
    }

    /// Whether this is Forgejo's "you are not signed in" answer.
    ///
    /// This is the *only* such signal on a web route — there is no `401` — so every caller that
    /// cares about authentication asks this rather than inspecting a status code.
    pub fn redirects_to_login(&self) -> bool {
        self.status.is_redirection()
            && self.location().is_some_and(|l| {
                let path = l.split(['?', '#']).next().unwrap_or(l);
                path.ends_with("/user/login") || path == "/user/login"
            })
    }

    /// Where a redirect points, when it is one of the two-factor legs.
    pub fn second_factor(&self) -> Option<SecondFactor> {
        if !self.status.is_redirection() {
            return None;
        }
        let l = self.location()?;
        let path = l.split(['?', '#']).next().unwrap_or(l);
        if path.ends_with("/user/two_factor") {
            Some(SecondFactor::Totp)
        } else if path.ends_with("/user/webauthn") {
            Some(SecondFactor::WebAuthn)
        } else {
            None
        }
    }

    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// Which second factor a sign-in was redirected into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondFactor {
    Totp,
    WebAuthn,
}

/// A cookie-authenticated client for one instance's web root.
pub struct WebClient {
    transport: Arc<dyn Transport>,
    /// `https://host[/subpath]`, no trailing slash. Never ends in `/api/v1`.
    web_base: String,
    headers: HeaderMap,
}

impl std::fmt::Debug for WebClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebClient")
            .field("web_base", &self.web_base)
            .field("transport", &self.transport.name())
            .finish()
    }
}

impl WebClient {
    /// Build a client with its own redirects-off transport.
    pub fn new(web_base: &str, user_agent: &str) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(std::time::Duration::from_secs(10))
            // The whole point. See the module comment: a followed `303` destroys both the
            // `Set-Cookie` a sign-in returns and the only signal that a session has lapsed.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Usage(format!("could not initialise the web client: {e}")))
            })?;
        Ok(Self::with_transport(web_base, ReqwestTransport::from_client(inner)))
    }

    /// Build a client over a supplied transport, for tests.
    pub fn with_transport(web_base: &str, transport: impl Transport + 'static) -> Self {
        let mut headers = HeaderMap::new();
        // Forgejo content-negotiates some of these routes; asking for HTML is what a browser
        // does, and the JSON routes answer JSON regardless of what is asked for.
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/json;q=0.9,*/*;q=0.8"),
        );
        Self {
            transport: Arc::new(transport),
            web_base: web_base.trim_end_matches('/').trim_end_matches("/api/v1").to_owned(),
            headers,
        }
    }

    pub fn web_base(&self) -> &str {
        &self.web_base
    }

    /// Resolve an instance-relative path against the web root.
    ///
    /// Refuses anything that would leave the instance. A caller passes paths built from a
    /// repository slug, and a slug is user input; without this, `-R '../../evil.example/x'`
    /// would aim a session cookie at another host. The same reasoning as
    /// [`crate::http::Client::web_bytes`]'s host check, applied before the request rather than
    /// after.
    pub fn url_for(&self, path: &str) -> Result<String> {
        let trimmed = path.trim_start_matches('/');
        if trimmed.contains("://") || path.starts_with("//") {
            return Err(Error::new(ErrorKind::Usage(format!(
                "a web path must be relative to the instance, but {path} names a host"
            ))));
        }
        if trimmed.split('/').any(|seg| seg == "..") {
            return Err(Error::new(ErrorKind::Usage(format!(
                "a web path may not climb out of the instance with `..`: {path}"
            ))));
        }
        Ok(format!("{}/{trimmed}", self.web_base))
    }

    /// Send one request. No redirect is followed and no retry is made here; both are decisions
    /// for the caller, who is the only one who knows whether a `303` means "lapsed" or "done".
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: WebBody,
        cookies: &[Cookie],
    ) -> Result<WebResponse> {
        let url = self.url_for(path)?;
        self.send_absolute(method, &url, body, cookies).await
    }

    pub(crate) async fn send_absolute(
        &self,
        method: Method,
        url: &str,
        body: WebBody,
        cookies: &[Cookie],
    ) -> Result<WebResponse> {
        let mut headers = self.headers.clone();
        if !cookies.is_empty() {
            let joined = cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value.expose_secret()))
                .collect::<Vec<_>>()
                .join("; ");
            let mut v = HeaderValue::from_str(&joined).map_err(|_| {
                Error::new(ErrorKind::Usage(
                    "the stored web session contains characters that cannot be sent as a cookie"
                        .to_owned(),
                ))
            })?;
            // So `--debug` and any panic print `Cookie: <redacted>` rather than the session.
            v.set_sensitive(true);
            headers.insert(header::COOKIE, v);
        }

        let out = match body {
            WebBody::None => OutBody::Empty,
            // Encoded exactly as `Body::Form` is in the API client — `encode::form`, `+` for
            // space — so a form field cannot mean one thing on one layer and another here.
            WebBody::Form(fields) => {
                let encoded = fields
                    .iter()
                    .map(|(k, v)| {
                        format!("{}={}", crate::http::encode::form(k), crate::http::encode::form(v))
                    })
                    .collect::<Vec<_>>()
                    .join("&");
                OutBody::Bytes {
                    mime: "application/x-www-form-urlencoded".to_owned(),
                    data: Bytes::from(encoded),
                }
            }
            WebBody::Json(data) => {
                OutBody::Bytes { mime: "application/json".to_owned(), data: Bytes::from(data) }
            }
        };

        let req = HttpRequest { method, url: url.to_owned(), headers, body: out, timeout: None };
        let resp = self.transport.execute(req).await?;
        let status = resp.status;
        let headers = resp.headers.clone();
        let body = resp.bytes().await?;
        Ok(WebResponse { status, headers, body })
    }
}

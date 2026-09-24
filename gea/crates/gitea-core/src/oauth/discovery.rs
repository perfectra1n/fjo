//! Finding the authorize and token endpoints.
//!
//! Gitea publishes an OpenID Connect discovery document and has kept the same two paths for
//! years, so this could reasonably be a pair of constants. It is not, for one reason: a constant
//! is a guess that cannot be corrected, and an instance behind a reverse proxy that relocates
//! them would be unreachable with no way for the user to say otherwise. Discovery asks; the
//! constants are what we fall back to when nothing answers.

use serde::Deserialize;

use crate::http::Client;

/// Gitea's authorize path. Note the token path is `/login/oauth/access_token` and not the
/// conventional `/token`, which is the kind of detail a reasonable guess gets wrong.
const AUTHORIZE_PATH: &str = "/login/oauth/authorize";
const TOKEN_PATH: &str = "/login/oauth/access_token";
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";

/// Where this instance's OAuth2 endpoints are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
}

#[derive(Deserialize)]
struct Document {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
}

impl Endpoints {
    /// The paths Gitea has always used, relative to an instance's web root.
    pub fn fixed(web_base: &str) -> Self {
        Self {
            authorize: format!("{web_base}{AUTHORIZE_PATH}"),
            token: format!("{web_base}{TOKEN_PATH}"),
        }
    }

    /// Read the discovery document, falling back to [`Endpoints::fixed`] for anything it does
    /// not usably provide.
    ///
    /// **Never fails.** A missing document, a 404, a body that is not JSON, a field that is
    /// absent, or an endpoint pointing somewhere else entirely all produce the fixed paths
    /// rather than an error. This mirrors [`Client::capabilities`], and for the same reason: a
    /// probe that can veto the operation it was meant to assist is worse than no probe. If the
    /// fallback is wrong the next request says so, with a message about the endpoint rather than
    /// about discovery.
    pub async fn discover(client: &Client) -> Self {
        let fixed = Self::fixed(client.web_base());
        let url = format!("{}{DISCOVERY_PATH}", client.web_base());
        let Ok(doc) = client.web_json::<Document>(&url).await else {
            return fixed;
        };
        Self {
            authorize: Self::vet(doc.authorization_endpoint, client.web_base())
                .unwrap_or(fixed.authorize),
            token: Self::vet(doc.token_endpoint, client.web_base()).unwrap_or(fixed.token),
        }
    }

    /// Accept a discovered endpoint only if it is on this instance.
    ///
    /// The document is served by the instance, so a compromised or misconfigured one could name
    /// any host it liked — and the token request that followed would carry an authorization code
    /// there. `Client::web_form` would refuse such a URL anyway, but refusing it here means the
    /// failure is "we ignored a bad discovery document and used the normal path", not "your
    /// login broke".
    fn vet(candidate: Option<String>, web_base: &str) -> Option<String> {
        let c = candidate?;
        let on_instance = c.strip_prefix(web_base).is_some_and(|rest| {
            // `https://host` must not be treated as a prefix of `https://host.evil.example`.
            rest.is_empty() || rest.starts_with(['/', '?', '#'])
        });
        on_instance.then_some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{Canned, FakeTransport};
    use crate::http::{Auth, Method};
    use std::sync::Arc;

    fn client_returning(body: Option<&'static str>) -> Client {
        let mut t = FakeTransport::new();
        t = match body {
            Some(b) => t.on(
                Method::GET,
                DISCOVERY_PATH,
                Canned::new(200).with_header("content-type", "application/json").with_body(b),
            ),
            None => t.on(Method::GET, DISCOVERY_PATH, Canned::new(404)),
        };
        Client::builder("https://git.example.org", Auth::None)
            .transport(Arc::new(t))
            .build()
            .expect("a valid base URL")
    }

    /// Bug this prevents: an instance that does not publish the document — or a network blip on
    /// a request that is only an optimisation — turning into a failed login.
    #[tokio::test]
    async fn discovery_falls_back_to_the_fixed_paths_when_the_document_is_missing() {
        let c = client_returning(None);
        assert_eq!(Endpoints::discover(&c).await, Endpoints::fixed("https://git.example.org"));
    }

    #[tokio::test]
    async fn a_published_document_is_used() {
        let c = client_returning(Some(
            r#"{"authorization_endpoint":"https://git.example.org/o/auth",
                "token_endpoint":"https://git.example.org/o/tok"}"#,
        ));
        let e = Endpoints::discover(&c).await;
        assert_eq!(e.authorize, "https://git.example.org/o/auth");
        assert_eq!(e.token, "https://git.example.org/o/tok");
    }

    /// The instance serves the document, so it must not be able to use it to aim a request
    /// carrying an authorization code at a host of its choosing.
    #[tokio::test]
    async fn a_discovered_endpoint_on_another_origin_is_discarded() {
        let c = client_returning(Some(
            r#"{"authorization_endpoint":"https://evil.example/auth",
                "token_endpoint":"https://git.example.org.evil.example/tok"}"#,
        ));
        assert_eq!(Endpoints::discover(&c).await, Endpoints::fixed("https://git.example.org"));
    }

    /// A document that answers but omits a field is not a reason to fail, and not a reason to
    /// discard the field it did supply.
    #[tokio::test]
    async fn a_partial_document_keeps_the_fixed_path_for_what_is_missing() {
        let c = client_returning(Some(r#"{"token_endpoint":"https://git.example.org/o/tok"}"#));
        let e = Endpoints::discover(&c).await;
        assert_eq!(e.authorize, "https://git.example.org/login/oauth/authorize");
        assert_eq!(e.token, "https://git.example.org/o/tok");
    }
}

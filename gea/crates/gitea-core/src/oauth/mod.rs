//! Logging in through Gitea's OAuth2 provider.
//!
//! Gitea has been an OAuth2 provider for years, which lets a CLI obtain a credential through
//! the browser instead of asking the user to paste a personal access token. The resulting token
//! expires in an hour rather than never, and the secret never crosses the clipboard.
//!
//! # What Gitea actually implements
//!
//! Several of these are not what a reader who knows OAuth would assume, and two of them break an
//! implementation silently rather than loudly. They are recorded here because every one of them
//! was established by reading Gitea's source, not its documentation.
//!
//! * **Authorization code only.** There is no device authorization grant — the feature request
//!   (gitea/gitea#4830) is open and its implementation is still a draft. A headless machine
//!   therefore cannot do a self-contained login; something has to carry a URL to a browser and
//!   the reply back.
//! * **PKCE is mandatory for public clients.** Gitea rejects an authorize request from a
//!   public client with no `code_challenge`, saying so in as many words.
//! * **The redirect URI must carry no path.** Gitea compares redirect URIs by exact string
//!   after uppercasing and trimming one trailing slash. For a public client on `http` and a
//!   loopback IP it first strips the *port* and compares again — but not the path. The built-in
//!   applications register `http://127.0.0.1`, so `http://127.0.0.1:45231` matches and
//!   `http://127.0.0.1:45231/callback` does not. Adding a tidy-looking `/callback` breaks every
//!   login with `redirect_uri_mismatch`. Use `127.0.0.1` and not `localhost`, too: the loopback
//!   special case parses the host as an IP address, and a name is not one.
//! * **Refresh tokens rotate.** Every refresh mints a new refresh token. Whether the previous
//!   one keeps working depends on the instance's `INVALIDATE_REFRESH_TOKENS`, so a client that
//!   keeps the old one works on some instances and locks the user out on others. Persist the new
//!   one, and persist it before using the access token that came with it.
//! * **`scope` is optional.** Only `response_type=code` and `client_id` are required. Omitting
//!   `scope` means no `openid`, which means no `id_token` — and the access token is issued just
//!   the same. That is why this module needs no JWT or JWKS machinery: the identity comes from
//!   `GET /user`, the same call the token login already makes.
//! * **Scopes are not enforced.** An OAuth2 token can do anything the user can. Gitea's
//!   `scopes_supported` lists only the OIDC identity scopes. Nothing here should record a scope
//!   list against an OAuth login, because printing one would describe a restriction that does
//!   not exist.
//!
//! # Lifetimes
//!
//! Access tokens last an hour (`ACCESS_TOKEN_EXPIRATION_TIME`, seconds). Refresh tokens last
//! 730 hours, about a month (`REFRESH_TOKEN_EXPIRATION_TIME`, hours). Both are instance
//! settings, so neither is a constant here — `expires_in` from the response is what counts.

mod discovery;
mod flow;
mod pkce;
mod stored;

pub use discovery::Endpoints;
pub use flow::{AuthorizeParams, TokenResponse, authorize_url, exchange_code, refresh};
pub use pkce::{Pkce, random_state};
pub use stored::{StoredOauth, kind_of};

/// Gitea's built-in OAuth2 application for `git-credential-oauth`.
///
/// Every Gitea instance registers this client id in `BuiltinApplications()`, with
/// `http://127.0.0.1` as a redirect URI and no client secret, so a login against an unmodified
/// instance needs no setup at all. That is the entire reason it is the default.
///
/// It is not gea's own id, and the consent screen will say "git-credential-oauth". Borrowing it
/// is a deliberate trade: zero-setup login against any instance, at the cost of a consent screen
/// that names the wrong tool. An administrator can also switch the built-in applications off
/// through `[oauth2] DEFAULT_APPLICATIONS`, which is why the id is overridable.
pub const BUILTIN_CLIENT_ID: &str = "a4792ccc-144e-407e-86c9-5e7d8d9c3269";

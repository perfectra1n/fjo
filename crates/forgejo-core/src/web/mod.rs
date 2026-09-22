//! Layer 0: the routes Forgejo never gave an API.
//!
//! # Why this module exists at all
//!
//! Everything else in this crate descends from `spec/forgejo-v16.0.5.json`. Some Forgejo
//! features are absent from it entirely — Projects (kanban boards) is the motivating one, and
//! the served spec has no `project` path of any kind. Those features exist only as web routes,
//! and a web route will not accept an API token: it answers one with exactly the `303` to
//! `/user/login` that it gives an anonymous request. No token scope fixes that, because the web
//! handlers and `/api/v1` are separate middleware stacks that do not fall through to each other.
//!
//! So reaching them needs a different credential (a session cookie), a different transport
//! (redirects off), and a different pin. Layers 1-3 are pinned to a vendored specification this
//! repository diffs. **This module is pinned to Forgejo's source**, at the tag named below, and
//! the routes it speaks are undocumented and unversioned. The integration suite runs against
//! that same version, which is what turns a Forgejo upgrade that moves a route into a red test
//! rather than a user's bug report.
//!
//! # The shape of the credential
//!
//! Signing in yields two things, and the distinction is the whole design:
//!
//! * a **remember token**, in the `persistent` cookie — long-lived (Forgejo's
//!   `LOGIN_REMEMBER_DAYS`, 31 by default), stored, and the only thing a password is ever needed
//!   for; and
//! * a **session**, in the `session` cookie — short-lived, minted from the remember token by
//!   `GET /user/login`, and re-minted silently whenever the server says it has lapsed.
//!
//! That is the same relationship OAuth's refresh and access tokens have, deliberately: see
//! [`crate::oauth::StoredOauth`]. One mental model covers both, and the persistence rule from
//! `oauth_refresh` — write the new credential before relying on it — applies here unchanged.
//!
//! # What is *not* guessed
//!
//! A client cannot know a server's `SESSION_LIFE_TIME`, so nothing here tries to. The session's
//! validity is never predicted from a clock; it is discovered from the server's own `303`, which
//! is the only signal Forgejo gives. The remember token's expiry *is* tracked, but from the
//! `Max-Age` the server sent rather than from the documented default, so an instance that
//! configures it differently is still reported correctly.

/// The Forgejo release whose web routes and templates this module was written against.
///
/// Referenced by error messages when a response does not have the shape this module expects,
/// which is the one moment a version mismatch is worth mentioning to a user.
pub const VERIFIED_AGAINST: &str = "16.0.5";

/// The cookie Forgejo sets for a signed-in session.
///
/// Forgejo renamed this from Gitea's `i_like_gitea`; the old name appears in a great deal of
/// documentation and in every Gitea-era script, and using it here would simply never match.
pub const SESSION_COOKIE: &str = "session";

/// The cookie holding the long-term authorization token, set when a sign-in asks to be
/// remembered. Forgejo's `COOKIE_REMEMBER_NAME`, whose default is this.
pub const REMEMBER_COOKIE: &str = "persistent";

pub mod client;
pub mod login;
pub mod session;
pub mod stored;

pub use client::{Cookie, WebBody, WebClient, WebResponse};
pub use login::{LoginStep, decode_entities, password, totp};
pub use session::remint;
pub use stored::WebCredential;

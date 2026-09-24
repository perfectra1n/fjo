//! Layer 0: the routes Gitea never gave an API.
//!
//! # Why this module exists at all
//!
//! Everything else in this crate descends from `spec/gitea-v1.27.3.json`. Some Gitea
//! features are absent from it entirely — Projects (kanban boards) is the motivating one, and
//! the served spec has no `project` path of any kind. Those features exist only as web routes,
//! and a web route will not accept an API token: it answers one with exactly the `303` to
//! `/user/login` that it gives an anonymous request. No token scope fixes that, because the web
//! handlers and `/api/v1` are separate middleware stacks that do not fall through to each other.
//!
//! So reaching them needs a different credential (a session cookie), a different transport
//! (redirects off), and a different pin. Layers 1-3 are pinned to a vendored specification this
//! repository diffs. **This module is pinned to Gitea's source**, at the tag named below, and
//! the routes it speaks are undocumented and unversioned. The integration suite runs against
//! that same version, which is what turns a Gitea upgrade that moves a route into a red test
//! rather than a user's bug report.
//!
//! # The shape of the credential
//!
//! Signing in yields two things, and the distinction is the whole design:
//!
//! * a **remember token**, in the `gitea_incredible` cookie — long-lived (Gitea's
//!   `LOGIN_REMEMBER_DAYS`, 31 by default), stored, and the only thing a password is ever needed
//!   for; and
//! * a **session**, in the `i_like_gitea` cookie — short-lived, minted from the remember token by
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
//! is the only signal Gitea gives. The remember token's expiry *is* tracked, but from the
//! `Max-Age` the server sent rather than from the documented default, so an instance that
//! configures it differently is still reported correctly.

/// The Gitea release whose web routes and templates this module was written against.
///
/// Referenced by error messages when a response does not have the shape this module expects,
/// which is the one moment a version mismatch is worth mentioning to a user.
pub const VERIFIED_AGAINST: &str = "1.27.3";

/// The cookie Gitea sets for a signed-in session: `[session] COOKIE_NAME`, whose default is
/// this. Forgejo renamed its own to `session`, which is why a script written for one of the two
/// never matches the other.
pub const SESSION_COOKIE: &str = "i_like_gitea";

/// The cookie holding the long-term authorization token, set when a sign-in asks to be
/// remembered. Gitea's `COOKIE_REMEMBER_NAME`, whose default is this.
pub const REMEMBER_COOKIE: &str = "gitea_incredible";

pub mod client;
pub mod login;
pub mod session;
pub mod stored;

pub use client::{Cookie, WebBody, WebClient, WebResponse};
pub use login::{LoginStep, decode_entities, password, totp};
pub use session::remint;
pub use stored::WebCredential;

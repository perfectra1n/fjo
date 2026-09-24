//! Renewing an OAuth session before a command uses it.
//!
//! # Why this runs on its own thread
//!
//! [`Runtime::new`](crate::runtime::Runtime::new) is called from **both** sides of
//! [`block_on`](crate::runtime::block_on): from inside an async body in most commands, and
//! synchronously from `raw.rs`'s `best_effort_slug` and `nodeinfo.rs`'s `connect`. Building a
//! tokio runtime inside another one panics, so a refresh cannot simply `block_on`, and making
//! `Runtime::new` async would put `.await` in about forty files and still leave those two
//! synchronous callers to restructure.
//!
//! A fresh thread carries no tokio thread-local, so it is correct from either side. This is the
//! same shape, and the same reasoning, as `gitea_core::config::secrets`'s keyring timeout.
//!
//! # Why the new refresh token is written before the new access token is used
//!
//! Gitea mints a new refresh token on every refresh. If the write fails and we use the access
//! token anyway, the session works for an hour and is then unrecoverable: the refresh token that
//! would have renewed it was never recorded, and on an instance with
//! `INVALIDATE_REFRESH_TOKENS` the old one is already dead. So a failed write means the refresh
//! did not happen, and the old credential is used for whatever life it has left.

use std::time::Duration;

use gitea_core::config::{Credentials, HostKey, Hosts};
use gitea_core::error::Result;
use gitea_core::http::{Client, Credentials as HttpCredentials};
use gitea_core::oauth::{self, StoredOauth};

/// How long before expiry a session is renewed.
///
/// Five minutes, not thirty seconds. It costs 8% of an hour-long token and buys immunity to
/// clock drift between this machine and the server, plus the whole duration of whatever command
/// is about to run — a release upload can outlast a token that looked comfortable at the start.
pub const SKEW: Duration = Duration::from_secs(300);

/// Refresh `stored`, persist the result, and hand back the renewed session.
///
/// Synchronous. See the module comment for why it is a thread rather than a `block_on`.
pub fn refresh_blocking(
    stored: &StoredOauth,
    hosts: &mut Hosts,
    host: &HostKey,
    login: &str,
    creds: &mut Credentials<'_>,
    base_url: &str,
) -> Result<StoredOauth> {
    let token_endpoint = stored.token_endpoint.clone();
    let client_id = stored.client_id.clone();
    let refresh_token = stored.refresh_token.clone();
    let base_url = base_url.to_owned();

    let tokens = std::thread::scope(|s| {
        s.spawn(move || {
            let client = Client::builder(&base_url, HttpCredentials::default())
                .user_agent(crate::runtime::user_agent())
                .build()?;
            crate::runtime::block_on_value(oauth::refresh(
                &client,
                &token_endpoint,
                &client_id,
                &refresh_token,
            ))
        })
        .join()
        // A panic in that thread is our bug, and the join error carries nothing useful. Report
        // it as a refresh failure so the caller's warn-and-continue path handles it.
        .unwrap_or_else(|_| {
            Err(gitea_core::error::Error::new(gitea_core::ErrorKind::OauthRefreshFailed {
                host: host.to_string(),
                login: login.to_owned(),
                reason: Some("the refresh thread did not finish".to_owned()),
            }))
        })
    })?;

    let fresh = StoredOauth::from_response(
        &tokens,
        &stored.client_id,
        &stored.token_endpoint,
        jiff::Timestamp::now(),
    );
    // Persist first. See the module comment: using an access token whose refresh token was not
    // recorded trades an hour of working session for a permanent lockout.
    creds.store(hosts, host, login, &fresh.to_json()?, Vec::new(), Some("oauth2"))?;
    hosts.save_if_dirty()?;
    Ok(fresh)
}

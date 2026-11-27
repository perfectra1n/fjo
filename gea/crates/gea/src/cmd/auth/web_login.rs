//! `gea auth login --web` — the browser half of logging in.
//!
//! # Shape
//!
//! Synchronous, except for two short async stretches. The listener has to be bound before the
//! authorize URL can be built (its port is in the redirect URI), and waiting for a browser is a
//! blocking wait with nothing to overlap it with, so the natural structure is:
//!
//! 1. bind, and invent the PKCE verifier and the state — synchronous;
//! 2. find the endpoints — async;
//! 3. open the browser and wait for the reply — synchronous;
//! 4. exchange the code, ask who we are, and store — async.
//!
//! # Secrecy
//!
//! `secrecy` is deliberately not a dependency of the `gea` crate (see `auth/common.rs`), so a
//! plaintext credential must never be bound to a named `String` here. The access token travels
//! from the token response, which holds it as a `SecretString`, straight into
//! `StoredOauth::to_json` and then into `Credentials::store`. Nothing in this file should ever
//! read `let access_token = ...`.

use std::io::Write;
use std::time::Duration;

use gitea_core::ErrorKind;
use gitea_core::config::{HostEntry, HostKey, SystemEnv};
use gitea_core::error::{Error, Result, TokenSource};
use gitea_core::http::{Auth, Client, Credentials as HttpCredentials};
use gitea_core::oauth::{self, AuthorizeParams, Endpoints, Pkce, StoredOauth};

use super::callback::{Callback, Params};
use super::common::{self, Setup};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;

/// Everything `login::run` has already worked out.
pub struct Ctx<'a> {
    pub globals: &'a GlobalOpts,
    pub key: HostKey,
    pub url: String,
    pub term: Term,
    pub interactive: bool,
    pub client_id: Option<String>,
    pub no_browser: bool,
    pub timeout: Duration,
    pub insecure_storage: bool,
}

pub fn run(mut setup: Setup, ctx: Ctx<'_>) -> Result<()> {
    let client_id = client_id_for(&setup, &ctx);
    let entry = HostEntry::from_input(&ctx.url)?;
    let client = anonymous_client(&entry)?;

    // 1. Bind first: the port is part of the redirect URI, which is part of the authorize URL.
    let callback = Callback::bind(ctx.key.as_str())?;
    let pkce = Pkce::generate()?;
    let state = oauth::random_state()?;

    // 2. Endpoints. Never fails; falls back to Gitea's fixed paths.
    let endpoints: Endpoints = crate::runtime::block_on_value(Endpoints::discover(&client));

    let url = oauth::authorize_url(
        &endpoints,
        &AuthorizeParams {
            client_id: &client_id,
            redirect_uri: callback.redirect_uri(),
            state: &state,
            challenge: pkce.challenge(),
        },
    );

    // 3. The reply, however it gets here.
    let params = if ctx.no_browser {
        paste_back(&ctx, &url)?
    } else {
        open_and_wait(&setup, &ctx, &callback, &url)?
    };

    // The state is checked before the code is read, and a mismatch means the code is never sent
    // anywhere. Something other than our own browser reached the callback, and exchanging what
    // it handed us would file that session under this user's name.
    if params.state.as_deref() != Some(state.as_str()) {
        return Err(Error::new(ErrorKind::OauthStateMismatch { host: ctx.key.to_string() }));
    }
    if let Some(error) = params.error {
        return Err(Error::new(ErrorKind::OauthAuthorizationDenied {
            host: ctx.key.to_string(),
            error,
            description: params.error_description,
        }));
    }
    let Some(code) = params.code else {
        return Err(Error::new(ErrorKind::OauthAuthorizationDenied {
            host: ctx.key.to_string(),
            error: "no_code".to_owned(),
            description: Some("the reply carried neither a code nor an error".to_owned()),
        }));
    };

    // 4. Redeem it.
    crate::runtime::block_on(async move {
        let tokens = oauth::exchange_code(
            &client,
            &endpoints,
            &client_id,
            callback.redirect_uri(),
            &code,
            pkce.verifier(),
        )
        .await?;

        let stored = StoredOauth::from_response(
            &tokens,
            &client_id,
            &endpoints.token,
            jiff::Timestamp::now(),
        );

        // The same invariant the token login has: the session is filed under the account the
        // *server* says it belongs to, never under one the user asserted.
        let authed = Client::builder(
            &entry.url,
            HttpCredentials::new(Auth::Bearer(stored.access_token.clone())),
        )
        .user_agent(crate::runtime::user_agent())
        .token_source(TokenSource::Flag)
        .build()?;
        let me = common::whoami(&authed).await?;

        let mut creds = setup.credentials(Some(&ctx.key)).insecure_storage(ctx.insecure_storage);
        // No scopes recorded: Gitea does not enforce scopes on an OAuth token, so a list here
        // would make `InsufficientScope` print a restriction that was never applied.
        let source = creds.store(
            &mut setup.hosts,
            &ctx.key,
            &me.login,
            &stored.to_json()?,
            Vec::new(),
            Some("oauth2"),
        )?;

        setup.hosts.set_active(&ctx.key)?;
        setup.hosts.select_login(&ctx.key, &me.login)?;
        setup.hosts.save_if_dirty()?;

        for kind in creds.take_warnings() {
            common::warn(&kind);
        }

        let mut out = support::writer(ctx.globals)?;
        writeln!(out, "Logged in to {} as {}", ctx.key, me.login)?;
        writeln!(out, "OAuth session stored in {}", common::stored_in(&source))?;
        out.flush()?;
        Ok(())
    })
}

/// `--client-id`, then the per-host or global preference, then Gitea's built-in application.
///
/// One function so the order exists in exactly one place.
fn client_id_for(setup: &Setup, ctx: &Ctx<'_>) -> String {
    ctx.client_id
        .clone()
        .or_else(|| setup.config.oauth_client_id(Some(ctx.key.as_str())))
        .unwrap_or_else(|| oauth::BUILTIN_CLIENT_ID.to_owned())
}

/// A client with no credential, for discovery and the exchange.
///
/// The exchange authenticates by the `client_id` in its body. Presenting a credential as well —
/// and during `auth login` it would be the one being replaced — invites the server to
/// authenticate the wrong one of the two.
fn anonymous_client(entry: &HostEntry) -> Result<Client> {
    Client::builder(&entry.url, HttpCredentials::default())
        .user_agent(crate::runtime::user_agent())
        .build()
}

/// Open a browser and wait for it to come back to the loopback port.
fn open_and_wait(setup: &Setup, ctx: &Ctx<'_>, callback: &Callback, url: &str) -> Result<Params> {
    // The URL goes out first, and to stderr, whatever happens next. It is the one thing that
    // unblocks a user in every failure mode this function has.
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Open this URL to authorize gea:\n\n  {url}\n");
    drop(err);

    let browser = setup.config.resolved_browser(Some(ctx.key.as_str()), &SystemEnv);
    if let Err(e) = support::open_url(&ctx.term, browser, url) {
        // Not fatal: the URL is already on screen, so a browser that will not start costs the
        // user a copy and paste rather than the login.
        support::note(&ctx.term, &format!("note: {e}"));
    }
    support::note(&ctx.term, "Waiting for the browser to come back...");
    callback.wait(ctx.key.as_str(), ctx.timeout)
}

/// Ask the user to paste the redirect URL back.
///
/// For the case the loopback listener cannot serve: the browser is on a different machine from
/// `gea`. Gitea redirects to `127.0.0.1` *there*, the browser shows a connection error, and
/// the URL bar holds everything we need.
fn paste_back(ctx: &Ctx<'_>, url: &str) -> Result<Params> {
    if !ctx.interactive {
        return Err(support::usage(
            "--no-browser needs a terminal to paste the reply back on; run without it, or use \
             `--with-token` instead",
        ));
    }

    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Open this URL to authorize gea:\n\n  {url}\n");
    let _ = writeln!(
        err,
        "Your browser will then fail to reach 127.0.0.1, which is expected.\n\
         Copy the URL it ended up on and paste it here."
    );
    drop(err);

    let pasted = support::interact::prompted(
        "the redirect URL",
        inquire::Text::new("Redirect URL:")
            .with_help_message("the whole URL, starting http://127.0.0.1:")
            .prompt(),
    )?;

    // Same parser as the listener, so a pasted URL and a real request cannot disagree.
    super::callback::parse_request_target(pasted.trim()).ok_or_else(|| {
        support::usage("that URL carried no OAuth reply: it should contain `code=` or `error=`")
    })
}

//! `fjo auth login --with-password` — the sign-in that yields a web session.
//!
//! # Why a password at all, when everything else here takes a token
//!
//! Forgejo's web routes accept no token. The only credential they take is a session cookie, and
//! the only way to get the first one is to sign in the way a browser does. This is the single
//! place in `fjo` that handles a password, and it does not keep it: what is stored is the
//! *remember token* Forgejo issues, which is revocable from the web UI and lapses on its own.
//!
//! # Secrecy
//!
//! `secrecy` is deliberately not a dependency of the `fjo` crate — see `auth/common.rs` — so a
//! plaintext credential must never be bound to a named `String` here. The password travels from
//! the prompt (or stdin) straight into [`forgejo_core::web::password`] and is dropped. Nothing
//! in this file should ever read `let password = ...`.

use forgejo_core::config::{HostKey, Slot};
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::web::{LoginStep, WebClient, WebCredential, login, session};

use crate::cmd::auth::common::{Setup, warn};

pub struct Ctx<'a> {
    pub key: HostKey,
    pub url: String,
    pub interactive: bool,
    /// The global `--otp`, when the user supplied one up front.
    pub otp: Option<&'a str>,
    /// The global `--login`. Non-interactively this is the only way to name the account, which
    /// is the shape CI needs: `--login bot < password-file`.
    pub login: Option<&'a str>,
}

pub fn run(mut setup: Setup, ctx: Ctx<'_>) -> Result<()> {
    let client = WebClient::new(&ctx.url, &crate::runtime::user_agent())?;
    let user = ask_user(&ctx)?;

    let cred = crate::runtime::block_on_value(sign_in(&client, &user, &ctx));
    let cred = cred?;

    // Verify before storing, the same contract `auth login` has for a token: a credential that
    // cannot actually mint a session is one that would fail later, somewhere else, with less
    // context than here.
    let cred = crate::runtime::block_on_value(session::renew(&client, cred))?;

    store(&mut setup, &ctx.key, &cred)?;

    let when = cred.remember_expires_at.strftime("%Y-%m-%d");
    println!("✓ web session for {} on {} stored; it lapses on {when}", cred.user, ctx.key);
    println!("  renew it with `fjo auth login --host {} --with-password`", ctx.key);
    Ok(())
}

/// The two legs of a sign-in, with the second only when Forgejo asks for it.
async fn sign_in(client: &WebClient, user: &str, ctx: &Ctx<'_>) -> Result<WebCredential> {
    let now = jiff::Timestamp::now();
    // The password is read inline and moved straight in; see the module comment on secrecy.
    // Passed as an unnamed temporary: the module comment's rule is that no plaintext
    // credential is ever bound to a named `String` in this crate.
    let step = login::password(client, user, read_password(ctx)?.as_str(), now).await?;

    match step {
        LoginStep::Done(cred) => Ok(*cred),
        LoginStep::TotpRequired { state } => {
            let code = match ctx.otp {
                Some(c) => c.to_owned(),
                None if ctx.interactive => inquire::Text::new("Two-factor code:")
                    .prompt()
                    .map_err(|e| prompt_failed(&e.to_string()))?,
                None => {
                    return Err(Error::new(ErrorKind::Usage(
                        "this account needs a two-factor code: pass it with --otp <CODE>"
                            .to_owned(),
                    )));
                }
            };
            match login::totp(client, user, code.trim(), &state, now).await? {
                LoginStep::Done(cred) => Ok(*cred),
                // `totp` already turns a second prompt into a failure; this arm cannot be
                // reached, and an error beats an `unreachable!`.
                LoginStep::TotpRequired { .. } => Err(Error::new(ErrorKind::WebLoginFailed {
                    host: ctx.key.to_string(),
                    reason: Some("the two-factor code was not accepted".to_owned()),
                })),
            }
        }
    }
}

fn ask_user(ctx: &Ctx<'_>) -> Result<String> {
    if let Some(named) = ctx.login {
        return Ok(named.to_owned());
    }
    if !ctx.interactive {
        return Err(Error::new(ErrorKind::Usage(
            "no username to sign in as: pass --login <USER>, or run this on a terminal. In CI, \
             prefer setting FJO_WEB_SESSION from a session exported with \
             `fjo auth export --web`."
                .to_owned(),
        )));
    }
    inquire::Text::new("Username:")
        .prompt()
        .map(|s| s.trim().to_owned())
        .map_err(|e| prompt_failed(&e.to_string()))
}

/// Hidden on a terminal, a line from stdin otherwise, so CI can pipe one in.
fn read_password(ctx: &Ctx<'_>) -> Result<String> {
    if ctx.interactive {
        return inquire::Password::new("Password:")
            .without_confirmation()
            .prompt()
            .map_err(|e| prompt_failed(&e.to_string()));
    }
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
        .map_err(|e| Error::new(ErrorKind::Usage(format!("could not read a password: {e}"))))?;
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(Error::new(ErrorKind::Usage(
            "no password on stdin; pipe one in, or run this on a terminal".to_owned(),
        )));
    }
    Ok(trimmed.to_owned())
}

pub(crate) fn store(setup: &mut Setup, key: &HostKey, cred: &WebCredential) -> Result<()> {
    let mut creds = forgejo_core::config::Credentials::new(crate::cmd::auth::common::env())
        .with_preference(setup.config.credential_store(Some(key.as_str())));
    let doc = cred.to_json()?;
    creds.store_in(&mut setup.hosts, key, &cred.user, Slot::Web, &doc, Vec::new(), None)?;
    for kind in creds.take_warnings() {
        warn(&kind);
    }
    setup.hosts.save_if_dirty()
}

fn prompt_failed(why: &str) -> Error {
    Error::new(ErrorKind::Usage(format!("could not read the prompt: {why}")))
}

//! `fjo auth login` — the command every authentication error tells the user to run.

use std::io::{Read, Write};

use clap::Args as ClapArgs;
use forgejo_core::config::{HostEntry, HostKey};
use forgejo_core::error::{Result, TokenSource};
use forgejo_core::types::Scope;

use super::common::{self, Setup};
use super::{web_login, web_password};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Term;

/// Scopes worth suggesting interactively.
///
/// Not a validated list and not a default — Forgejo cannot be asked to mint a token over the API
/// without a password, so all `fjo` can do is tell the user what to tick in the web UI. The
/// point of printing them is that Forgejo's vocabulary is **not** GitHub's: someone following a
/// GitHub guide goes looking for `repo` and `read:org`, finds neither, and concludes the
/// instance is broken.
const SUGGESTED_SCOPES: &[&str] =
    &["read:user", "write:repository", "write:issue", "read:organization", "read:notification"];

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    /// Read the token from stdin
    #[arg(long)]
    pub with_token: bool,

    /// The token itself. Prefer --with-token: an argument is visible in `ps` and shell history
    #[arg(long, value_name = "TOKEN", conflicts_with = "with_token")]
    pub token: Option<String>,

    /// Store the token in hosts.toml (mode 0600) instead of the OS keyring
    #[arg(long)]
    pub insecure_storage: bool,

    /// Scopes the token was created with, recorded so a 403 can say what it has
    #[arg(long, value_name = "SCOPES", value_delimiter = ',')]
    pub scopes: Vec<String>,

    // Long-only on purpose, and not a doc comment because this is a note to the next person
    // editing this file rather than to a user reading --help. Every other `-w` in fjo means
    // "show me this in a browser *instead of* acting"; here the browser is how the acting
    // happens, so `fjo auth login -w` would read as "open the login page" and do something else.
    /// Log in through your browser instead of pasting a token
    #[arg(long, conflicts_with_all = ["with_token", "token"])]
    pub web: bool,

    /// Sign in with a password, for the web-only routes (`fjo web`, `fjo project`)
    ///
    /// Yields a session rather than a token. Forgejo's web routes accept no token at all, so
    /// this is the only way to reach the features it never gave an API.
    #[arg(long, conflicts_with_all = ["with_token", "token", "web"])]
    pub with_password: bool,

    /// Print the authorization URL instead of opening a browser, and paste the reply back
    #[arg(long, requires = "web")]
    pub no_browser: bool,

    /// OAuth client id, if this instance registers its own application
    #[arg(long, value_name = "ID", requires = "web")]
    pub client_id: Option<String>,

    /// Seconds to wait for the browser to come back
    #[arg(long, value_name = "SECONDS", requires = "web", default_value_t = 120)]
    pub timeout: u64,
}

const LONG_HELP: &str = "\
Verify and store a token for a Forgejo server.

The token is checked with GET /user and saved under the account it belongs to.
If the keyring is unavailable, fjo reports the fallback to hosts.toml with
0600 permissions.

With --web, fjo opens your browser, you click Authorize, and the session is
stored without a token ever crossing the clipboard. OAuth sessions are renewed
automatically and lapse after about 30 days; a token created in the web UI does
not expire, which is what CI should use.

  fjo auth login --host codeberg.org
  fjo auth login --host codeberg.org --web
  fjo auth login --host git.example.org --web --no-browser
  fjo auth login --host git.example.org --with-token < token.txt
  echo $TOKEN | fjo auth login --host git.example.org --with-token";

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Everything decidable locally is decided before the network is touched, so a missing token
    // or an unusable host is a usage error rather than a confusing 401.
    let mut setup = Setup::load()?;
    let term = Term::detect();
    let interactive = support::interact::may_prompt_for(&setup.config, globals.host.as_deref());

    let host_input = match globals.host.clone() {
        Some(h) => h,
        None if interactive => ask_host()?,
        None => {
            return Err(support::usage(
                "no host to log in to: pass --host <HOST> (a bare hostname, host:port, or a full \
                 URL for a subpath install)",
            ));
        }
    };
    let key = HostKey::parse(&host_input)?;

    // Registering the host now gives us its canonical URL for the token-creation hint. Nothing
    // reaches disk until the token verifies: `Hosts` is only ever saved on the success path.
    let (url, settings_url) = {
        let entry = setup.hosts.add_host(&host_input)?;
        (entry.url.clone(), entry.token_settings_url())
    };

    if args.with_password {
        if !args.scopes.is_empty() {
            support::note(
                &term,
                "note: --scopes is ignored for --with-password; a web session is not scoped",
            );
        }
        return web_password::run(
            setup,
            web_password::Ctx {
                key,
                url,
                interactive,
                otp: globals.otp.as_deref(),
                login: globals.login.as_deref(),
            },
        );
    }

    if args.web {
        if !args.scopes.is_empty() {
            // Recording them would make a later `InsufficientScope` print `token has: ...` for a
            // restriction Forgejo never applied, which is worse than saying nothing.
            support::note(
                &term,
                "note: --scopes is ignored for --web; Forgejo does not enforce scopes on an \
                 OAuth token",
            );
        }
        return web_login::run(
            setup,
            web_login::Ctx {
                globals,
                key,
                url,
                term,
                interactive,
                client_id: args.client_id.clone(),
                no_browser: args.no_browser,
                timeout: std::time::Duration::from_secs(args.timeout),
                insecure_storage: args.insecure_storage,
            },
        );
    }

    let token = read_token(args, &settings_url, interactive, &term)?;
    let scopes: Vec<Scope> = args.scopes.iter().map(|s| Scope::from(s.trim())).collect();

    crate::runtime::block_on(async move {
        let entry = HostEntry::from_input(&url)?;
        let client = common::client_for(&entry, &token, TokenSource::Flag)?;
        // The one call that makes this command trustworthy.
        let me = common::whoami(&client).await?;

        if let Some(asked) = globals.login.as_deref()
            && asked != me.login
        {
            // Not fatal: the token is valid and we know whose it is. Silence would be worse —
            // the user would later wonder why `--login` names an account that is not there.
            common::warn(&forgejo_core::ErrorKind::Usage(format!(
                "--login {asked} was ignored: this token belongs to {}, and a token is always \
                 filed under the account it actually authenticates as",
                me.login
            )));
        }

        let mut creds = setup.credentials(Some(&key)).insecure_storage(args.insecure_storage);
        let source = creds.store(
            &mut setup.hosts,
            &key,
            &me.login,
            &token.as_str().into(),
            scopes,
            Some("pat"),
        )?;

        setup.hosts.set_active(&key)?;
        setup.hosts.select_login(&key, &me.login)?;
        setup.hosts.save_if_dirty()?;

        // Printed after the store attempt, so a keyring that could not be reached is reported
        // together with the fallback that was used instead of it.
        for kind in creds.take_warnings() {
            common::warn(&kind);
        }

        let mut out = support::writer(globals)?;
        writeln!(out, "Logged in to {key} as {}", me.login)?;
        writeln!(out, "Token stored in {}", common::stored_in(&source))?;
        out.flush()?;
        Ok(())
    })
}

/// The interactive host question.
///
/// `codeberg.org` is offered as the default because it is the largest public Forgejo and the
/// answer for anyone trying `fjo` out; a self-hosted user types over it.
fn ask_host() -> Result<String> {
    support::interact::prompted(
        "--host",
        inquire::Text::new("Forgejo instance:")
            .with_default("codeberg.org")
            .with_help_message("a hostname, host:port, or a URL for a subpath install")
            .prompt(),
    )
}

/// `--token`, then `--with-token` from stdin, then a masked prompt, then a usage error.
fn read_token(args: &Args, settings_url: &str, interactive: bool, term: &Term) -> Result<String> {
    if let Some(t) = &args.token {
        let t = t.trim();
        if t.is_empty() {
            return Err(support::usage("--token was empty"));
        }
        support::note(
            term,
            "note: --token puts the secret in your shell history and in `ps` output; \
             --with-token reads it from stdin instead",
        );
        return Ok(t.to_owned());
    }

    if args.with_token {
        return read_stdin_token();
    }

    if !interactive {
        return Err(support::usage(
            "no token available: pipe one in with `--with-token` (or pass `--token`), because \
             there is no terminal here to prompt on",
        ));
    }

    // The hint is the whole reason this path is not just a bare prompt: Forgejo's scope names are
    // not GitHub's, and a user hunting for `repo` will not find it.
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "Create a token at {settings_url}");
    let _ = writeln!(
        err,
        "Select the required scopes (read:<area> or write:<area>). They cannot be changed later. Common scopes: {}",
        SUGGESTED_SCOPES.join(", ")
    );
    drop(err);

    let t = support::interact::prompted(
        "the token",
        inquire::Password::new("Paste your token:")
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .with_help_message("input is masked")
            .prompt(),
    )?;
    let t = t.trim().to_owned();
    if t.is_empty() {
        return Err(support::usage("no token was entered"));
    }
    Ok(t)
}

/// The whole of stdin, reduced to the first non-empty line.
///
/// `gh auth login --with-token` accepts a file with a trailing newline, and so must this: the
/// natural way to produce one is `fjo ... --with-token < token.txt`, and a token carrying a
/// `\n` fails authentication with a 401 that says nothing about whitespace.
fn read_stdin_token() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| support::usage(format!("--with-token: could not read stdin: {e}")))?;
    match buf.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(t) => Ok(t.to_owned()),
        None => Err(support::usage("--with-token: stdin was empty")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: a trailing newline surviving into the `Authorization` header, so
    /// `fjo auth login --with-token < token.txt` fails with a 401 that blames the token rather
    /// than the whitespace.
    #[test]
    fn a_token_from_stdin_loses_its_trailing_newline() {
        // `read_stdin_token` reads the real stdin, so the trimming rule is exercised through the
        // same expression it uses.
        let pick =
            |raw: &str| raw.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_owned);
        assert_eq!(pick("tok\n").as_deref(), Some("tok"));
        assert_eq!(pick("\n  tok  \nignored\n").as_deref(), Some("tok"));
        assert_eq!(pick("   \n"), None);
    }

    /// Bug this prevents: a non-interactive login hanging on a prompt nobody can answer, or
    /// failing with a message that does not name the flag to use instead.
    #[test]
    fn a_non_interactive_login_with_no_token_names_with_token() {
        let args = Args {
            with_token: false,
            token: None,
            insecure_storage: false,
            scopes: Vec::new(),
            web: false,
            with_password: false,
            no_browser: false,
            client_id: None,
            timeout: 120,
        };
        let e = read_token(&args, "https://h/user/settings/applications", false, &Term::piped())
            .unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--with-token"), "{e}");
    }

    /// Forgejo's scopes are not GitHub's, and the suggested set must be spelled in Forgejo's
    /// vocabulary or the hint sends people looking for scopes that do not exist.
    #[test]
    fn suggested_scopes_are_forgejo_scopes() {
        for s in SUGGESTED_SCOPES {
            let scope = Scope::from(*s);
            assert!(scope.parts().is_some(), "{s} is not read:<area> / write:<area>");
        }
        assert!(!SUGGESTED_SCOPES.contains(&"repo"), "that is GitHub's name");
        assert!(!SUGGESTED_SCOPES.contains(&"read:org"), "Forgejo spells it read:organization");
    }
}

//! `gea auth git-credential` — git's credential-helper protocol.
//!
//! `git` runs this; a human never does. The protocol is documented in `gitcredentials(7)`: a
//! verb as `argv[1]`, then `key=value` lines on stdin terminated by a blank line or EOF, and for
//! `get` a `key=value` reply on stdout.
//!
//! # Every failure is silent success
//!
//! Nothing here returns an error. That is not laziness — it is the protocol:
//!
//! * On `get`, a helper that cannot answer is expected to say **nothing** and exit 0, so git moves
//!   on to the next helper or prompts. Exiting non-zero makes `git push` fail outright, so a host
//!   `gea` happens not to know about would break pushes that used to work.
//! * `store` is ignored, because git would otherwise hand us a password a *user* typed and we
//!   would silently overwrite the token `auth login` verified.
//! * `erase` is ignored, and this one matters most: git erases credentials after a 401. Honouring
//!   it would mean one expired-token push silently logged the user out of `gea` itself, and the
//!   next `gea pr list` would report "you are not logged in" with no connection to the push.

use std::io::{BufRead, Write};

use clap::Args as ClapArgs;
use gitea_core::Result;
use gitea_core::config::HostKey;

use super::common::{self, Setup};
use crate::global::GlobalOpts;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// `get`, `store` or `erase`, as git passes it
    #[arg(value_name = "OPERATION")]
    pub operation: String,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    run_with(globals, args, &mut std::io::stdin().lock(), &mut std::io::stdout().lock())
}

/// The testable core. Streams are parameters so a test never touches the process's real stdin —
/// which under `cargo test` may be a terminal, and a helper that blocks on it would hang the
/// suite.
fn run_with(
    globals: &GlobalOpts,
    args: &Args,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    // Read the request even for `store` and `erase`: git writes the whole block and a helper that
    // exits without draining it makes git see EPIPE and complain.
    let request = read_request(input)?;
    if args.operation != "get" {
        return Ok(());
    }
    let Some((login, token)) = lookup(globals, &request) else { return Ok(()) };

    // Echoing protocol and host back is not required, but it lets git check that the helper
    // answered the question it was asked rather than a cached different one.
    if let Some(p) = request.get("protocol") {
        writeln!(out, "protocol={p}")?;
    }
    if let Some(h) = request.get("host") {
        writeln!(out, "host={h}")?;
    }
    writeln!(out, "username={login}")?;
    writeln!(out, "password={}", token.expose())?;
    out.flush()?;
    Ok(())
}

/// The `key=value` lines git wrote, up to the blank line or EOF.
///
/// A `Vec` rather than a map because git may legally repeat a key (`wwwauth[]` in newer versions),
/// and dropping duplicates would be a silent change of meaning. `get` looks keys up by first
/// occurrence, which is what the protocol specifies.
type Request = Vec<(String, String)>;

trait Lookup {
    fn get(&self, key: &str) -> Option<&str>;
}

impl Lookup for Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn read_request(input: &mut dyn BufRead) -> Result<Request> {
    let mut out = Request::new();
    for line in input.lines() {
        let line = line?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.to_owned(), v.to_owned()));
        }
    }
    Ok(out)
}

/// The credential for the host git is asking about, or `None`.
///
/// Longest-prefix matching through [`gitea_core::config::Hosts::match_prefix`] rather than a
/// bare hostname lookup, because two Gitea instances can sit behind one authority at different
/// path prefixes — the case `setup-git` enables `credential.useHttpPath` for. Matching on the
/// authority alone would hand instance A's token to instance B.
/// Resolve a credential for the host git is asking about, renewing an OAuth session first.
///
/// # Why the refresh belongs here and not only in `Runtime`
///
/// `git` invokes this helper whenever it needs a password, which is typically hours after
/// `auth login` and never through `Runtime`. An OAuth access token lives an hour, so without
/// this every `git push` after the first would hand git a token the server has already stopped
/// accepting.
///
/// # Why a failed refresh is still silent
///
/// The module contract above: on `get`, a helper that cannot answer says nothing and exits 0.
/// Returning an error here would make one lapsed session break `git push` outright instead of
/// letting git fall through to the next helper or prompt. That is a genuinely different failure
/// policy from `Runtime::new`'s warn-and-continue, and it is the protocol's, not a choice.
fn lookup(globals: &GlobalOpts, request: &Request) -> Option<(String, common::Credential)> {
    let host = request.get("host")?;
    let path = request.get("path").unwrap_or("");
    let mut setup = Setup::load().ok()?;

    let key = match setup.hosts.match_prefix(host, path) {
        Some((entry, _)) => entry.name.clone(),
        None => {
            let parsed = HostKey::parse(host).ok()?;
            setup.hosts.get(&parsed).map(|e| e.name.clone())?
        }
    };

    let login = setup.hosts.resolve_login(&key, globals.login.as_deref()).ok()?;
    let mut creds = setup.credentials(Some(&key));
    let token = creds.token(&mut setup.hosts, &key, &login).ok()??;
    let credential = common::Credential::new(token);

    // Renew before answering, if it is an OAuth session that is about to lapse. A failure is
    // swallowed on purpose (see above): the old access token may still have minutes left, and
    // if it does not, git's own 401 handling is a better outcome than a failed push.
    let credential = match credential.session() {
        Some(s)
            if s.can_refresh()
                && s.is_expiring(crate::oauth_refresh::SKEW, jiff::Timestamp::now()) =>
        {
            let url = setup.hosts.get(&key).map(|e| e.url.clone());
            match url.and_then(|u| {
                crate::oauth_refresh::refresh_blocking(
                    s,
                    &mut setup.hosts,
                    &key,
                    &login,
                    &mut creds,
                    &u,
                )
                .ok()
            }) {
                Some(fresh) => common::Credential::Oauth(Box::new(fresh)),
                None => credential,
            }
        }
        _ => credential,
    };

    // Deliberately not saving `hosts.toml` here: git can invoke a helper many times during one
    // fetch, and rewriting the file under a `git` process's feet buys nothing. A refresh is the
    // exception and saves itself, because a rotated refresh token that is not recorded is a
    // session that cannot be renewed again.
    Some((login, credential))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_up_to_the_blank_line() {
        let mut input = "protocol=https\nhost=git.example.org\n\nignored=yes\n".as_bytes();
        let req = read_request(&mut input).unwrap();
        assert_eq!(req.get("protocol"), Some("https"));
        assert_eq!(req.get("host"), Some("git.example.org"));
        assert_eq!(req.get("ignored"), None);
    }

    /// Bug this prevents: splitting on the last `=` (or on every `=`), which mangles a value that
    /// legitimately contains one — git sends `wwwauth[]=Basic realm="x=y"` on a 401.
    #[test]
    fn a_value_may_contain_an_equals_sign() {
        let mut input = "wwwauth[]=Basic realm=\"a=b\"\n".as_bytes();
        let req = read_request(&mut input).unwrap();
        assert_eq!(req.get("wwwauth[]"), Some("Basic realm=\"a=b\""));
    }

    /// Bug this prevents: honouring `erase`. git erases credentials after a 401, so one expired
    /// token during `git push` would silently log the user out of gea, and the next `gea`
    /// command would report "you are not logged in" with nothing linking it to the push.
    #[test]
    fn store_and_erase_do_nothing_at_all() {
        for op in ["store", "erase"] {
            let args = Args { operation: op.to_owned() };
            let mut input =
                "protocol=https\nhost=git.example.org\npassword=typed-by-a-human\n".as_bytes();
            let mut out = Vec::new();
            run_with(&GlobalOpts::default(), &args, &mut input, &mut out).unwrap();
            assert!(out.is_empty(), "{op} answered with {:?}", String::from_utf8_lossy(&out));
        }
    }
}

//! `fjo web <path>` — layer 0, the escape hatch below the escape hatch.
//!
//! `fjo api` reaches anything under `/api/v1`. This reaches anything under the instance's web
//! root, which is where Forgejo keeps the features it never gave an API — Projects above all.
//! The flags are `fjo api`'s, deliberately, so the muscle memory transfers; only the root and
//! the credential differ.
//!
//! # The session lifecycle lives here
//!
//! [`forgejo_core::web`] knows how to mint a session and how to recognise a lapsed one. It does
//! not know where credentials are stored or when to give up, because those are decisions about
//! *this* process. So the loop is here:
//!
//! 1. send with the session we have;
//! 2. if the server says it lapsed, mint a new one, **persist it**, and retry — once;
//! 3. if minting itself fails, the remember token is gone and only a password can help.
//!
//! Step 2 is bounded at one retry on purpose. Unbounded, a dead remember token becomes a hang;
//! at zero, every ordinary session expiry becomes a failed scheduled job. One is exactly the
//! number of legitimate re-mints a single request can need.
//!
//! Persisting *before* the retry is the rule [`crate::oauth_refresh`] records for the same
//! reason: a session used but not written down is one the next invocation has to mint again.

use clap::{ArgMatches, Args as ClapArgs};
use forgejo_core::ErrorKind;
use forgejo_core::error::{Error, Result};
use forgejo_core::http::Method;
use forgejo_core::web::session;
use forgejo_core::web::{Cookie, SESSION_COOKIE, WebBody, WebClient, WebCredential, WebResponse};

use crate::api::fields::{self, Typing};
use crate::global::GlobalOpts;
use crate::output::{Filter, Pipeline, Template};
use crate::runtime::Runtime;

const ID_RAW_FIELD: &str = "web-raw-field";
const ID_FIELD: &str = "web-field";

/// How close to expiry the remember token has to be before it is worth interrupting about.
///
/// Three days, because this is the one failure that cannot self-heal: everything else in this
/// file recovers silently, so a scheduled job's log should show this warning several runs before
/// it ever shows a failure.
const EXPIRY_WARNING: jiff::SignedDuration = jiff::SignedDuration::from_hours(72);

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct WebArgs {
    /// `[METHOD] PATH` — the method is optional and may be given here or with `-X`.
    ///
    /// Accepting it positionally is a deviation from `fjo api`, which takes `-X` only. It is
    /// deliberate: these routes are transcribed from a browser's network tab or from a `curl`
    /// line, where the method leads, and `fjo web POST owner/repo/projects/3/12/move` is how
    /// everyone writes it down. A leading word is treated as a method only when it IS one, so a
    /// path can never be mistaken for one.
    #[arg(value_name = "[METHOD] PATH", num_args = 1..=2, required = true)]
    pub target: Vec<String>,

    /// HTTP method; inferred as GET, or POST when any field is given
    #[arg(short = 'X', long, value_name = "METHOD")]
    pub method: Option<String>,

    /// Add a string body parameter
    #[arg(short = 'f', long = "raw-field", id = ID_RAW_FIELD, value_name = "KEY=VALUE")]
    pub raw_field: Vec<String>,

    /// Add a typed body parameter; `@file` reads a file, `@-` reads stdin
    #[arg(short = 'F', long = "field", id = ID_FIELD, value_name = "KEY=VALUE")]
    pub field: Vec<String>,

    /// Send this file as the whole body; `-` reads stdin
    #[arg(long, value_name = "FILE")]
    pub input: Option<String>,

    /// Add a request header
    #[arg(short = 'H', long = "header", value_name = "KEY: VALUE")]
    pub header: Vec<String>,

    /// Print the status line and response headers
    #[arg(short = 'i', long)]
    pub include: bool,
}

const LONG_ABOUT: &str = "\
Call a route under the instance's web root, with a signed-in session.

For the parts of Forgejo that have no REST API -- project boards above all.
These routes are undocumented and unversioned, and a token will not work on
them: Forgejo answers one with the same redirect it gives a signed-out
request. Sign in first with `fjo auth login --with-password`.

Prefer `fjo api` whenever the endpoint exists there.

  fjo web GET  myorg/myrepo/projects/3
  fjo web POST myorg/myrepo/projects/3/12/move --input cards.json";

/// The methods recognised in the leading position.
///
/// A closed list, so that a repository called `Get` is a path and not a verb — the check is
/// exact and case-sensitive-insensitive only against these.
const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

impl WebArgs {
    /// Split `target` into an optional method and the path.
    fn split(&self) -> Result<(Option<String>, String)> {
        match self.target.as_slice() {
            [one] => {
                if METHODS.contains(&one.to_ascii_uppercase().as_str()) {
                    return Err(usage(format!(
                        "{one} is an HTTP method, not a path. Give the path too, as in \
                         `fjo web {one} owner/repo/projects/3`."
                    )));
                }
                Ok((None, one.clone()))
            }
            [first, second] => {
                let m = first.to_ascii_uppercase();
                if !METHODS.contains(&m.as_str()) {
                    return Err(usage(format!(
                        "{first} is not an HTTP method. Write `fjo web <METHOD> <PATH>`, or give \
                         the path alone."
                    )));
                }
                Ok((Some(m), second.clone()))
            }
            // clap enforces 1..=2, so this is unreachable in practice; an error beats a panic.
            _ => Err(usage("fjo web takes a path, optionally preceded by an HTTP method")),
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &WebArgs, matches: &ArgMatches) -> Result<()> {
    // Web routes carry no `Link` header, so there is no next page to follow. Silently ignoring
    // the flag would hand back one page and let a script believe it had them all.
    if globals.paginate {
        return Err(usage(
            "--paginate does not work on web routes: they carry no Link header. Ask for the \
             page you want directly.",
        ));
    }

    let specs = ordered_fields(args, matches);
    let parsed = fields::parse(&specs, &mut std::io::stdin())?;
    let mut rt = Runtime::new(globals)?;
    crate::runtime::block_on(execute(&mut rt, globals, args, parsed))
}

/// The `-f`/`-F` values in the order the user typed them, which is the order they are sent.
fn ordered_fields(args: &WebArgs, matches: &ArgMatches) -> Vec<(Typing, String)> {
    let mut all: Vec<(usize, Typing, String)> = Vec::new();
    for (id, typing, values) in
        [(ID_RAW_FIELD, Typing::Raw, &args.raw_field), (ID_FIELD, Typing::Typed, &args.field)]
    {
        let indices: Vec<usize> = matches.indices_of(id).map(Iterator::collect).unwrap_or_default();
        for (n, value) in values.iter().enumerate() {
            all.push((indices.get(n).copied().unwrap_or(0), typing, value.clone()));
        }
    }
    all.sort_by_key(|(i, _, _)| *i);
    all.into_iter().map(|(_, t, v)| (t, v)).collect()
}

async fn execute(
    rt: &mut Runtime,
    globals: &GlobalOpts,
    args: &WebArgs,
    parsed: Vec<fields::Field>,
) -> Result<()> {
    let (positional, raw_path) = args.split()?;
    let method = method_for(args, positional, !parsed.is_empty())?;
    let body = body_for(args, &parsed, &method)?;
    let path = substitute(&raw_path, rt, globals)?;

    let m: Method = method.parse().map_err(|_| usage(format!("{method} is not an HTTP method")))?;
    let resp = request(rt, m, &path, body).await?;
    write_response(rt, globals, args, &resp)
}

/// One authenticated web request, with the whole session lifecycle around it.
///
/// This is the single implementation of the mint/retry/persist rule described in the module
/// comment, and both `fjo web` and `fjo project` go through it. Two copies of a retry rule are
/// two places for it to drift, and that rule *is* the reliability story.
pub(crate) async fn request(
    rt: &mut Runtime,
    method: Method,
    path: &str,
    body: WebBody,
) -> Result<WebResponse> {
    let client = rt.web_client()?;
    let mut cred = rt
        .load_web_credential()?
        .ok_or_else(|| Error::new(ErrorKind::WebSessionMissing { host: rt.host().to_string() }))?;

    warn_if_expiring(&cred);

    // No session yet is normal: a credential is stored the moment it is created, and its session
    // may since have been dropped. Minting now rather than sending a cookie-less request, which
    // would take the lapsed path anyway one round trip later.
    if cred.session.is_none() {
        cred = renew(rt, &client, cred).await?;
    }

    let resp = send(&client, method.clone(), path, body.clone(), &cred).await?;
    if !session::is_lapsed(&resp) {
        return Ok(resp);
    }
    // Exactly one retry. See the module comment.
    let cred = renew(rt, &client, cred).await?;
    send(&client, method, path, body, &cred).await
}

/// Mint a session and write it down *before* it is used.
async fn renew(rt: &mut Runtime, client: &WebClient, cred: WebCredential) -> Result<WebCredential> {
    let fresh = session::renew(client, cred).await?;
    rt.store_web_credential(&fresh)?;
    Ok(fresh)
}

async fn send(
    client: &WebClient,
    method: Method,
    path: &str,
    body: WebBody,
    cred: &WebCredential,
) -> Result<WebResponse> {
    let cookies = match &cred.session {
        Some(s) => vec![Cookie::new(SESSION_COOKIE, s.clone())],
        None => Vec::new(),
    };
    client.send(method, path, body, &cookies).await
}

/// One line, once, when the only unrecoverable failure is approaching.
fn warn_if_expiring(cred: &WebCredential) {
    if cred.is_expiring(EXPIRY_WARNING, jiff::Timestamp::now()) {
        eprintln!(
            "warning: the web session for {} lapses on {}; renew it with `fjo auth login \
             --with-password`",
            cred.user,
            cred.remember_expires_at.strftime("%Y-%m-%d")
        );
    }
}

fn method_for(args: &WebArgs, positional: Option<String>, any_field: bool) -> Result<String> {
    match (&args.method, positional) {
        // Both given and disagreeing is a typo worth stopping for, not a precedence puzzle.
        (Some(flag), Some(pos)) if flag.to_ascii_uppercase() != pos => Err(usage(format!(
            "the method is given twice and they disagree: `{pos}` and `-X {flag}`"
        ))),
        (Some(m), _) => Ok(m.to_ascii_uppercase()),
        (None, Some(pos)) => Ok(pos),
        (None, None) if any_field || args.input.is_some() => Ok("POST".to_owned()),
        (None, None) => Ok("GET".to_owned()),
    }
}

fn body_for(args: &WebArgs, parsed: &[fields::Field], method: &str) -> Result<WebBody> {
    if let Some(source) = &args.input {
        let bytes = read_input(source, &mut std::io::stdin())?;
        return Ok(WebBody::Json(bytes));
    }
    if parsed.is_empty() || method == "GET" {
        return Ok(WebBody::None);
    }
    // Forgejo's web routes are HTML forms except where its own JavaScript posts JSON, and a
    // form body is what `-f`/`-F` most nearly mean. `--input` is the way to send JSON.
    Ok(WebBody::Form(fields::to_query(parsed)))
}

fn read_input(source: &str, stdin: &mut dyn std::io::Read) -> Result<Vec<u8>> {
    if source == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(stdin, &mut buf)
            .map_err(|e| usage(format!("could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read(source).map_err(|e| usage(format!("could not read {source}: {e}")))
}

/// `{owner}` and `{repo}` from the resolved repository, as `fjo api` does.
fn substitute(path: &str, rt: &Runtime, globals: &GlobalOpts) -> Result<String> {
    if !path.contains('{') {
        return Ok(path.to_owned());
    }
    let repo = rt.repo(globals)?;
    Ok(path.replace("{owner}", &repo.slug.owner).replace("{repo}", &repo.slug.name))
}

fn write_response(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &WebArgs,
    resp: &WebResponse,
) -> Result<()> {
    if args.include {
        println!("HTTP/1.1 {}", resp.status);
        for (name, value) in &resp.headers {
            println!("{name}: {}", value.to_str().unwrap_or("<binary>"));
        }
        println!();
    }

    let looks_json = resp
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("json"));

    let wants_pipeline =
        globals.json.is_some() || globals.jq.is_some() || globals.template.is_some();

    if wants_pipeline && !looks_json {
        return Err(usage(
            "this route answered HTML, which --json, --jq and --template cannot filter. Drop \
             them, or use -i to see the response as it came.",
        ));
    }

    if looks_json {
        let value: serde_json::Value = serde_json::from_slice(&resp.body)
            .map_err(|e| usage(format!("the response is not valid JSON: {e}")))?;
        let fields = globals.json.as_deref().map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let filter = globals.jq.as_deref().map(Filter::compile).transpose()?;
        let template = globals.template.as_deref().map(Template::parse).transpose()?;
        let pipeline = Pipeline::new()
            .fields(fields.as_deref())
            .jq(filter.as_ref())
            .template(template.as_ref());
        let mut out = std::io::stdout();
        return pipeline.render(value, rt.term(), &mut out);
    }

    print!("{}", resp.text());
    Ok(())
}

fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Usage(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: &[&str]) -> WebArgs {
        WebArgs {
            target: target.iter().map(|s| (*s).to_owned()).collect(),
            method: None,
            raw_field: Vec::new(),
            field: Vec::new(),
            input: None,
            header: Vec::new(),
            include: false,
        }
    }

    #[test]
    fn a_leading_http_method_is_recognised_and_a_path_is_not() {
        let (m, p) = args(&["POST", "o/r/projects/3/12/move"]).split().expect("splits");
        assert_eq!(m.as_deref(), Some("POST"));
        assert_eq!(p, "o/r/projects/3/12/move");

        let (m, p) = args(&["o/r/projects/3"]).split().expect("splits");
        assert_eq!(m, None);
        assert_eq!(p, "o/r/projects/3");
    }

    /// Bug this prevents: a repository or owner whose name happens to read like a verb being
    /// swallowed as the method, leaving the real path missing.
    #[test]
    fn only_an_exact_http_method_leads() {
        let err = args(&["getting", "o/r/x"]).split().expect_err("refuses");
        assert!(err.to_string().contains("not an HTTP method"), "{err}");
        // And a single path that merely starts with those letters is a path.
        let (m, p) = args(&["getty/repo/projects/1"]).split().expect("splits");
        assert_eq!(m, None);
        assert_eq!(p, "getty/repo/projects/1");
    }

    /// A method with no path is a truncated command line, and guessing a path would be worse
    /// than saying so.
    #[test]
    fn a_method_alone_is_refused_with_the_shape_to_use() {
        let err = args(&["POST"]).split().expect_err("refuses");
        assert!(err.to_string().contains("fjo web POST"), "{err}");
    }

    #[test]
    fn the_method_given_twice_and_disagreeing_is_an_error_not_a_precedence_puzzle() {
        let mut a = args(&["POST", "o/r/x"]);
        a.method = Some("delete".to_owned());
        let err = method_for(&a, Some("POST".to_owned()), false).expect_err("refuses");
        assert!(err.to_string().contains("disagree"), "{err}");

        // Agreeing in different cases is fine.
        let mut a = args(&["POST", "o/r/x"]);
        a.method = Some("post".to_owned());
        assert_eq!(method_for(&a, Some("POST".to_owned()), false).expect("agrees"), "POST");
    }

    #[test]
    fn the_method_is_inferred_the_way_fjo_api_infers_it() {
        assert_eq!(method_for(&args(&["o/r/x"]), None, false).expect("get"), "GET");
        assert_eq!(method_for(&args(&["o/r/x"]), None, true).expect("post"), "POST");
        let mut a = args(&["o/r/x"]);
        a.input = Some("-".to_owned());
        assert_eq!(method_for(&a, None, false).expect("post"), "POST");
    }
}

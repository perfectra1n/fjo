//! Layer 1: `gea api <endpoint>`, modelled on `gh api`.
//!
//! The escape hatch. Every endpoint on the instance is reachable here whether or not the
//! specification this build was generated from mentions it, which is what makes `gea` useful
//! against a Gitea newer than the pinned spec — and what makes it possible to answer "does
//! this instance support X?" without waiting for a release.
//!
//! Deliberate choices, in the order they bite:
//!
//! * **The endpoint is a REST path, and `/api/v1` is optional.** `user`, `/user`, and
//!   `api/v1/user` are the same request. People paste all three out of documentation, and
//!   turning the third into `/api/v1/api/v1/user` would 404 with no hint why.
//! * **`{owner}`, `{repo}`, and `{branch}` are substituted from resolved context**, so
//!   `gea api 'repos/{owner}/{repo}/pulls'` works inside a clone. Context is resolved only
//!   when a placeholder actually needs it — shelling out to `git` for `gea api version` would
//!   be pure cost.
//! * **The method is inferred: `GET`, or `POST` once any field is supplied.** That is `gh`'s
//!   rule and it is right far more often than it is wrong; `-X` overrides it.
//! * **Errors go through the runtime's classifier, not through the body.** A 404 becomes the
//!   three-part diagnostic with its remedy, and `-i` is how you see the raw body anyway.

pub mod fields;
pub mod paginate;

use std::borrow::Cow;
use std::io::Write;

use clap::{ArgMatches, Args};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::{Accept, Body, Client, Mime, RawResponse, Request, encode, redact};
use serde_json::Value;

use crate::global::GlobalOpts;
use crate::output::{self, Filter, Pipeline, Template};
use crate::runtime::Runtime;

use fields::Typing;

/// Argument ids, spelled out so the interleaving read in [`ordered_fields`] cannot silently
/// stop matching the derive's generated names.
const ID_RAW_FIELD: &str = "raw-field";
const ID_FIELD: &str = "field";

#[derive(Debug, Clone, Args)]
pub struct ApiArgs {
    /// The endpoint path, e.g. `user`, `repos/{owner}/{repo}/pulls`, `/version`.
    #[arg(value_name = "ENDPOINT")]
    pub endpoint: String,

    /// HTTP method. Inferred as GET, or POST when any field is supplied.
    #[arg(short = 'X', long, value_name = "METHOD")]
    pub method: Option<String>,

    /// Add a string field: `-f title=hi`. Always a JSON string, never a number or a boolean.
    #[arg(short = 'f', long = "raw-field", id = ID_RAW_FIELD, value_name = "KEY=VALUE")]
    pub raw_field: Vec<String>,

    /// Add a typed field: `-F draft=true`, `-F milestone=3`, `-F body=@notes.md`, `-F body=@-`.
    #[arg(short = 'F', long = "field", id = ID_FIELD, value_name = "KEY=VALUE")]
    pub field: Vec<String>,

    /// Read the whole request body from a file; `-` reads stdin. Field flags then become query
    /// parameters.
    #[arg(long, value_name = "FILE")]
    pub input: Option<String>,

    /// Add a request header: `-H 'Accept: text/plain'`.
    #[arg(short = 'H', long = "header", value_name = "KEY: VALUE")]
    pub header: Vec<String>,

    /// Print the response status line and headers before the body.
    #[arg(short = 'i', long)]
    pub include: bool,

    /// With --paginate, return every page wrapped in one JSON array.
    #[arg(long)]
    pub slurp: bool,

    /// Do not print the response body.
    #[arg(long)]
    pub silent: bool,

    /// Print the request and the response status and headers to stderr.
    #[arg(long)]
    pub verbose: bool,
}

/// Entry point from `main`.
///
/// `matches` is the `api` subcommand's own matches, needed for one thing the derive cannot
/// give: the *interleaved* order of `-f` and `-F`, which `labels[]=a labels[]=b` depends on.
pub fn run(globals: &GlobalOpts, args: &ApiArgs, matches: &ArgMatches) -> Result<()> {
    // Everything that can be decided from the command line alone is decided **before** the
    // runtime is built, so a mistyped `--jq`, a field written without an `=`, or a bare `--json`
    // is a usage error rather than `no Gitea host is set up yet`. Reporting a configuration
    // problem for a typo sends the user to fix the wrong thing.
    let mut stdin = std::io::stdin();
    let parsed = fields::parse(&ordered_fields(args, matches), &mut stdin)?;
    let method = method_for(args, !parsed.is_empty() && !globals.paginate)?;
    let parts = PipelineParts::compile(globals)?;

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        execute(&rt, globals, args, &parsed, &method, &parts).await
    })
}

/// `-f` and `-F` values in the order they appeared on the command line.
///
/// clap hands each flag back as its own `Vec`, which loses the interleaving. `indices_of` is the
/// only way to recover it, and it matters: `-f 'labels[]' -F 'labels[]=4'` and the reverse mean
/// different things.
fn ordered_fields(args: &ApiArgs, matches: &ArgMatches) -> Vec<(Typing, String)> {
    let mut indexed: Vec<(usize, Typing, String)> = Vec::new();
    for (id, typing, values) in
        [(ID_RAW_FIELD, Typing::Raw, &args.raw_field), (ID_FIELD, Typing::Typed, &args.field)]
    {
        match matches.indices_of(id) {
            Some(idx) => {
                for (i, v) in idx.zip(values) {
                    indexed.push((i, typing, v.clone()));
                }
            }
            // No indices means the argument is absent; fall back to the declared order so this
            // still works if the function is called with hand-built matches.
            None => indexed.extend(values.iter().map(|v| (usize::MAX, typing, v.clone()))),
        }
    }
    indexed.sort_by_key(|(i, _, _)| *i);
    indexed.into_iter().map(|(_, t, v)| (t, v)).collect()
}

async fn execute(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &ApiArgs,
    parsed: &[fields::Field],
    method: &str,
    parts: &PipelineParts,
) -> Result<()> {
    // Fields cannot be a body when the method has no body, and cannot be a body when `--input`
    // already is one. Both cases send them as query parameters — which is what `gh` does, and
    // is the only reading that does not silently discard them.
    let fields_to_query = args.input.is_some() || method == "GET" || method == "HEAD";

    let (path, endpoint_query) = endpoint_path(&args.endpoint, rt, globals)?;
    let mut req = Request::get(path).accept(Accept::Json);
    set_method(&mut req, method)?;

    // Before the `-f`/`-F` fields, so the order on the wire matches the order the user typed.
    for (key, value) in endpoint_query {
        req = req.query(key, value);
    }

    if fields_to_query {
        for (key, value) in fields::to_query(parsed) {
            req = req.query(key, value);
        }
    }

    req.body = match (&args.input, parsed.is_empty(), fields_to_query) {
        (Some(source), _, _) => Body::Json(read_input(source, &mut std::io::stdin())?),
        (None, false, false) => {
            let body = fields::to_body(parsed)?;
            serde_json::to_vec(&body)
                .map(Body::Json)
                .map_err(|e| usage(format!("could not serialise the request body: {e}")))?
        }
        _ => Body::None,
    };

    for h in &args.header {
        let (name, value) = split_header(h)?;
        req = req.header(name, value);
    }

    rt.trace(&format!("{} {}", req.method, redact::url(&request_line(rt.client(), &req))));
    if args.verbose {
        trace_request(rt.client(), &req, &args.header);
    }

    let pipeline = parts.pipeline();

    if globals.paginate {
        return paginated(rt, globals, args, &req, &pipeline).await;
    }
    single(rt, globals, args, req, &pipeline).await
}

/// The compiled `--json`/`--jq`/`--template` triad.
///
/// Owned separately from the borrowed [`Pipeline`] so that one compiled filter and one parsed
/// template are reused across every page of a `--paginate` run rather than rebuilt per page.
#[derive(Debug)]
struct PipelineParts {
    fields: Option<Vec<String>>,
    filter: Option<Filter>,
    template: Option<Template>,
}

impl PipelineParts {
    fn compile(globals: &GlobalOpts) -> Result<Self> {
        let fields = match globals.json.as_deref() {
            None => None,
            // Layer 1 talks to endpoints this build may know nothing about, so there is no
            // field table to list — and inventing one from the first response would answer a
            // different question than "what can I select?".
            // One line, because the renderer restates a `Usage` message as both the headline and
            // the `problem:` fact; a paragraph here would be printed twice.
            Some("") => {
                return Err(usage(
                    "gea api cannot list fields for arbitrary endpoints. Specify --json id,name, inspect with --jq keys, or list fields with `gea raw <group> <op> --json`."
                        .to_owned(),
                ));
            }
            Some(list) => Some(
                list.split(',')
                    .map(str::trim)
                    .map(str::to_owned)
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
        };
        Ok(Self {
            fields,
            filter: globals.jq.as_deref().map(Filter::compile).transpose()?,
            template: globals.template.as_deref().map(Template::parse).transpose()?,
        })
    }

    fn pipeline(&self) -> Pipeline<'_> {
        Pipeline::new()
            .fields(self.fields.as_deref())
            .jq(self.filter.as_ref())
            .template(self.template.as_ref())
    }
}

// ------------------------------------------------------------------------------ one request

async fn single(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &ApiArgs,
    req: Request,
    pipeline: &Pipeline<'_>,
) -> Result<()> {
    // `raw` rather than `value`: it does not turn a non-2xx into an `Err`, which is what lets
    // `-i` print the body the user asked for *and* still exit with a classified code. It is
    // also the only exit that can tell us the content type, and layer 1 does not know in
    // advance whether an endpoint answers with JSON, plain text, or a zip file.
    let resp = rt.client().raw(req.clone()).await?;
    if args.verbose {
        trace_response(&resp);
    }
    let failure = rt.client().classify_raw(&req, &resp).await;

    // On a plain failure the diagnostic already carries the server's message, and printing the
    // body as well would double it. `-i` is the explicit request to see it regardless.
    if args.include || failure.is_none() {
        write_response(rt, globals, args, pipeline, &resp)?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn write_response(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &ApiArgs,
    pipeline: &Pipeline<'_>,
    resp: &RawResponse,
) -> Result<()> {
    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;

    if args.include {
        writeln!(out, "HTTP/1.1 {}", resp.status)?;
        for (name, value) in &resp.headers {
            // A response cannot carry our credential, but it can carry a `set-cookie` session
            // token, and `redact` already knows the list.
            writeln!(out, "{name}: {}", redact::header_value(name, value))?;
        }
        writeln!(out)?;
    }
    if args.silent {
        return Ok(());
    }

    let mime = Mime::new(resp.header("content-type").unwrap_or_default());
    if resp.body.is_empty() {
        // A 204, or a HEAD. Nothing to print, and an empty JSON parse would be a confusing
        // "expected value at line 1 column 1".
        return Ok(());
    }

    if mime.is_json() || looks_like_json(&resp.body) {
        let value: Value = serde_json::from_slice(&resp.body).map_err(|e| {
            Error::new(ErrorKind::Decode {
                pointer: "/".to_owned(),
                expected: e.to_string(),
                body_excerpt: String::from_utf8_lossy(&resp.body).chars().take(400).collect(),
            })
        })?;
        pipeline.render(value, rt.term(), &mut out)?;
        return Ok(());
    }

    if pipeline.is_explicit() {
        return Err(usage(format!(
            "response is {mime}, not JSON. Remove --json, --jq, and --template to print the body."
        )));
    }

    if mime.is_text() {
        // Verbatim, with no trailing newline added: `gea api repos/o/r/raw/README` is how you
        // fetch a file, and "helpfully" appending a byte corrupts it.
        out.write_all(&resp.body)?;
        return Ok(());
    }

    drop(out);
    let mut body: &[u8] = &resp.body;
    output::write_bytes(mime.as_str(), &mut body, &dest, rt.term(), globals.force)?;
    Ok(())
}

// -------------------------------------------------------------------------------- pagination

async fn paginated(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &ApiArgs,
    req: &Request,
    pipeline: &Pipeline<'_>,
) -> Result<()> {
    if args.include {
        return Err(usage(
            "--include shows one response's status and headers, and --paginate makes several \
             requests; use one or the other"
                .to_owned(),
        ));
    }

    let walked = paginate::walk(rt.client(), req, globals.limit).await?;
    rt.trace(&format!(
        "paginate: {} item(s) over {} page(s); stopped because {}",
        walked.items,
        walked.pages.len(),
        walked.stopped
    ));

    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;
    if args.silent {
        return Ok(());
    }

    if args.slurp {
        // One document: an array whose elements are the pages. `gh --slurp` does the same, and
        // it is the shape that survives `--jq 'add'`.
        pipeline.render(Value::Array(walked.pages), rt.term(), &mut out)?;
        return Ok(());
    }
    // One document per page, matching `gh --paginate`. The pipeline is reused, not rebuilt, so
    // a `--jq` expression is compiled once for a hundred pages.
    for page in walked.pages {
        pipeline.render(page, rt.term(), &mut out)?;
    }
    Ok(())
}

// ------------------------------------------------------------------------ request assembly

/// GET, or POST once any field was supplied; `-X` wins over both.
///
/// One deliberate departure from `gh`: `any_field` is passed as `false` when `--paginate` is set,
/// so `gea api repos/o/r/issues -f state=open --paginate` stays a GET with `state` in the query
/// string. `gh` would infer POST there and send the *same write* once per page. Nothing that
/// works under `gh` behaves differently here; only a command that would have been a surprising
/// write becomes the read the user meant. An explicit `-X POST --paginate` is still honoured.
///
/// Returned as a `String` rather than an `http::Method` because that type is not nameable from
/// this crate — see [`set_method`].
fn method_for(args: &ApiArgs, any_field: bool) -> Result<String> {
    if let Some(m) = &args.method {
        let upper = m.trim().to_ascii_uppercase();
        // Validate here, so `-X 'not a method'` is a usage error rather than a request nobody
        // can explain. `set_method` re-parses; the cost is one 6-byte comparison.
        set_method(&mut Request::get("/"), &upper)?;
        return Ok(upper);
    }
    // `--input` is a body, and a body on a GET is a request most servers ignore silently.
    if any_field || args.input.is_some() {
        return Ok("POST".to_owned());
    }
    Ok("GET".to_owned())
}

/// Set a request's method by name.
///
/// `http::Method` is not a direct dependency of `gea`, so the type cannot be *named* here — but
/// it can be inferred from the field being assigned, and its `FromStr` accepts any valid HTTP
/// token. That keeps every method reachable (including `HEAD` and any extension a proxy wants)
/// without `gea` taking a dependency purely to spell one type.
pub(crate) fn set_method(req: &mut Request, name: &str) -> Result<()> {
    req.method = name.parse().map_err(|_| usage(format!("{name:?} is not an HTTP method")))?;
    Ok(())
}

/// The endpoint as a path relative to `/api/v1`, with placeholders filled in, plus any query
/// the endpoint carried.
///
/// # A typed query has to leave the path
///
/// `repos/o/r/issues?limit=100` used to become [`Request::path`] whole, `?` and all. Nothing
/// downstream looks inside `path` for a query: [`Request::has_query`] inspects only the
/// structured list, so `paginate::walk`'s opt-out saw no `limit`, sent its own, and `url_for`
/// pushed a second `?` onto a URL that already had one —
/// `/api/v1/repos/o/r/issues?limit=100?limit=50&page=2`. Neither limit survives that.
///
/// Splitting here — after [`substitute`], whose values go through [`encode::seg`] and so cannot
/// contribute a `?` of their own — puts the pairs where `set_query`, `has_query`, and
/// [`encode::query_string`] all read the same one list.
fn endpoint_path(
    endpoint: &str,
    rt: &Runtime,
    globals: &GlobalOpts,
) -> Result<(String, Vec<(String, String)>)> {
    let substituted = substitute(endpoint, rt, globals)?;
    let trimmed = substituted.trim();
    if trimmed.is_empty() {
        return Err(usage("the endpoint is empty; try `gea api version`".to_owned()));
    }
    let (path, query) = match trimmed.split_once('?') {
        Some((path, qs)) => (path, split_query(qs)?),
        None => (trimmed, Vec::new()),
    };
    Ok((strip_api_prefix(path), query))
}

/// The `k=v&k=v` half of a typed endpoint, decoded.
///
/// Decoded, not copied verbatim, because the pairs are re-encoded by [`encode::query_string`] on
/// the way out: handing it the raw `a%20b` would emit `a%2520b`, a search for the six characters
/// the user escaped rather than the two they meant. See [`encode::decode_query`] for what that
/// round trip does and does not preserve.
///
/// A bare `k` with no `=` is a present-but-empty parameter (`?draft&state=open`), which is how
/// every server reads it — dropping it would silently discard a flag the user typed.
fn split_query(qs: &str) -> Result<Vec<(String, String)>> {
    let decode = |s: &str| {
        encode::decode_query(s).map(Cow::into_owned).ok_or_else(|| {
            usage(format!(
                "{s:?} in the endpoint's query is not valid UTF-8 once its %-escapes are decoded"
            ))
        })
    };
    qs.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((decode(k)?, decode(v)?))
        })
        .collect()
}

/// A leading `/` is optional and a leading `/api/v1` is redundant.
///
/// The client always prefixes `/api/v1`, so a path that already carries one must lose it here or
/// become `/api/v1/api/v1/…` — which 404s every request pasted out of API documentation, with
/// nothing in the message to say why.
fn strip_api_prefix(endpoint: &str) -> String {
    let rest = endpoint.trim().trim_start_matches('/');
    let rest = match rest.strip_prefix("api/v1/") {
        Some(after) => after,
        None if rest == "api/v1" => "",
        None => rest,
    };
    format!("/{rest}")
}

/// Replace `{owner}`, `{repo}`, and `{branch}`.
///
/// Values are percent-encoded as path segments on the way in: an owner is a single segment, and
/// a repository whose name contains a `/` (rejected by Gitea, but reachable through `-R`)
/// would otherwise silently address a different route.
fn substitute(endpoint: &str, rt: &Runtime, globals: &GlobalOpts) -> Result<String> {
    if !endpoint.contains('{') {
        return Ok(endpoint.to_owned());
    }
    let mut out = String::with_capacity(endpoint.len() + 16);
    let mut rest = endpoint;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}').map(|i| i + open) else {
            return Err(usage(format!("{endpoint:?} has an unclosed '{{'")));
        };
        let name = &rest[open + 1..close];
        let value = match name {
            "owner" => rt.repo(globals)?.slug.owner.clone(),
            "repo" => rt.repo(globals)?.slug.name.clone(),
            "branch" => rt.branch()?,
            other => {
                return Err(usage(format!(
                    "{{{other}}} is not a placeholder gea substitutes; \
                     it knows {{owner}}, {{repo}}, and {{branch}}"
                )));
            }
        };
        out.push_str(&encode::seg(&value));
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn read_input(source: &str, stdin: &mut dyn std::io::Read) -> Result<Vec<u8>> {
    if source == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(stdin, &mut buf)
            .map_err(|e| usage(format!("--input -: could not read stdin: {e}")))?;
        return Ok(buf);
    }
    std::fs::read(source).map_err(|e| usage(format!("--input {source}: {e}")))
}

fn split_header(spec: &str) -> Result<(String, String)> {
    let Some((name, value)) = spec.split_once(':') else {
        return Err(usage(format!("{spec:?} is not a header; write it as 'Name: value'")));
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(usage(format!("{spec:?} has no header name before the ':'")));
    }
    Ok((name.to_owned(), value.trim().to_owned()))
}

// ---------------------------------------------------------------------------------- tracing

/// The absolute URL, for a trace line. Assembled here rather than read off the request because
/// [`Client`] builds it privately and only exposes the base.
fn request_line(client: &Client, req: &Request) -> String {
    let mut url = format!("{}{}", client.api_base(), req.path);
    let qs = encode::query_string(req.query.iter().map(|(k, v)| (k.as_ref(), v.as_str())));
    if !qs.is_empty() {
        url.push('?');
        url.push_str(&qs);
    }
    url
}

/// `--verbose`'s request half.
///
/// Only the headers *we* were asked to add are shown, and they go through
/// [`redact::header_value`]. The credential headers — `Authorization`, `Sudo`,
/// `X-GITEA-OTP` — are attached inside the client and are not visible here at all, which is
/// the strongest form of "the token cannot leak into `--verbose` output": there is nothing to
/// redact because there is nothing to see.
fn trace_request(client: &Client, req: &Request, headers: &[String]) {
    eprintln!("> {} {}", req.method, redact::url(&request_line(client, req)));
    for h in headers {
        if let Some((name, value)) = h.split_once(':') {
            eprintln!("> {}: {}", name.trim(), redact::header_value(name.trim(), value.trim()));
        }
    }
    eprintln!(">");
}

fn trace_response(resp: &RawResponse) {
    eprintln!("< HTTP/1.1 {}", resp.status);
    for (name, value) in &resp.headers {
        eprintln!("< {name}: {}", redact::header_value(name, value));
    }
    eprintln!("<");
}

/// A body whose first non-space byte is `{` or `[` is JSON whatever the header said.
///
/// Gitea answers a few endpoints with `text/plain` and a JSON body, and one with no
/// `Content-Type` at all. Sniffing here means `--jq` works on them instead of failing with
/// "the response is text/plain, not JSON".
fn looks_like_json(body: &[u8]) -> bool {
    matches!(body.iter().find(|b| !b.is_ascii_whitespace()), Some(b'{' | b'['))
}

fn usage(msg: String) -> Error {
    Error::new(ErrorKind::Usage(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches, Parser};

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        api: ApiArgs,
    }

    fn parse(words: &[&str]) -> (ApiArgs, ArgMatches) {
        let m = Harness::command().try_get_matches_from(words).unwrap_or_else(|e| panic!("{e}"));
        let h = Harness::from_arg_matches(&m).unwrap();
        (h.api, m)
    }

    #[test]
    fn the_api_v1_prefix_is_optional_and_never_doubled() {
        // A path pasted out of API documentation already carries `/api/v1`; appending a second
        // one 404s every such request with no clue why.
        for input in ["user", "/user", "api/v1/user", "/api/v1/user", " user "] {
            assert_eq!(strip_api_prefix(input), "/user", "for {input}");
        }
        assert_eq!(strip_api_prefix("api/v1"), "/");
        // A path that merely *starts* with those letters is not the prefix.
        assert_eq!(strip_api_prefix("api/v1beta/x"), "/api/v1beta/x");
    }

    fn client() -> Client {
        use gitea_core::http::transport::{Canned, FakeTransport};
        gitea_core::http::Client::builder("https://git.example.org", gitea_core::http::Auth::None)
            .transport(std::sync::Arc::new(FakeTransport::new().fallback(Canned::json(200, "[]"))))
            .build()
            .unwrap()
    }

    /// Build the request the way [`execute`] does, minus everything that needs a runtime.
    fn request_for(endpoint: &str) -> Request {
        let (path, query) = match endpoint.split_once('?') {
            Some((p, qs)) => (strip_api_prefix(p), split_query(qs).unwrap()),
            None => (strip_api_prefix(endpoint), Vec::new()),
        };
        let mut req = Request::get(path);
        for (k, v) in query {
            req = req.query(k, v);
        }
        req
    }

    /// Bug this prevents: a query typed into the endpoint became part of `Request::path`
    /// wholesale, `?` and all. `has_query` only ever inspects the structured list, so
    /// `paginate::walk`'s `if base.has_query("limit")` opt-out read false, the paginator added
    /// its own paging, and `url_for` pushed a second `?` onto a URL that already had one:
    ///
    ///     /api/v1/repos/o/r/issues?limit=100?limit=50&page=2
    ///
    /// Two `?` means everything after the first is one opaque parameter value, so neither limit
    /// is honoured. The walk still terminated on the `Link` header, which is why this survived a
    /// passing integration test.
    #[test]
    fn a_query_typed_into_the_endpoint_does_not_collide_with_the_paginators() {
        let req = request_for("repos/o/r/issues?limit=100&state=open");
        assert_eq!(req.path, "/repos/o/r/issues", "the query must leave the path");
        assert!(req.has_query("limit"), "the paginator's opt-out reads this, and read false");
        assert!(req.has_query("state"));

        let url = request_line(&client(), &req);
        assert_eq!(url.matches('?').count(), 1, "malformed URL: {url}");
        assert_eq!(url, "https://git.example.org/api/v1/repos/o/r/issues?limit=100&state=open");

        // And `set_query` now *replaces* rather than appending a rival, which is the property
        // that makes the paginator's own `limit`/`page` unambiguous.
        let mut paged = req;
        paged.set_query("limit", 50);
        paged.set_query("page", 2);
        let url = request_line(&client(), &paged);
        assert_eq!(url.matches('?').count(), 1, "malformed URL: {url}");
        assert_eq!(url.matches("limit=").count(), 1, "two limits: {url}");
    }

    /// A user's `%`-escape must not be re-escaped on its way back out: `?q=a%20b` is a search
    /// for `a b`, and `q=a%2520b` searches for the six characters they typed to avoid it.
    #[test]
    fn a_percent_escape_in_the_endpoint_is_not_double_encoded() {
        let url = request_line(&client(), &request_for("repos/o/r/issues?q=a%20b&t=c%2B%2B"));
        assert_eq!(url, "https://git.example.org/api/v1/repos/o/r/issues?q=a%20b&t=c%2B%2B");

        // A bare key is a present-but-empty parameter, which is how a server reads it; dropping
        // it would silently discard a flag the user typed.
        let url = request_line(&client(), &request_for("repos/o/r/issues?draft&state=open"));
        assert_eq!(url, "https://git.example.org/api/v1/repos/o/r/issues?draft=&state=open");

        // An escape that is not UTF-8 has no `String` form, so it is a usage error rather than
        // a silently mangled search term.
        assert!(split_query("q=%FF").is_err());
    }

    #[test]
    fn the_method_is_get_until_a_field_appears() {
        let (args, _) = parse(&["gea", "user"]);
        assert_eq!(method_for(&args, false).unwrap(), "GET");
        // A field means the user is sending something, and sending something means POST.
        assert_eq!(method_for(&args, true).unwrap(), "POST");

        let (args, _) = parse(&["gea", "user", "-X", "delete"]);
        assert_eq!(method_for(&args, true).unwrap(), "DELETE");

        let (args, _) = parse(&["gea", "user", "--input", "-"]);
        assert_eq!(method_for(&args, false).unwrap(), "POST");

        let (args, _) = parse(&["gea", "user", "-X", "SLURP"]);
        assert!(method_for(&args, false).is_ok(), "an extension method is not our business");
        let (args, _) = parse(&["gea", "user", "-X", "not a method"]);
        assert_eq!(method_for(&args, false).unwrap_err().exit_code(), 2);
    }

    /// Bug this prevents: reading `-f` and `-F` as two separate lists, so
    /// `-F 'labels[]=1' -f 'labels[]'` clears the list the user just built.
    #[test]
    fn interleaved_field_flags_keep_their_command_line_order() {
        let (args, m) = parse(&["gea", "x", "-F", "a=1", "-f", "b=2", "-F", "c=3"]);
        let specs = ordered_fields(&args, &m);
        assert_eq!(
            specs,
            vec![
                (Typing::Typed, "a=1".to_owned()),
                (Typing::Raw, "b=2".to_owned()),
                (Typing::Typed, "c=3".to_owned()),
            ]
        );
    }

    #[test]
    fn headers_are_split_on_the_first_colon_only() {
        assert_eq!(
            split_header("Accept: application/vnd.x; q=1").unwrap(),
            ("Accept".to_owned(), "application/vnd.x; q=1".to_owned())
        );
        assert!(split_header("Accept").is_err());
        assert!(split_header(": nope").is_err());
    }

    #[test]
    fn a_json_body_is_sniffed_when_the_header_lies() {
        assert!(looks_like_json(b"  {\"a\":1}"));
        assert!(looks_like_json(b"[1]"));
        assert!(!looks_like_json(b"# heading"));
        assert!(!looks_like_json(b""));
    }

    /// Bare `--json` cannot list fields for an arbitrary endpoint, and the message has to say
    /// what to do instead rather than just refusing.
    #[test]
    fn bare_json_on_layer_one_explains_the_alternatives() {
        let globals = GlobalOpts { json: Some(String::new()), ..GlobalOpts::default() };
        let e = PipelineParts::compile(&globals).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("--jq"), "{msg}");
        assert!(msg.contains("gea raw"), "{msg}");
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn a_json_field_list_is_split_and_trimmed() {
        let globals = GlobalOpts { json: Some(" id , name ,".to_owned()), ..GlobalOpts::default() };
        let parts = PipelineParts::compile(&globals).unwrap();
        assert_eq!(parts.fields.as_deref(), Some(&["id".to_owned(), "name".to_owned()][..]));
    }
}

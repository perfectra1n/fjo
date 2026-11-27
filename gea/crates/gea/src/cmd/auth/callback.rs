//! The loopback listener that receives Gitea's OAuth redirect.
//!
//! # Why this is hand-rolled
//!
//! It reads one request line and writes one fixed response. A web framework would bring an async
//! runtime, a router and a few hundred kilobytes to a binary with a 12 MiB budget, to do
//! something that fits on a page of `std::net`.
//!
//! # Why it is synchronous
//!
//! `auth login` is synchronous until it enters `block_on`, and this runs before that. Nothing
//! else is happening while we wait for a browser, so there is nothing to overlap with — and
//! staying outside the async runtime means Ctrl-C terminates the process normally, with no
//! signal handler and no cancellation plumbing.
//!
//! # Why it lives in `gea` and not `gitea-core`
//!
//! `gitea-core` is a published SDK and every one of its tests is hermetic; binding a port is
//! the thing its transport module explicitly argues against. Keeping the socket here keeps the
//! SDK egress-only, and the parser below is pure so the interesting half needs no socket either.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use gitea_core::ErrorKind;
use gitea_core::error::{CallbackFailure, Error, Result};
use gitea_core::http::encode;

/// How long a single connection may take to send its request line. A browser sends it
/// immediately; anything slower is a scanner or a half-open socket, and must not be allowed to
/// consume the whole deadline.
const PER_CONNECTION: Duration = Duration::from_secs(2);

/// How often the accept loop wakes to re-check the deadline. A blocking `accept()` would sit in
/// a syscall and make Ctrl-C feel dead.
const POLL: Duration = Duration::from_millis(50);

/// Most of a request line is irrelevant to us, and an unbounded read from a socket anyone on the
/// machine can connect to is a memory-exhaustion invitation.
const MAX_REQUEST: usize = 8 * 1024;

/// What the browser came back with.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Params {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

impl Params {
    /// Whether this is the reply we are waiting for, as opposed to a favicon request.
    fn is_reply(&self) -> bool {
        self.code.is_some() || self.error.is_some()
    }
}

/// A bound loopback socket waiting for one redirect.
pub struct Callback {
    listener: TcpListener,
    redirect_uri: String,
}

impl Callback {
    /// Bind an ephemeral port on `127.0.0.1`.
    pub fn bind(host: &str) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| bind_failed(host, &e))?;
        let port = listener.local_addr().map_err(|e| bind_failed(host, &e))?.port();
        listener.set_nonblocking(true).map_err(|e| bind_failed(host, &e))?;
        Ok(Self { listener, redirect_uri: format!("http://127.0.0.1:{port}") })
    }

    /// The `redirect_uri` to send, which is **the origin and nothing else**.
    ///
    /// No path, no trailing slash. Gitea compares redirect URIs by exact string after
    /// uppercasing and trimming one trailing slash; for a public client on `http` and a loopback
    /// IP it first strips the port and compares again. The built-in applications register
    /// `http://127.0.0.1`, so the port is forgiven and a path is not — appending the `/callback`
    /// that every OAuth tutorial uses makes every login fail with `redirect_uri_mismatch`.
    ///
    /// `127.0.0.1` rather than `localhost` for the same reason: the loopback special case parses
    /// the host as an IP address, and a name is not one.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn port(&self) -> Option<u16> {
        self.listener.local_addr().ok().map(|a| a.port())
    }

    /// Wait for the redirect, or give up.
    pub fn wait(&self, host: &str, timeout: Duration) -> Result<Params> {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(Error::new(ErrorKind::OauthCallbackUnavailable {
                    host: host.to_owned(),
                    port: self.port(),
                    reason: CallbackFailure::Timeout(timeout.as_secs()),
                }));
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(params) = handle(stream) {
                        return Ok(params);
                    }
                    // Not the reply — a favicon fetch, or a speculative connection. Keep
                    // waiting: consuming the deadline on one of these is how a login fails for
                    // no visible reason.
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(bind_failed(host, &e)),
            }
        }
    }
}

fn bind_failed(host: &str, e: &std::io::Error) -> Error {
    Error::new(ErrorKind::OauthCallbackUnavailable {
        host: host.to_owned(),
        port: None,
        reason: CallbackFailure::Bind(e.to_string()),
    })
}

/// Read one request, answer it, and report whether it was the reply.
fn handle(mut stream: TcpStream) -> Option<Params> {
    let _ = stream.set_read_timeout(Some(PER_CONNECTION));
    let _ = stream.set_write_timeout(Some(PER_CONNECTION));

    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let line = loop {
        match stream.read(&mut chunk) {
            Ok(0) => break None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(at) = buf.windows(2).position(|w| w == b"\r\n") {
                    break String::from_utf8(buf[..at].to_vec()).ok();
                }
                if buf.len() >= MAX_REQUEST {
                    break None;
                }
            }
            Err(_) => break None,
        }
    };

    let params = line.as_deref().and_then(parse_request_target);
    let body = match &params {
        Some(p) if p.error.is_none() => PAGE_OK,
        Some(_) => PAGE_DENIED,
        None => PAGE_NOT_FOUND,
    };
    let status = if params.is_some() { "200 OK" } else { "404 Not Found" };
    // Content-Length and an explicit close, or the browser keeps the connection open waiting for
    // more and never renders the page telling the user they can go back to the terminal.
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);

    params.filter(Params::is_reply)
}

/// Pull the OAuth parameters out of an HTTP request line.
///
/// Pure, and shared with the paste-back fallback so there is one parser and one test table
/// rather than two that drift.
pub fn parse_request_target(line: &str) -> Option<Params> {
    // `GET /?code=…&state=… HTTP/1.1`, or the whole URL when a user pastes one back.
    let target = line.split_whitespace().nth(1).unwrap_or(line);
    let query = target.split_once('?').map_or("", |(_, q)| q);
    if query.is_empty() {
        return None;
    }

    let mut p = Params::default();
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(v) = encode::decode_query(v) else { continue };
        let v = v.into_owned();
        match k {
            "code" => p.code = Some(v),
            "state" => p.state = Some(v),
            "error" => p.error = Some(v),
            "error_description" => p.error_description = Some(v),
            _ => {}
        }
    }
    Some(p)
}

const PAGE_OK: &str = "<!doctype html><meta charset=utf-8><title>gea</title>\
<body style=\"font-family:system-ui;padding:3rem;max-width:32rem\">\
<h1>You are logged in.</h1><p>You can close this tab and go back to the terminal.</p>";

const PAGE_DENIED: &str = "<!doctype html><meta charset=utf-8><title>gea</title>\
<body style=\"font-family:system-ui;padding:3rem;max-width:32rem\">\
<h1>Login was not completed.</h1><p>Go back to the terminal for the details.</p>";

const PAGE_NOT_FOUND: &str = "<!doctype html><meta charset=utf-8><title>gea</title>\
<body style=\"font-family:system-ui;padding:3rem;max-width:32rem\">\
<p>gea is waiting for a login redirect.</p>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_request_line_yields_the_code_and_state() {
        let p = parse_request_target("GET /?code=abc123&state=xyz789 HTTP/1.1")
            .expect("a request carrying a code is a reply");
        assert_eq!(p.code.as_deref(), Some("abc123"));
        assert_eq!(p.state.as_deref(), Some("xyz789"));
        assert!(p.is_reply());
    }

    /// Bug this prevents: Chrome fetching /favicon.ico the moment the page loads, winning the
    /// race against the real callback, and the login failing with nothing on screen to explain
    /// it. Anything without a code or an error is not the reply and must not end the wait.
    #[test]
    fn a_favicon_request_is_not_a_callback() {
        assert!(parse_request_target("GET /favicon.ico HTTP/1.1").is_none());
        assert!(parse_request_target("GET / HTTP/1.1").is_none());
        let unrelated = parse_request_target("GET /?utm_source=x HTTP/1.1").expect("has a query");
        assert!(!unrelated.is_reply(), "a query with no code or error is not a reply");
    }

    #[test]
    fn a_callback_error_carries_the_description() {
        let p = parse_request_target(
            "GET /?error=access_denied&error_description=the+user+denied+the+request HTTP/1.1",
        )
        .expect("an error is also a reply");
        assert_eq!(p.error.as_deref(), Some("access_denied"));
        assert_eq!(p.error_description.as_deref(), Some("the user denied the request"));
        assert!(p.code.is_none());
        assert!(p.is_reply());
    }

    /// The same parser reads what a user pastes back over SSH, so a whole URL has to work as
    /// well as a request line.
    #[test]
    fn a_pasted_redirect_url_parses_the_same_way() {
        let p = parse_request_target("http://127.0.0.1:45231/?code=abc&state=xyz")
            .expect("a pasted URL is a reply");
        assert_eq!(p.code.as_deref(), Some("abc"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
    }

    /// Percent-encoded values must survive. A Gitea authorization code is URL-safe, but an
    /// error_description is prose and routinely is not.
    #[test]
    fn percent_encoded_values_are_decoded() {
        let p = parse_request_target("GET /?error=x&error_description=a%20b%2Bc HTTP/1.1")
            .expect("a reply");
        assert_eq!(p.error_description.as_deref(), Some("a b+c"));
    }

    #[test]
    fn a_malformed_request_line_is_ignored_rather_than_fatal() {
        for line in ["", "GET", "not a request at all", "GET /?"] {
            assert!(parse_request_target(line).is_none_or(|p| !p.is_reply()), "{line:?}");
        }
    }

    /// The redirect URI is the origin and nothing else. See `Callback::redirect_uri`.
    #[test]
    fn the_redirect_uri_is_a_bare_loopback_origin_with_no_path() {
        let cb = Callback::bind("git.example.org").expect("binding a loopback port");
        let uri = cb.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(!uri.contains("localhost"), "Gitea parses the host as an IP: {uri}");
        assert_eq!(uri.matches('/').count(), 2, "no path is allowed: {uri}");
        assert!(!uri.ends_with('/'), "{uri}");
    }
}

//! Pagination: the highest-consequence module in this crate.
//!
//! # Why this is hand-written
//!
//! The Gitea Swagger document declares **no `Link` header anywhere** — the mechanism the live
//! API actually paginates with. (`X-Total-Count` fares slightly better: 32 of the 174 shared
//! `#/responses` entries declare it, and no inline operation response does.) So pagination cannot
//! be generated from the spec; it has to be discovered at runtime and treated as optional, because
//! an older instance, a stripping reverse proxy, or an endpoint that simply never emitted them
//! will send neither.
//!
//! # Why the obvious termination rule is wrong
//!
//! The intuitive rule is "if a page came back shorter than the `limit` I asked for, it was the
//! last page." It is wrong, and it fails **silently, with data loss, and with a successful exit
//! code**.
//!
//! Gitea clamps `limit` to `max_response_items` (default **50**) without saying so: no
//! warning, no header, no error. Ask for 100, get 50. Under the intuitive rule the client sees
//! `50 < 100`, concludes it has everything, and stops. `gea issue list --limit 100` on a
//! repository with 300 issues prints 50 issues and exits 0. No one notices until a script that
//! was supposed to close stale issues quietly stops after the first fifty.
//!
//! The fix is rule (c) below: compare against the **observed first-page size**, never the
//! requested limit. If page 1 came back with 50 items, then 50 is this endpoint's page size,
//! and only a page with fewer than 50 can be the last one.
//!
//! # The algorithm
//!
//! ```text
//! effective_limit = min(requested, caps.max_response_items)  if /settings/api succeeded
//!                 | unset (let the server choose)            otherwise
//! terminate when ANY of:
//!   a. a Link header WAS present but has no rel="next"                (authoritative)
//!   b. returned == 0                                                  (always safe)
//!   c. no Link header at all AND returned < OBSERVED first-page size
//!   d. total_count known AND cumulative >= total_count
//!   e. cumulative >= the user's --limit N
//! follow Link rel="next" verbatim when present; else increment `page`
//! ```
//!
//! # Why there is a sixth rule, and why it is an error
//!
//! Rules (a)-(e) all require the **server** to cooperate: to send a `Link`, to send an
//! `X-Total-Count`, to run out of items, or to honour `?page`. An instance that ignores `?page`
//! and re-serves page 1 forever, behind a proxy that strips both headers, satisfies none of
//! them: every page is a full page, no header ever says "last", and the walk falls through to
//! `Next::Page(n + 1)` for ever — 100% CPU, unbounded memory, no output, no error. That is not
//! hypothetical; it is what a `?page`-ignoring instance does, and it is what [`FakeTransport`]
//! does when a canned reply ignores the page parameter.
//!
//! So there is a rule (f), [`MAX_PAGES`], that terminates **of our own accord**.
//!
//! It **errors**; it does not truncate. Quietly returning the first `MAX_PAGES` pages would be
//! the silent-data-loss failure that rules (a)-(e) exist to prevent, dressed up as a safety
//! feature: a successful exit code over an answer we know to be incomplete. The one thing worse
//! than hanging is printing a wrong answer and exiting 0.
//!
//! [`FakeTransport`]: crate::http::FakeTransport

use std::fmt;

use http::HeaderMap;

use crate::capabilities::Capabilities;
use crate::error::{Error, ErrorKind, Result};

/// The parsed `Link` header.
///
/// `present` is separate from `next.is_some()` and carries real weight: "a `Link` header exists
/// and has no `rel="next"`" is *authoritative* end-of-collection (rule a), while "no `Link`
/// header at all" means we are on our own and must fall back to counting (rule c). Collapsing
/// the two loses the distinction that makes rule (a) trustworthy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Links {
    pub present: bool,
    pub next: Option<String>,
    pub prev: Option<String>,
    pub first: Option<String>,
    pub last: Option<String>,
}

/// Parse one or more RFC 5988 `Link` header values.
///
/// Hand-written, ~40 lines, because the one thing a naive `split(',')` gets wrong is a comma
/// *inside* a URL — and Gitea emits exactly that: `?labels=bug,ci` round-trips into the
/// `Link` header on any filtered list. Splitting on commas first would truncate the `next` URL
/// to `<...?labels=bug` and silently re-request page 1 with different filters, forever.
///
/// The scanner therefore tracks angle-bracket depth: commas inside `<…>` are part of the URL.
pub fn parse_link(value: &str) -> Links {
    let mut links = Links { present: true, ..Links::default() };
    let bytes = value.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Skip separators and whitespace between link-values.
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // A link-value must begin with `<uri-reference>`. Anything else is malformed; skip to
        // the next comma rather than giving up on the whole header, so one bad entry cannot
        // cost us a `rel="next"` that follows it.
        if bytes[i] != b'<' {
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            continue;
        }
        i += 1;
        let url_start = i;
        while i < bytes.len() && bytes[i] != b'>' {
            i += 1;
        }
        if i >= bytes.len() {
            break; // unterminated; nothing usable follows
        }
        let url = value[url_start..i].trim().to_owned();
        i += 1; // past '>'

        // Parameters, up to the next top-level comma. `rel` may be quoted or bare, and may
        // carry several space-separated relation types (`rel="next last"`).
        let params_start = i;
        while i < bytes.len() && bytes[i] != b',' {
            i += 1;
        }
        let params = &value[params_start..i];

        for param in params.split(';') {
            let Some((k, v)) = param.split_once('=') else { continue };
            if !k.trim().eq_ignore_ascii_case("rel") {
                continue;
            }
            let v = v.trim().trim_matches('"');
            for rel in v.split_ascii_whitespace() {
                let slot = match rel.to_ascii_lowercase().as_str() {
                    "next" => &mut links.next,
                    "prev" | "previous" => &mut links.prev,
                    "first" => &mut links.first,
                    "last" => &mut links.last,
                    _ => continue,
                };
                // First wins: a duplicated rel is malformed, and preferring the earlier one is
                // at least deterministic.
                if slot.is_none() {
                    *slot = Some(url.clone());
                }
            }
        }
    }
    links
}

/// Collect the `Link` header(s) from a response.
///
/// Multiple `Link` headers are legal and equivalent to one comma-joined header, so all values
/// are parsed. Absent entirely → `Links::default()` with `present: false`, which is what
/// rule (c) keys off.
pub fn links(headers: &HeaderMap) -> Links {
    let joined: Vec<&str> =
        headers.get_all("link").iter().filter_map(|v| v.to_str().ok()).collect();
    if joined.is_empty() {
        return Links::default();
    }
    parse_link(&joined.join(", "))
}

/// `x-total-count`, when the instance sends it.
///
/// Used for progress reporting, `--slurp` preallocation, and termination rule (d). Always
/// optional: the spec declares no headers, so an instance that omits it is not broken.
pub fn total_count(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("x-total-count")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(|s| s.parse().ok())
}

/// What one page told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    pub links: Links,
    pub total_count: Option<u64>,
    /// How many items the page actually contained. **Observed**, never requested — the entire
    /// point of this module.
    pub returned: usize,
}

impl PageInfo {
    pub fn from_headers(headers: &HeaderMap, returned: usize) -> Self {
        Self { links: links(headers), total_count: total_count(headers), returned }
    }
}

/// Why iteration stopped. Carried so `--debug` can explain a short result set instead of
/// leaving the user to wonder whether items are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Rule (a): a `Link` header was present and offered no `next`. Authoritative.
    LinkExhausted,
    /// Rule (b): the page was empty.
    EmptyPage,
    /// Rule (c): no `Link` header, and this page was shorter than the observed first page.
    ShortPageWithoutLink,
    /// Rule (d): `x-total-count` says we have them all.
    TotalReached,
    /// Rule (e): the user's `--limit` is satisfied.
    UserLimit,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StopReason::LinkExhausted => "the Link header offered no next page",
            StopReason::EmptyPage => "the page was empty",
            StopReason::ShortPageWithoutLink => {
                "no Link header, and the page was shorter than the first page"
            }
            StopReason::TotalReached => "x-total-count was reached",
            StopReason::UserLimit => "the requested limit was reached",
        })
    }
}

/// Where to go after a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Stop(StopReason),
    /// Follow this URL **verbatim**. Reconstructing it from parts would drop query parameters
    /// the server chose to add (cursor tokens, resolved defaults) and re-send ones it dropped.
    Url(String),
    /// No `Link` header: fall back to `?page=N`.
    Page(u32),
}

/// Rule (f): the hard page ceiling, after which iteration **errors**.
///
/// # Choosing the number
///
/// `max_response_items` defaults to **50**, so this ceiling times 50 — **500,000 items** — is
/// the size of collection a user could legitimately be walking before the guard fires. Nothing
/// on a Gitea instance is plausibly that big through a *command-line* client: the largest
/// public issue trackers run to tens of thousands, and the ceiling still clears the biggest of
/// them by an order of magnitude. The one collection that can genuinely exceed it is a commit
/// list on a very large mirrored repository, and half a million commits streamed through a CLI
/// is a request that wants `--limit` anyway — which is exactly what the error says.
///
/// The ceiling is deliberately **not** lower. It has to sit far enough above every real walk
/// that hitting it means something is wrong, because the consequence of firing is an error, and
/// an error on a legitimate walk would be the same silent-truncation bug from the other
/// direction: a user who really had 60,000 items being told to give up.
///
/// It is also deliberately not much higher. A runaway costs one request per page, so the
/// ceiling bounds the damage of a misbehaving instance at 10,000 requests and 500,000 buffered
/// items rather than at "until the machine runs out of memory".
pub const MAX_PAGES: u32 = 10_000;

/// Rule (f)'s error.
///
/// Free-standing so the exact wording is testable without driving a whole stream, and so the
/// two things a reader needs — what the instance did, and what to do about it — stay in one
/// place.
///
/// # On the [`ErrorKind`] used
///
/// [`ErrorKind::PaginationDidNotTerminate`] is the dedicated variant for this, and it exits **7**
/// (the server bucket) rather than 2 (usage): the request was valid and correctly formed, and the
/// far end is what misbehaved. `--limit` works *around* a broken instance; it does not correct a
/// mistake. The explanation the user reads — a `?page`-ignoring instance, a header-stripping
/// proxy, and both remedies — lives in that variant's renderer, which is where the three-part
/// error contract puts it, and `rule_f_errors_when_the_server_never_signals_an_end` asserts
/// against that rendering.
fn runaway(pages: u32, cumulative: usize) -> Error {
    Error::new(ErrorKind::PaginationDidNotTerminate { pages, items: cumulative })
}

/// Termination state for one paginated iteration.
#[derive(Debug, Clone, Default)]
pub struct Paginator {
    /// The user's `--limit N`, a cap on total items across all pages. Distinct from the
    /// per-page `limit` query parameter, which is [`effective_limit`].
    pub user_limit: Option<usize>,
    pub cumulative: usize,
    /// Set from page 1 and never updated. This is the value rule (c) compares against.
    pub first_page_size: Option<usize>,
    pub total_count: Option<u64>,
    /// Pages **consumed**, not the page to request next.
    ///
    /// Counting what happened rather than tracking an intent keeps the two ways of advancing in
    /// step. A `Link`-driven walk that loses its `Link` header partway — one endpoint emits it,
    /// its next page does not, which a reverse proxy can cause — has to fall back to `?page=N`,
    /// and an intent counter left at 1 would re-request a page already yielded. Duplicated items
    /// with a successful exit code is the same class of bug as losing them.
    pub pages_seen: u32,
}

impl Paginator {
    pub fn new(user_limit: Option<usize>) -> Self {
        Self { user_limit, ..Self::default() }
    }

    /// How many items of a freshly-fetched page to keep, honouring `--limit`.
    ///
    /// Called *before* [`Paginator::advance`], because it needs the pre-page cumulative count.
    pub fn keep(&self, returned: usize) -> usize {
        match self.user_limit {
            Some(limit) => returned.min(limit.saturating_sub(self.cumulative)),
            None => returned,
        }
    }

    /// Record a page and decide what comes next.
    ///
    /// The `Result` is rule (f), and it is a `Result` rather than a sixth [`StopReason`] on
    /// purpose: a stop reason can be ignored by a caller that only cares about items, and this
    /// one must not be. Every driver of this type — the [`ItemStream`] here, `gea`'s
    /// page-at-a-time `--paginate` walk — gets a compile error until it decides what to do,
    /// which is the only way a guard against an infinite loop stays in place.
    ///
    /// [`ItemStream`]: crate::http::ItemStream
    pub fn advance(&mut self, info: &PageInfo) -> Result<Next> {
        // Capture the first-page size *before* this page is recorded, so rule (c) cannot
        // compare page 1 against itself and stop immediately on a one-page collection.
        let observed_first = self.first_page_size;
        self.pages_seen += 1;
        self.cumulative += info.returned;
        if self.first_page_size.is_none() {
            self.first_page_size = Some(info.returned);
        }
        if let Some(t) = info.total_count {
            self.total_count = Some(t);
        }

        // (a) A Link header was present but has no next. The server has told us, explicitly,
        //     that this is the end. Nothing else can override it.
        if info.links.present && info.links.next.is_none() {
            return Ok(Next::Stop(StopReason::LinkExhausted));
        }
        // (b) An empty page always terminates. Continuing would loop forever on an endpoint
        //     that pads `Link` with a next it cannot fulfil.
        if info.returned == 0 {
            return Ok(Next::Stop(StopReason::EmptyPage));
        }
        // (e) The user asked for N and has N.
        if self.user_limit.is_some_and(|l| self.cumulative >= l) {
            return Ok(Next::Stop(StopReason::UserLimit));
        }
        // (d) The instance told us the total and we have it.
        if self.total_count.is_some_and(|t| self.cumulative as u64 >= t) {
            return Ok(Next::Stop(StopReason::TotalReached));
        }
        // (c) No Link header at all. Compare against the OBSERVED first-page size — never the
        //     limit we requested, because the server silently clamps that to
        //     max_response_items and a clamped page would look like the last one.
        if !info.links.present && observed_first.is_some_and(|first| info.returned < first) {
            return Ok(Next::Stop(StopReason::ShortPageWithoutLink));
        }

        // (f) Every rule above needed the server to say something. It said nothing, again, for
        //     MAX_PAGES pages running. Checked LAST so that a walk whose final page happens to
        //     be number MAX_PAGES still terminates normally through (a)-(e) rather than being
        //     reported as a runaway: this point is only reached when we are about to ask for
        //     page MAX_PAGES + 1.
        if self.pages_seen >= MAX_PAGES {
            return Err(runaway(self.pages_seen, self.cumulative));
        }

        Ok(match &info.links.next {
            Some(url) => Next::Url(url.clone()),
            None => Next::Page(self.pages_seen + 1),
        })
    }
}

/// The per-page `limit` query parameter to send.
///
/// When `/settings/api` answered, clamp the request to what the instance will actually honour;
/// asking for more than `max_response_items` achieves nothing except manufacturing the
/// short-page ambiguity that rule (c) exists to survive.
///
/// When it did **not** answer, send nothing and let the server pick. Guessing a limit we cannot
/// validate is how you get a value the instance clamps — the exact situation we were trying to
/// avoid. An unset `limit` yields `default_paging_num` pages, which is slower and completely
/// correct.
pub fn effective_limit(requested: Option<u32>, caps: Option<&Capabilities>) -> Option<u32> {
    let caps = caps?;
    if !caps.settings_known {
        return None;
    }
    let max = caps.max_response_items;
    Some(requested.map_or(max, |r| r.min(max)).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_the_shape_gitea_actually_sends() {
        let l = parse_link(
            r#"<https://h/api/v1/repos/o/r/issues?page=2>; rel="next", <https://h/api/v1/repos/o/r/issues?page=7>; rel="last""#,
        );
        assert!(l.present);
        assert_eq!(l.next.as_deref(), Some("https://h/api/v1/repos/o/r/issues?page=2"));
        assert_eq!(l.last.as_deref(), Some("https://h/api/v1/repos/o/r/issues?page=7"));
        assert_eq!(l.prev, None);
    }

    /// A comma inside the URL. `?labels=bug,ci` round-trips into the `Link` header on any
    /// filtered list, and a `split(',')` parser truncates `next` to `…?labels=bug` — which
    /// re-requests page 1 with *different filters*, forever.
    #[test]
    fn a_comma_inside_the_url_does_not_split_the_header() {
        let l = parse_link(r#"<https://h/api/v1/issues?labels=bug,ci&page=2>; rel="next""#);
        assert_eq!(l.next.as_deref(), Some("https://h/api/v1/issues?labels=bug,ci&page=2"));
    }

    #[test]
    fn tolerates_bare_and_multi_valued_rel() {
        let l = parse_link("<https://h/a?page=2>; rel=next");
        assert_eq!(l.next.as_deref(), Some("https://h/a?page=2"));

        let l = parse_link(r#"<https://h/a?page=9>; rel="next last""#);
        assert_eq!(l.next.as_deref(), Some("https://h/a?page=9"));
        assert_eq!(l.last.as_deref(), Some("https://h/a?page=9"));
    }

    /// One malformed entry must not cost us a valid `rel="next"` that follows it.
    #[test]
    fn a_malformed_entry_does_not_discard_the_rest() {
        let l = parse_link(r#"garbage, <https://h/a?page=2>; rel="next""#);
        assert_eq!(l.next.as_deref(), Some("https://h/a?page=2"));
    }

    /// Rule (a) depends on telling these two states apart.
    #[test]
    fn a_link_header_with_no_next_is_present_but_exhausted() {
        let l =
            parse_link(r#"<https://h/a?page=1>; rel="first", <https://h/a?page=1>; rel="last""#);
        assert!(l.present, "the header existed, which is authoritative");
        assert_eq!(l.next, None);

        let absent = links(&hm(&[("x-total-count", "3")]));
        assert!(!absent.present, "no header at all is a different situation");
    }

    #[test]
    fn total_count_is_optional_and_tolerant() {
        assert_eq!(total_count(&hm(&[("x-total-count", "317")])), Some(317));
        assert_eq!(total_count(&hm(&[("x-total-count", " 12 ")])), Some(12));
        assert_eq!(total_count(&hm(&[("x-total-count", "lots")])), None);
        assert_eq!(total_count(&HeaderMap::new()), None);
    }

    // ------------------------------------------------------------------ termination rules

    fn info(returned: usize, link: Option<&str>, total: Option<u64>) -> PageInfo {
        PageInfo { links: link.map(parse_link).unwrap_or_default(), total_count: total, returned }
    }

    #[test]
    fn rule_a_link_without_next_is_authoritative_even_on_a_full_page() {
        let mut p = Paginator::new(None);
        let i = info(50, Some(r#"<https://h/a?page=1>; rel="last""#), None);
        assert_eq!(p.advance(&i).unwrap(), Next::Stop(StopReason::LinkExhausted));
    }

    #[test]
    fn rule_b_an_empty_page_always_stops() {
        let mut p = Paginator::new(None);
        // Even with a next link that the server should not have sent.
        let i = info(0, Some(r#"<https://h/a?page=2>; rel="next""#), None);
        assert_eq!(p.advance(&i).unwrap(), Next::Stop(StopReason::EmptyPage));
    }

    /// The critical rule, in isolation: on page 1 there is no observed size yet, so a
    /// "short" first page must NOT terminate iteration.
    #[test]
    fn rule_c_never_fires_on_the_first_page() {
        let mut p = Paginator::new(None);
        // 50 items, no Link header. We do not yet know the page size, so we must ask for more.
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Page(2));
        assert_eq!(p.first_page_size, Some(50));
    }

    #[test]
    fn rule_c_compares_against_the_observed_first_page_size() {
        let mut p = Paginator::new(None);
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Page(2));
        // A second full page keeps going...
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Page(3));
        // ...and a genuinely short one ends it.
        assert_eq!(
            p.advance(&info(17, None, None)).unwrap(),
            Next::Stop(StopReason::ShortPageWithoutLink)
        );
        assert_eq!(p.cumulative, 117);
    }

    #[test]
    fn rule_d_stops_when_total_count_is_satisfied() {
        let mut p = Paginator::new(None);
        assert_eq!(p.advance(&info(50, None, Some(80))).unwrap(), Next::Page(2));
        assert_eq!(
            p.advance(&info(30, None, Some(80))).unwrap(),
            Next::Stop(StopReason::TotalReached)
        );
    }

    /// `x-total-count` often appears only on page 1; the value must be remembered.
    #[test]
    fn total_count_is_remembered_across_pages() {
        let mut p = Paginator::new(None);
        p.advance(&info(50, Some(r#"<https://h/a?page=2>; rel="next""#), Some(60))).unwrap();
        let n = p.advance(&info(10, Some(r#"<https://h/a?page=3>; rel="next""#), None)).unwrap();
        assert_eq!(n, Next::Stop(StopReason::TotalReached), "total_count from page 1 must persist");
    }

    #[test]
    fn rule_e_honours_the_user_limit_and_truncates_the_final_page() {
        let mut p = Paginator::new(Some(120));
        assert_eq!(p.keep(50), 50);
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Page(2));
        assert_eq!(p.keep(50), 50);
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Page(3));
        // Only 20 of the third page are wanted.
        assert_eq!(p.keep(50), 20);
        assert_eq!(p.advance(&info(50, None, None)).unwrap(), Next::Stop(StopReason::UserLimit));
    }

    #[test]
    fn a_next_link_is_followed_verbatim_rather_than_reconstructed() {
        let mut p = Paginator::new(None);
        let url = "https://h/api/v1/repos/o/r/issues?state=open&page=2&limit=50&cursor=xyz";
        let n = p.advance(&info(50, Some(&format!(r#"<{url}>; rel="next""#)), None)).unwrap();
        assert_eq!(n, Next::Url(url.to_owned()), "cursor tokens must survive");
        assert_eq!(p.pages_seen, 1);
    }

    /// A walk that starts on `Link` and loses the header partway must resume at the *next* page,
    /// not re-request one it already yielded. A proxy that strips `Link` on some responses
    /// produces exactly this, and duplicated items are as wrong as missing ones.
    #[test]
    fn losing_the_link_header_partway_resumes_at_the_correct_page() {
        let mut p = Paginator::new(None);
        assert_eq!(
            p.advance(&info(50, Some(r#"<https://h/a?page=2>; rel="next""#), None)).unwrap(),
            Next::Url("https://h/a?page=2".to_owned())
        );
        // Page 2 arrives with no Link header at all.
        assert_eq!(
            p.advance(&info(50, None, None)).unwrap(),
            Next::Page(3),
            "must not re-request page 2"
        );
    }

    // --------------------------------------------------------------- rule (f), the ceiling

    /// The shape no other rule catches: a full page, every time, with no `Link` and no
    /// `X-Total-Count`. Rules (a)-(e) all need the server to say something, and it never does.
    /// Before rule (f) this fell through to `Next::Page(n + 1)` for ever.
    #[test]
    fn rule_f_errors_when_the_server_never_signals_an_end() {
        let mut p = Paginator::new(None);
        // MAX_PAGES - 1 identical full pages, each asking for the next.
        for page in 1..MAX_PAGES {
            assert_eq!(
                p.advance(&info(50, None, None)).unwrap(),
                Next::Page(page + 1),
                "page {page} should still be trusted"
            );
        }
        let err = p.advance(&info(50, None, None)).unwrap_err();
        // The full three-part diagnostic, not `Display`: `Display` is deliberately the one-line
        // headline, and the explanation this test guards lives in the facts and the remedy.
        let msg = crate::error::render::render(&err, crate::error::render::Color::Never);

        // The message has to carry its own explanation: it is the only thing the user sees.
        assert!(msg.contains(&MAX_PAGES.to_string()), "must name the page count: {msg}");
        assert!(msg.contains("?page"), "must name a ?page-ignoring instance: {msg}");
        assert!(msg.contains("proxy"), "must name a header-stripping proxy: {msg}");
        assert!(msg.contains("--limit"), "must offer something actionable: {msg}");
        assert_eq!(p.pages_seen, MAX_PAGES);
    }

    /// Rule (f) is checked **last**, so a collection whose genuine final page happens to be
    /// number `MAX_PAGES` terminates normally instead of being reported as a runaway.
    #[test]
    fn rule_f_does_not_fire_when_another_rule_would_have_stopped_us_anyway() {
        let mut p = Paginator::new(None);
        for _ in 1..MAX_PAGES {
            p.advance(&info(50, None, None)).unwrap();
        }
        assert_eq!(p.pages_seen, MAX_PAGES - 1);
        // The final page is short: rule (c) owns this, not rule (f).
        assert_eq!(
            p.advance(&info(17, None, None)).unwrap(),
            Next::Stop(StopReason::ShortPageWithoutLink)
        );
    }

    /// A `Link`-driven runaway is caught too. A server can emit `rel="next"` on every page for
    /// ever just as easily as it can ignore `?page`.
    #[test]
    fn rule_f_also_bounds_a_link_that_never_runs_out() {
        let mut p = Paginator::new(None);
        let link = r#"<https://h/a?page=2>; rel="next""#;
        for _ in 1..MAX_PAGES {
            assert!(matches!(p.advance(&info(50, Some(link), None)).unwrap(), Next::Url(_)));
        }
        assert!(p.advance(&info(50, Some(link), None)).is_err());
    }

    // ----------------------------------------------------------------- effective limit

    #[test]
    fn effective_limit_clamps_to_the_instance_maximum_when_known() {
        let caps = Capabilities {
            max_response_items: 50,
            settings_known: true,
            ..Capabilities::conservative()
        };
        assert_eq!(effective_limit(Some(100), Some(&caps)), Some(50));
        assert_eq!(effective_limit(Some(20), Some(&caps)), Some(20));
        assert_eq!(effective_limit(None, Some(&caps)), Some(50));
    }

    /// When `/settings/api` is unavailable we send no `limit` at all. Guessing one we cannot
    /// validate re-creates the clamped-page ambiguity rule (c) exists to survive.
    #[test]
    fn effective_limit_is_unset_when_settings_api_did_not_answer() {
        let unknown = Capabilities::conservative(); // settings_known == false
        assert_eq!(effective_limit(Some(100), Some(&unknown)), None);
        assert_eq!(effective_limit(Some(100), None), None);
    }

    // ============================================================================
    // The regression net for the worst silent bug in this tool.
    // ============================================================================

    mod clamped_limit {
        use crate::http::auth::Auth;
        use crate::http::transport::{Canned, FakeTransport};
        use crate::http::{Client, Paging, Request};
        use futures::TryStreamExt;
        use http::Method;
        use std::sync::Arc;

        /// A Gitea instance that clamps `limit` to `max_response_items` — **silently**, which is
        /// what Gitea actually does: no warning, no header, no error, no hint of any kind.
        ///
        /// 120 issues, `max_response_items = 50`. The client asks for `limit=100`; every page comes
        /// back with at most 50. There is deliberately **no `Link` header**, because that is the
        /// configuration in which the bug is reachable: with a `Link` header, rule (a) saves you.
        fn instance_with_120_issues_and_no_link_header() -> Arc<FakeTransport> {
            const TOTAL: usize = 120;
            const CLAMP: usize = 50;

            Arc::new(
                FakeTransport::new()
                    .on(
                        Method::GET,
                        "/api/v1/settings/api",
                        Canned::json(200, r#"{"max_response_items":50,"default_paging_num":30}"#),
                    )
                    .on(Method::GET, "/api/v1/version", Canned::new(404))
                    .on_fn(Method::GET, "/api/v1/repos/o/r/issues", |call| {
                        let page: usize =
                            call.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                        // The clamp. Whatever `limit` was asked for, at most CLAMP come back.
                        let requested: usize =
                            call.query_param("limit").and_then(|p| p.parse().ok()).unwrap_or(30);
                        let per_page = requested.min(CLAMP);

                        let start = (page - 1) * per_page;
                        let end = (start + per_page).min(TOTAL);
                        let items: Vec<String> = (start..end)
                            .map(|i| format!(r#"{{"number":{},"title":"issue {}"}}"#, i + 1, i + 1))
                            .collect();
                        Canned::json(200, format!("[{}]", items.join(",")))
                            .with_header("x-total-count", TOTAL.to_string())
                    }),
            )
        }

        #[derive(Debug, serde::Deserialize)]
        struct Issue {
            number: u32,
        }

        fn client(t: Arc<FakeTransport>) -> Client {
            Client::builder("https://git.example.org", Auth::token("t"))
                .transport(t)
                .build()
                .unwrap()
        }

        /// **The test.** Ask for 100 per page against an instance that clamps to 50.
        ///
        /// The intuitive termination rule — "a page shorter than the requested `limit` is the last
        /// page" — sees `50 < 100` on page 1 and stops. That returns 50 of 120 issues, prints them,
        /// and **exits 0**. No error, no warning, no way for a script to detect it. `gea issue
        /// list --limit 100` would silently lose everything past item 50, and a cleanup script
        /// built on it would silently stop working.
        ///
        /// Rule (c) compares against the *observed* first-page size instead, so 50 is recognised as
        /// this endpoint's page size and iteration continues until a genuinely short page arrives.
        #[tokio::test]
        async fn a_server_that_clamps_100_to_50_still_yields_every_item() {
            let t = instance_with_120_issues_and_no_link_header();
            let c = client(t.clone());

            let stream = c.items_paged::<Issue>(
                Request::get("/repos/o/r/issues"),
                Paging { limit: None, per_page: Some(100) },
            );
            let items: Vec<Issue> = stream.try_collect().await.unwrap();

            assert_eq!(items.len(), 120, "the clamp must not be mistaken for the end of the data");
            let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
            assert_eq!(
                numbers,
                (1..=120).collect::<Vec<u32>>(),
                "no gaps, no duplicates, in order"
            );

            // Three data pages (50 + 50 + 20), fetched by incrementing `page` because there is no
            // Link header to follow.
            let pages = t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues");
            assert_eq!(pages.len(), 3, "50 + 50 + 20");
            assert_eq!(pages[0].query_param("page"), None, "page 1 is implicit");
            assert_eq!(pages[1].query_param("page"), Some("2"));
            assert_eq!(pages[2].query_param("page"), Some("3"));
        }

        /// The other half of the fix: the `limit` we *send* is clamped to what the instance said it
        /// will honour, so the short-page ambiguity is not manufactured in the first place.
        #[tokio::test]
        async fn the_requested_limit_is_clamped_to_max_response_items_before_being_sent() {
            let t = instance_with_120_issues_and_no_link_header();
            let c = client(t.clone());
            let _: Vec<Issue> = c
                .items_paged::<Issue>(
                    Request::get("/repos/o/r/issues"),
                    Paging { limit: None, per_page: Some(100) },
                )
                .try_collect()
                .await
                .unwrap();
            for call in t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues") {
                assert_eq!(
                    call.query_param("limit"),
                    Some("50"),
                    "asking for 100 achieves nothing"
                );
            }
        }

        /// `--limit N` must cap total items and stop early — including mid-page.
        #[tokio::test]
        async fn a_user_limit_truncates_mid_page_and_stops_fetching() {
            let t = instance_with_120_issues_and_no_link_header();
            let c = client(t.clone());
            let items: Vec<Issue> = c
                .items_paged::<Issue>(
                    Request::get("/repos/o/r/issues"),
                    Paging { limit: Some(60), per_page: Some(100) },
                )
                .try_collect()
                .await
                .unwrap();
            assert_eq!(items.len(), 60);
            assert_eq!(items.last().unwrap().number, 60);
            assert_eq!(
                t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues").len(),
                2,
                "no third page once the user's limit is satisfied"
            );
        }

        /// With a `Link` header the URLs are followed verbatim, including any parameters the
        /// server added. Reconstructing them from `page`/`limit` would drop those.
        #[tokio::test]
        async fn link_headers_are_followed_verbatim_when_present() {
            let t = Arc::new(
                FakeTransport::new()
                    .on(Method::GET, "/api/v1/settings/api", Canned::json(200, r#"{"max_response_items":50}"#))
                    .on(Method::GET, "/api/v1/version", Canned::new(404))
                    .on_fn(Method::GET, "/api/v1/repos/o/r/issues", |call| {
                        // The server's own cursor, which cannot be reconstructed by the client.
                        match call.query_param("cursor") {
                            None => Canned::json(200, r#"[{"number":1}]"#).with_header(
                                "link",
                                r#"<https://git.example.org/api/v1/repos/o/r/issues?cursor=abc>; rel="next""#,
                            ),
                            Some("abc") => Canned::json(200, r#"[{"number":2}]"#)
                                .with_header("link", r#"<https://git.example.org/api/v1/repos/o/r/issues>; rel="first""#),
                            other => panic!("unexpected cursor {other:?}"),
                        }
                    }),
            );
            let items: Vec<Issue> = client(t.clone())
                .items::<Issue>(Request::get("/repos/o/r/issues"))
                .try_collect()
                .await
                .unwrap();
            assert_eq!(items.iter().map(|i| i.number).collect::<Vec<_>>(), vec![1, 2]);
            let calls = t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues");
            assert_eq!(calls[1].query_param("cursor"), Some("abc"), "the cursor must survive");
            assert_eq!(calls.len(), 2, "rule (a): a Link header with no next is authoritative");
        }

        /// An endpoint that pads `Link` with a `next` it cannot fulfil would otherwise loop
        /// forever. Rule (b) is the backstop.
        #[tokio::test]
        async fn an_empty_page_terminates_even_with_a_next_link() {
            let t = Arc::new(
                FakeTransport::new()
                    .on(Method::GET, "/api/v1/settings/api", Canned::new(404))
                    .on(Method::GET, "/api/v1/version", Canned::new(404))
                    .on_fn(Method::GET, "/api/v1/repos/o/r/issues", |call| {
                        let page: usize =
                            call.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                        let body = if page == 1 { r#"[{"number":1}]"# } else { "[]" };
                        Canned::json(200, body).with_header(
                            "link",
                            format!(
                                r#"<https://git.example.org/api/v1/repos/o/r/issues?page={}>; rel="next""#,
                                page + 1
                            ),
                        )
                    }),
            );
            let items: Vec<Issue> = client(t.clone())
                .items::<Issue>(Request::get("/repos/o/r/issues"))
                .try_collect()
                .await
                .unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues").len(), 2);
        }

        // ================================================================
        // Rule (f): the stream must ERROR, not hang, and not truncate.
        // ================================================================

        /// An instance that ignores `?page` behind a proxy that strips `Link` and
        /// `X-Total-Count`: every request answers with the same **full** page, for ever.
        ///
        /// `max_response_items` is 3 rather than 50 only so the test costs 30,000 items instead
        /// of half a million; nothing about the shape depends on the number. The page is full by
        /// the instance's own declared maximum, which is what makes rule (c) inapplicable — the
        /// observed first-page size is 3 and every later page is also 3.
        ///
        /// # How this fails when rule (f) regresses
        ///
        /// The handler **panics** once it is asked for more pages than the ceiling allows. A
        /// wall-clock `tokio::time::timeout` would not work here and it is worth saying why: the
        /// runaway is a loop of futures that are all immediately ready, so it never yields to the
        /// scheduler and a timer task never gets to run — the timeout would be starved by the
        /// very hang it was meant to catch. Counting requests is decided by the transport, not by
        /// the clock, so it fires deterministically, in milliseconds, on any machine.
        fn an_instance_that_never_signals_the_end() -> Arc<FakeTransport> {
            const PAGE: usize = 3;
            // One request more than a correct client can make. Reaching it means rule (f) is gone.
            let ceiling = super::MAX_PAGES as usize + 1;
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            Arc::new(
                FakeTransport::new()
                    .on(
                        Method::GET,
                        "/api/v1/settings/api",
                        Canned::json(200, r#"{"max_response_items":3,"default_paging_num":3}"#),
                    )
                    .on(Method::GET, "/api/v1/version", Canned::new(404))
                    .on_fn(Method::GET, "/api/v1/repos/o/r/issues", move |_call| {
                        let n = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        assert!(
                            n <= ceiling,
                            "the paginator asked for page {n}: rule (f) (the MAX_PAGES ceiling in \
                             http::paginate) is not stopping a server that never signals an end, \
                             and this walk would run forever"
                        );
                        // The same page every time. No Link, no x-total-count, never short.
                        let items: Vec<String> =
                            (0..PAGE).map(|i| format!(r#"{{"number":{}}}"#, i + 1)).collect();
                        Canned::json(200, format!("[{}]", items.join(",")))
                    }),
            )
        }

        /// **The test.** A stream over a collection that never ends must produce an `Err`.
        ///
        /// Not a hang — that is what this replaced; `cargo test --workspace` span two threads at
        /// 100% with unbounded memory growth until it was killed.
        ///
        /// And not a silent truncation either. Yielding the first `MAX_PAGES` pages and ending
        /// the stream cleanly would hand the caller an answer we *know* is incomplete, with a
        /// successful exit code — the exact silent-data-loss failure rules (a)-(e) exist to
        /// prevent. Items already yielded stay yielded; what the caller cannot do is mistake the
        /// result for the whole collection.
        #[tokio::test]
        async fn a_server_that_never_signals_the_end_errors_rather_than_looping_forever() {
            let t = an_instance_that_never_signals_the_end();
            let started = std::time::Instant::now();

            let mut stream =
                std::pin::pin!(client(t.clone()).items::<Issue>(Request::get("/repos/o/r/issues")));
            let mut yielded = 0usize;
            let err = loop {
                match futures::StreamExt::next(&mut stream).await {
                    Some(Ok(_)) => yielded += 1,
                    Some(Err(e)) => break e,
                    None => panic!(
                        "the stream ended cleanly after {yielded} items. A truncated answer with \
                         no error is worse than the hang this replaced: the caller cannot tell it \
                         is incomplete."
                    ),
                }
            };

            // The full three-part diagnostic, not `Display`: `Display` is deliberately the
            // one-line headline, and the explanation this test guards lives in the facts and
            // the remedy.
            let msg = crate::error::render::render(&err, crate::error::render::Color::Never);
            assert!(msg.contains(&super::MAX_PAGES.to_string()), "must name the page count: {msg}");
            assert!(msg.contains("?page"), "must name a ?page-ignoring instance: {msg}");
            assert!(msg.contains("proxy"), "must name a header-stripping proxy: {msg}");
            assert!(msg.contains("--limit"), "must offer something actionable: {msg}");

            // The request naming the collection that ran away, rather than "somewhere".
            assert_eq!(err.ctx.path.as_deref(), Some("/api/v1/repos/o/r/issues"));

            assert_eq!(
                t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues").len(),
                super::MAX_PAGES as usize,
                "exactly MAX_PAGES requests, then stop"
            );
            // Everything up to the page that triggered the guard. The final page's items go with
            // the failed walk rather than being handed over: by then we have concluded the
            // instance is not paginating, and a caller that ignored the error would otherwise
            // find one more page's worth of data waiting for it.
            assert_eq!(yielded, (super::MAX_PAGES as usize - 1) * 3);

            // Documentation of intent rather than the real guard (see the fixture): the guard is
            // the request counter, which does not depend on how fast the machine is.
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "bounded pagination took {:?}, which suggests the ceiling is not bounding it",
                started.elapsed()
            );
        }

        /// `--limit` is the remedy the error names, so it had better work: a user limit stops the
        /// same runaway cleanly, with no error at all.
        #[tokio::test]
        async fn a_user_limit_escapes_a_server_that_never_signals_the_end() {
            let t = an_instance_that_never_signals_the_end();
            let items: Vec<Issue> = client(t.clone())
                .items_paged::<Issue>(
                    Request::get("/repos/o/r/issues"),
                    Paging { limit: Some(10), per_page: None },
                )
                .try_collect()
                .await
                .expect("--limit must bound the walk without erroring");
            assert_eq!(items.len(), 10);
            assert_eq!(
                t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues").len(),
                4,
                "3 + 3 + 3 + 1"
            );
        }

        /// A single short page with no `Link` header is one page, not the start of an infinite
        /// walk — rule (c) must not fire on page 1 and rule (d) closes it out.
        #[tokio::test]
        async fn a_single_short_page_is_fetched_once() {
            let t = Arc::new(
                FakeTransport::new()
                    .on(
                        Method::GET,
                        "/api/v1/settings/api",
                        Canned::json(200, r#"{"max_response_items":50}"#),
                    )
                    .on(Method::GET, "/api/v1/version", Canned::new(404))
                    .on(
                        Method::GET,
                        "/api/v1/repos/o/r/issues",
                        Canned::json(200, r#"[{"number":1},{"number":2}]"#)
                            .with_header("x-total-count", "2"),
                    ),
            );
            let items: Vec<Issue> = client(t.clone())
                .items::<Issue>(Request::get("/repos/o/r/issues"))
                .try_collect()
                .await
                .unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(t.calls_to(&Method::GET, "/api/v1/repos/o/r/issues").len(), 1);
        }
    }
}

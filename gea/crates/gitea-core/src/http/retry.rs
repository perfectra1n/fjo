//! When to try again, and — much more importantly — when not to.
//!
//! # The hard rule
//!
//! **`POST` and `PATCH` are never retried, for any reason other than `429`.**
//!
//! This is not a tunable, not a config key, and not a policy field. It is a `match` arm with
//! this comment attached, because the failure it prevents is invisible: a `POST /pulls` that
//! times out *after* the server committed the transaction, retried, creates a second pull
//! request. The user sees one success and has two PRs — or two issues, two releases, two
//! comments, two webhook deliveries. Nothing in the response tells you it happened, and no
//! amount of care at the call site can undo it.
//!
//! `429` is the one exception and it is exempt for a specific, checkable reason: a rate-limit
//! rejection happens *before* the handler runs, so the request provably was not processed.
//! That is a property of how rate limiting works, not an optimistic assumption.
//!
//! Someone will eventually propose retrying `POST` on a connect error, reasoning that nothing
//! could have been sent yet. Resist it: reqwest reports connection-pool and HTTP/2 stream
//! failures the same way, and those can occur after the bytes are on the wire.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::Method;

use crate::error::{ErrorKind, Phase};

/// Waits longer than this deserve a visible indicator naming the host and attempt. A CLI that
/// sits silent for eight seconds looks hung, and the user reaches for Ctrl-C.
pub const NOTIFY_THRESHOLD: Duration = Duration::from_secs(2);

/// Retry configuration.
///
/// The defaults are the plan's: three attempts total, 400 ms base, ±30% jitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Total attempts, **not** additional retries. `max: 3` means one try plus at most two
    /// more. Naming it "max attempts" avoids the classic off-by-one where `max_retries: 3`
    /// silently means four requests.
    pub max: u32,
    /// First backoff; doubles per attempt.
    pub base: Duration,
    /// Fractional jitter, applied symmetrically: `0.3` spreads each wait over ±30%.
    /// Without it, N concurrent clients that hit the same 503 retry in lockstep forever.
    pub jitter: f64,
    /// Cap on computed backoff, so attempt 8 of a long-lived process does not sleep for
    /// minutes.
    pub max_backoff: Duration,
    /// Cap on an honoured `Retry-After`. A misconfigured instance can send `Retry-After: 3600`;
    /// blocking a CLI for an hour is worse than failing with the number in the message.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max: 3,
            base: Duration::from_millis(400),
            jitter: 0.3,
            max_backoff: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

/// Why we are waiting, so the indicator can say something true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// `429`. Retried for every method.
    RateLimited,
    /// `5xx`. Idempotent methods only.
    ServerError(u16),
    /// Connect, TLS, DNS, or a timeout before any response bytes. Idempotent methods only.
    Transport,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// Give up and classify the failure.
    Fail,
    Retry {
        after: Duration,
        reason: RetryReason,
    },
}

impl Decision {
    pub fn is_retry(&self) -> bool {
        matches!(self, Decision::Retry { .. })
    }

    pub fn after(&self) -> Option<Duration> {
        match self {
            Decision::Retry { after, .. } => Some(*after),
            Decision::Fail => None,
        }
    }
}

/// What the client tells its progress callback while waiting.
#[derive(Debug, Clone)]
pub struct WaitNotice {
    pub host: String,
    /// The attempt that just failed, 1-based.
    pub attempt: u32,
    pub of: u32,
    pub after: Duration,
    pub reason: RetryReason,
}

/// Methods that are safe to repeat: the second request has the same effect as the first.
///
/// `GET` and `HEAD` are obvious. `PUT` and `DELETE` are idempotent *by definition* in HTTP —
/// `PUT` sets state to a value, `DELETE` removes a thing that is then already gone (a second
/// `DELETE` returning 404 is a correct, harmless outcome we surface as-is). `POST` and `PATCH`
/// are absent, and that absence is the point of this module.
pub fn is_idempotent(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS)
}

impl RetryPolicy {
    /// Disable retrying entirely. Useful for `--no-retry` and for tests that count requests.
    pub fn none() -> Self {
        Self { max: 1, ..Self::default() }
    }

    /// Decide what to do about a response status.
    ///
    /// `attempt` is 1-based and names the attempt that just produced `status`. `replayable`
    /// comes from [`super::transport::OutBody::is_replayable`]: a body read from stdin is gone.
    pub fn on_status(
        &self,
        method: &Method,
        status: u16,
        retry_after: Option<Duration>,
        attempt: u32,
        replayable: bool,
        jitter_sample: f64,
    ) -> Decision {
        if attempt >= self.max || !replayable {
            return Decision::Fail;
        }
        match status {
            // 429: exempt from the POST/PATCH prohibition, because rate limiting rejects the
            // request before the handler runs — it provably was not processed. Honour the
            // server's own number when it gave one; it knows its window and we do not.
            429 => {
                let after = retry_after
                    .map(|d| d.min(self.max_retry_after))
                    .unwrap_or_else(|| self.backoff(attempt, jitter_sample));
                Decision::Retry { after, reason: RetryReason::RateLimited }
            }

            // 5xx: the request may well have been processed before the server fell over, so
            // this is idempotent-only. See the module comment before relaxing it.
            500..=599 if is_idempotent(method) => Decision::Retry {
                after: retry_after
                    .map(|d| d.min(self.max_retry_after))
                    .unwrap_or_else(|| self.backoff(attempt, jitter_sample)),
                reason: RetryReason::ServerError(status),
            },

            _ => Decision::Fail,
        }
    }

    /// Decide what to do about a transport-level failure.
    ///
    /// Only failures that happened *before any response bytes arrived* are retryable, and only
    /// for idempotent methods. A [`Phase::Body`] timeout means the server accepted and answered
    /// the request; repeating it would repeat the side effect.
    pub fn on_transport(
        &self,
        method: &Method,
        kind: &ErrorKind,
        attempt: u32,
        replayable: bool,
        jitter_sample: f64,
    ) -> Decision {
        if attempt >= self.max || !replayable || !is_idempotent(method) {
            return Decision::Fail;
        }
        let retryable = match kind {
            ErrorKind::Dns { .. } | ErrorKind::Connect { .. } | ErrorKind::Tls { .. } => true,
            ErrorKind::Timeout { phase, .. } => {
                matches!(phase, Phase::Connect | Phase::Headers)
            }
            _ => false,
        };
        if retryable {
            Decision::Retry {
                after: self.backoff(attempt, jitter_sample),
                reason: RetryReason::Transport,
            }
        } else {
            Decision::Fail
        }
    }

    /// Exponential backoff with symmetric jitter.
    ///
    /// `jitter_sample` is a caller-supplied value in `[0, 1)` rather than drawn here, so the
    /// curve is a pure function and testable: `sample = 0.5` gives exactly the unjittered
    /// delay, `0.0` the low bound, and `~1.0` the high bound.
    pub fn backoff(&self, attempt: u32, jitter_sample: f64) -> Duration {
        let exp = attempt.saturating_sub(1).min(16);
        let base = self.base.saturating_mul(1u32 << exp).min(self.max_backoff);
        let factor = 1.0 + self.jitter * (2.0 * jitter_sample.clamp(0.0, 1.0) - 1.0);
        Duration::from_secs_f64((base.as_secs_f64() * factor).max(0.0)).min(self.max_backoff)
    }
}

/// Parse a `Retry-After` header.
///
/// Both forms in RFC 9110 are accepted, because both are seen in the wild: `delay-seconds`
/// (`Retry-After: 30`) and an HTTP-date (`Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`).
/// Supporting only the integer form is the common shortcut, and it degrades to "wait 400 ms and
/// get another 429" against any instance behind a proxy that rewrites the header — which then
/// looks like a client that ignores rate limits.
///
/// A date in the past yields `Duration::ZERO` (retry immediately), never an error: a clock skew
/// of a few seconds between client and server is normal and must not turn into a failure.
pub fn retry_after(value: &str) -> Option<Duration> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    // delay-seconds. Fractional seconds are not legal but are cheap to tolerate.
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(secs) = v.parse::<f64>()
        && secs.is_finite()
        && secs >= 0.0
    {
        return Some(Duration::from_secs_f64(secs));
    }
    http_date_delay(v, jiff::Timestamp::now())
}

/// The HTTP-date branch, with `now` injected so the test does not depend on the wall clock.
fn http_date_delay(v: &str, now: jiff::Timestamp) -> Option<Duration> {
    // HTTP-date's `GMT` is an obsolete RFC 2822 zone name. jiff accepts it, but a numeric
    // offset is the safer parse, so try the rewrite as a fallback rather than relying on it.
    let parsed = jiff::fmt::rfc2822::parse(v)
        .or_else(|_| jiff::fmt::rfc2822::parse(&v.replace(" GMT", " +0000")))
        .ok()?;
    let delta_ms = parsed.timestamp().as_millisecond() - now.as_millisecond();
    Some(Duration::from_millis(delta_ms.max(0) as u64))
}

/// A jitter source that does not cost a `rand` dependency.
///
/// xorshift64\*, seeded from the clock. The quality bar for "spread retries out a bit" is
/// nothing like the bar for cryptography or simulation, and the alternative — a fixed
/// dependency for one `f64` — is not worth it. Contention between threads can hand two callers
/// the same value, which is harmless for the same reason.
pub struct Jitter;

impl Jitter {
    /// A value in `[0, 1)`.
    pub fn sample() -> f64 {
        // 53 bits is the full mantissa of an f64, so this covers [0, 1) uniformly.
        (next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn next_u64() -> u64 {
    static STATE: OnceLock<AtomicU64> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E3779B97F4A7C15, |d| d.as_nanos() as u64);
        // xorshift is stuck at zero forever, so force a non-zero seed.
        AtomicU64::new(nanos | 1)
    });
    let mut x = state.load(Ordering::Relaxed);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    state.store(x, Ordering::Relaxed);
    x.wrapping_mul(0x2545F491_4F6CDD1D)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID: f64 = 0.5;

    /// THE rule. A retried `POST /pulls` creates two pull requests; a retried `PATCH` applies
    /// an edit twice. If this test ever fails, do not "fix" it by changing the assertion.
    #[test]
    fn post_and_patch_are_never_retried_on_5xx_or_transport_failure() {
        let p = RetryPolicy::default();
        for m in [Method::POST, Method::PATCH] {
            for status in [500, 502, 503, 504] {
                assert_eq!(
                    p.on_status(&m, status, None, 1, true, MID),
                    Decision::Fail,
                    "{m} {status} must not be retried"
                );
            }
            let kind = ErrorKind::Connect {
                host: "h".into(),
                port: 443,
                cause: "refused".into(),
                looks_like_plaintext: false,
                scheme: "https".into(),
            };
            assert_eq!(p.on_transport(&m, &kind, 1, true, MID), Decision::Fail, "{m} connect");
            let kind = ErrorKind::Timeout {
                host: "h".into(),
                after: Duration::ZERO,
                phase: Phase::Headers,
            };
            assert_eq!(p.on_transport(&m, &kind, 1, true, MID), Decision::Fail, "{m} timeout");
        }
    }

    /// The single exception, and only because a rate-limit rejection happens before the handler
    /// runs — so the request provably was not processed.
    #[test]
    fn rate_limiting_is_retried_even_for_post() {
        let p = RetryPolicy::default();
        for m in [Method::POST, Method::PATCH, Method::GET, Method::DELETE] {
            let d = p.on_status(&m, 429, Some(Duration::from_secs(5)), 1, true, MID);
            assert_eq!(
                d,
                Decision::Retry { after: Duration::from_secs(5), reason: RetryReason::RateLimited },
                "{m} 429"
            );
        }
    }

    #[test]
    fn idempotent_methods_retry_5xx_and_transport_failures() {
        let p = RetryPolicy::default();
        for m in [Method::GET, Method::HEAD, Method::PUT, Method::DELETE] {
            assert!(p.on_status(&m, 503, None, 1, true, MID).is_retry(), "{m} 503");
            let kind = ErrorKind::Dns { host: "h".into() };
            assert!(p.on_transport(&m, &kind, 1, true, MID).is_retry(), "{m} dns");
        }
    }

    /// A body timeout means the response head already arrived — the server processed the
    /// request. Repeating it repeats the side effect even for a nominally idempotent method
    /// whose handler is not.
    #[test]
    fn a_failure_after_response_bytes_is_not_retried() {
        let p = RetryPolicy::default();
        let kind =
            ErrorKind::Timeout { host: "h".into(), after: Duration::ZERO, phase: Phase::Body };
        assert_eq!(p.on_transport(&Method::GET, &kind, 1, true, MID), Decision::Fail);
    }

    /// A body read from stdin is consumed by attempt one. Retrying would send a truncated body
    /// and report success — silent corruption, which is strictly worse than a failed request.
    #[test]
    fn an_unreplayable_body_is_never_retried() {
        let p = RetryPolicy::default();
        assert_eq!(p.on_status(&Method::PUT, 503, None, 1, false, MID), Decision::Fail);
        assert_eq!(p.on_status(&Method::GET, 429, None, 1, false, MID), Decision::Fail);
    }

    /// `max` counts attempts, not retries: `max: 3` is one try plus two more, three requests
    /// total. The off-by-one here is a 33% traffic increase against every instance.
    #[test]
    fn max_counts_attempts_not_retries() {
        let p = RetryPolicy::default();
        assert!(p.on_status(&Method::GET, 503, None, 1, true, MID).is_retry());
        assert!(p.on_status(&Method::GET, 503, None, 2, true, MID).is_retry());
        assert_eq!(p.on_status(&Method::GET, 503, None, 3, true, MID), Decision::Fail);
        assert_eq!(
            RetryPolicy::none().on_status(&Method::GET, 503, None, 1, true, MID),
            Decision::Fail
        );
    }

    #[test]
    fn non_retryable_statuses_fail_immediately() {
        let p = RetryPolicy::default();
        for status in [400, 401, 403, 404, 409, 413, 422, 423] {
            assert_eq!(p.on_status(&Method::GET, status, None, 1, true, MID), Decision::Fail);
        }
    }

    #[test]
    fn backoff_doubles_and_jitter_is_symmetric_about_the_base() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(1, MID), Duration::from_millis(400));
        assert_eq!(p.backoff(2, MID), Duration::from_millis(800));
        assert_eq!(p.backoff(3, MID), Duration::from_millis(1600));
        // ±30%.
        assert_eq!(p.backoff(1, 0.0), Duration::from_millis(280));
        assert_eq!(p.backoff(1, 1.0), Duration::from_millis(520));
        // Capped.
        assert_eq!(p.backoff(20, MID), p.max_backoff);
    }

    #[test]
    fn jitter_samples_stay_in_range() {
        for _ in 0..1000 {
            let s = Jitter::sample();
            assert!((0.0..1.0).contains(&s), "{s}");
        }
    }

    #[test]
    fn retry_after_accepts_delay_seconds() {
        assert_eq!(retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(retry_after("  0 "), Some(Duration::ZERO));
        assert_eq!(retry_after("1.5"), Some(Duration::from_millis(1500)));
        assert_eq!(retry_after(""), None);
        assert_eq!(retry_after("soon"), None);
    }

    /// The form everyone forgets. A proxy that rewrites `Retry-After` into a date turns an
    /// unparsed header into "wait 400 ms, get another 429" — a client that looks like it is
    /// ignoring rate limits.
    #[test]
    fn retry_after_accepts_an_http_date() {
        let now: jiff::Timestamp = "2015-10-21T07:28:00Z".parse().unwrap();
        let d = http_date_delay("Wed, 21 Oct 2015 07:28:30 GMT", now).expect("HTTP-date form");
        assert_eq!(d, Duration::from_secs(30));
    }

    /// Clock skew between client and server is normal; a date in the past must mean "now", not
    /// an error and certainly not a negative wait.
    #[test]
    fn an_http_date_in_the_past_means_retry_immediately() {
        let now: jiff::Timestamp = "2015-10-21T07:29:00Z".parse().unwrap();
        assert_eq!(http_date_delay("Wed, 21 Oct 2015 07:28:00 GMT", now), Some(Duration::ZERO));
    }

    /// A misconfigured instance sending `Retry-After: 3600` must not park the CLI for an hour.
    #[test]
    fn an_absurd_retry_after_is_capped() {
        let p = RetryPolicy::default();
        let d = p.on_status(&Method::GET, 429, Some(Duration::from_secs(3600)), 1, true, MID);
        assert_eq!(d.after(), Some(p.max_retry_after));
    }
}

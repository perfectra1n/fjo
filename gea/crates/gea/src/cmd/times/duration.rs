//! Go-style durations, parsed and formatted.
//!
//! Gitea's tracked time is an integer number of seconds on the wire, but nobody thinks in
//! seconds: the web UI, `tea`, and every issue tracker anyone has used take `1h25m`. So the
//! command line takes `1h25m` and this module is the only place that knows how.
//!
//! # Why this is hand-written rather than delegated
//!
//! `jiff` can parse ISO-8601 (`PT1H25M`) and its own friendly format, but not Go's
//! `time.ParseDuration` syntax, and Go's is what Gitea's own documentation, its web UI hints,
//! and `tea` all use. A user who types `1h25m` must not be told to write `PT1H25M`.
//!
//! # The rules, and why each one
//!
//! * **Units**: `h`, `m`, `s` — plus `ms`, `us`/`µs`, `ns` for compatibility with Go, and `d`
//!   (24 h) and `w` (7 d) as documented extensions. Go has no `d`, because a calendar day is
//!   not a fixed duration; that objection does not apply to *tracked work time*, where "2d"
//!   means two working entries of 24 h and nobody is crossing a DST boundary in an issue
//!   comment. Refusing `2d` would send people to a calculator.
//! * **A bare number is refused.** Go accepts only `0`. `gea times add 42 90` could plausibly
//!   mean 90 seconds (the API's unit) or 90 minutes (what a human means), and guessing either
//!   way silently records the wrong number. The error names both spellings.
//! * **Fractions are allowed** (`1.5h`), like Go.
//! * **A negative duration is refused.** Go accepts `-1h`; the API would take it and reduce the
//!   issue's total, which is a way to falsify a timesheet by typo rather than a feature.
//! * **Whitespace between components is allowed**, so [`format`]'s output (`1h 25m`) parses
//!   back. A formatter whose output its own parser rejects is a trap for anyone scripting
//!   `gea times list --json time` into `gea times add`.

use gitea_core::error::{Error, ErrorKind, Result};

/// One second, in nanoseconds — the unit everything below is accumulated in, so that `1.5h` and
/// `500ms` are both exact before the final rounding.
const NANOS_PER_SEC: i128 = 1_000_000_000;

/// Parse a Go-style duration into whole seconds.
///
/// The API records integer seconds, so the result is rounded to nearest rather than truncated:
/// `90.6s` recorded as 90 loses time the user said they spent, and always-down accumulates.
pub fn parse_seconds(input: &str) -> Result<i64> {
    let nanos = parse_nanos(input)?;
    if nanos == 0 {
        return Err(bad(input, "a duration of zero would record nothing"));
    }
    let secs = (nanos + NANOS_PER_SEC / 2) / NANOS_PER_SEC;
    if secs == 0 {
        return Err(bad(
            input,
            "Gitea records tracked time in whole seconds, and this rounds to zero",
        ));
    }
    i64::try_from(secs).map_err(|_| bad(input, "that is longer than the API can represent"))
}

/// Parse a Go-style duration into nanoseconds.
///
/// Separate from [`parse_seconds`] so the unit tests can pin sub-second behaviour, and so a
/// future caller that wants sub-second precision does not have to re-derive the grammar.
pub fn parse_nanos(input: &str) -> Result<i128> {
    let text = input.trim();
    if text.is_empty() {
        return Err(bad(input, "it is empty"));
    }
    if text.starts_with('-') {
        return Err(bad(
            input,
            "tracked time cannot be negative; to remove an entry use `gea times delete`, or \
             `gea times reset` to clear an issue",
        ));
    }
    // `0` alone is legal in Go and means zero; keep that so a script that computes a duration
    // and gets nothing does not fail on the *syntax* before reaching the clearer zero error.
    if text.trim_start_matches('+') == "0" {
        return Ok(0);
    }

    let mut total: i128 = 0;
    let mut rest = text.trim_start_matches('+');
    let mut components = 0usize;

    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let digits = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
        if digits == 0 {
            return Err(bad(input, &format!("{:?} is not a number", first_word(rest))));
        }
        let number_text = &rest[..digits];
        let number: f64 = number_text
            .parse()
            .map_err(|_| bad(input, &format!("{number_text:?} is not a number")))?;
        rest = &rest[digits..];

        let (unit_len, nanos_per_unit) = unit(rest).ok_or_else(|| {
            if rest.is_empty() {
                bad(
                    input,
                    &format!(
                        "{number_text} has no unit; write {number_text}m for minutes or \
                         {number_text}s for seconds"
                    ),
                )
            } else {
                bad(
                    input,
                    &format!(
                        "{:?} is not a unit; use w, d, h, m, s, ms, us or ns",
                        first_word(rest)
                    ),
                )
            }
        })?;
        rest = &rest[unit_len..];

        // Multiply in f64 and round *per component*, so `1.5h` is exactly 5_400s rather than
        // 5_399.999…; the accumulator stays integral.
        total += (number * nanos_per_unit as f64).round() as i128;
        components += 1;
    }

    if components == 0 {
        return Err(bad(input, "it names no duration"));
    }
    Ok(total)
}

/// The nanoseconds one unit is worth, and how many bytes its name took.
///
/// Two-character units are tested before one-character ones: `ms` must not be read as `m`
/// followed by a stray `s`, which would turn 500 milliseconds into 500 minutes and 0 seconds.
fn unit(rest: &str) -> Option<(usize, i128)> {
    const MS: i128 = 1_000_000;
    const SEC: i128 = NANOS_PER_SEC;
    const MIN: i128 = 60 * SEC;
    const HOUR: i128 = 60 * MIN;
    const DAY: i128 = 24 * HOUR;
    const WEEK: i128 = 7 * DAY;
    for (name, nanos) in [("ms", MS), ("us", 1_000), ("µs", 1_000), ("μs", 1_000), ("ns", 1)] {
        if rest.starts_with(name) {
            return Some((name.len(), nanos));
        }
    }
    for (name, nanos) in [("w", WEEK), ("d", DAY), ("h", HOUR), ("m", MIN), ("s", SEC)] {
        if rest.starts_with(name) {
            return Some((name.len(), nanos));
        }
    }
    None
}

/// Render seconds the way [`parse_seconds`] accepts them: `1h 25m`, `3d 4h`, `45s`, `0s`.
///
/// Zero components are omitted, so a two-week total does not read `14d 0h 0m 0s`. Days and
/// weeks are used because a repository total of `1w 2d 3h` is legible and `219h` is not.
pub fn format(seconds: i64) -> String {
    if seconds == 0 {
        return "0s".to_owned();
    }
    let negative = seconds < 0;
    let mut rest = seconds.unsigned_abs();
    let mut parts: Vec<String> = Vec::new();
    for (unit, size) in [("w", 604_800u64), ("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        let n = rest / size;
        if n > 0 {
            parts.push(format!("{n}{unit}"));
            rest -= n * size;
        }
    }
    let body = parts.join(" ");
    // A negative total should be impossible — `parse_seconds` refuses one — but the API is the
    // source of truth for what is stored, and printing `-1h` is more honest than `1h`.
    if negative { format!("-{body}") } else { body }
}

/// The compact form, for a table column where every character costs width: `1h25m`.
pub fn format_compact(seconds: i64) -> String {
    format(seconds).replace(' ', "")
}

fn first_word(s: &str) -> &str {
    let end = s.find(|c: char| c.is_ascii_digit() || c.is_whitespace()).unwrap_or(s.len());
    if end == 0 { &s[..s.len().min(1)] } else { &s[..end] }
}

fn bad(input: &str, why: &str) -> Error {
    Error::new(ErrorKind::Usage(format!(
        "invalid duration {input:?}: {why}. Use units, such as 1h25m, 90m, 2h, 45s, 1.5h, or 3d. Spaces between parts are allowed."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapes_people_actually_type_all_parse() {
        assert_eq!(parse_seconds("1h25m").unwrap(), 5_100);
        assert_eq!(parse_seconds("90m").unwrap(), 5_400);
        assert_eq!(parse_seconds("2h").unwrap(), 7_200);
        assert_eq!(parse_seconds("45s").unwrap(), 45);
        assert_eq!(parse_seconds("1h25m30s").unwrap(), 5_130);
        assert_eq!(parse_seconds("1.5h").unwrap(), 5_400);
        assert_eq!(parse_seconds("3d").unwrap(), 259_200);
        assert_eq!(parse_seconds("1w").unwrap(), 604_800);
        // Whitespace and mixed case, because both arrive from shells and copy-paste.
        assert_eq!(parse_seconds(" 1h 25m ").unwrap(), 5_100);
    }

    /// Bug this prevents: reading `500ms` as 500 minutes. `m` is a legal unit and a prefix of
    /// `ms`, so a parser that tests one-character units first is off by a factor of 60_000.
    #[test]
    fn milliseconds_are_not_minutes() {
        assert_eq!(parse_nanos("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_nanos("2m").unwrap(), 120 * NANOS_PER_SEC);
        assert_eq!(parse_nanos("1500us").unwrap(), 1_500_000);
        assert_eq!(parse_nanos("1500µs").unwrap(), 1_500_000);
        assert_eq!(parse_nanos("42ns").unwrap(), 42);
    }

    /// Bug this prevents: guessing a unit for a bare number. The API's unit is seconds and a
    /// human's is minutes, so `gea times add 42 90` is ambiguous by a factor of 60 — and both
    /// readings are plausible enough that nobody would notice the wrong one for weeks.
    #[test]
    fn a_bare_number_is_refused_and_the_message_names_both_readings() {
        let e = parse_seconds("90").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        let msg = e.to_string();
        assert!(msg.contains("90m"), "{msg}");
        assert!(msg.contains("90s") || msg.contains("s for seconds"), "{msg}");
    }

    #[test]
    fn malformed_durations_are_rejected() {
        for input in ["", "  ", "h", "1x", "1h m", "abc", "1..5h", "m30"] {
            let e = parse_seconds(input).unwrap_err();
            assert_eq!(e.exit_code(), 2, "{input:?} should be a usage error");
        }
    }

    /// Bug this prevents: accepting `-1h` and silently *reducing* an issue's recorded time,
    /// which the API will happily do.
    #[test]
    fn a_negative_duration_is_refused_and_points_at_delete() {
        let e = parse_seconds("-1h").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("times delete"), "{msg}");
        assert!(msg.contains("times reset"), "{msg}");
    }

    #[test]
    fn zero_is_refused_because_it_would_record_nothing() {
        assert!(parse_seconds("0").is_err());
        assert!(parse_seconds("0s").is_err());
        // …but zero *nanoseconds* is a legal parse; only recording it is refused.
        assert_eq!(parse_nanos("0").unwrap(), 0);
    }

    #[test]
    fn sub_second_input_that_rounds_to_zero_is_refused_rather_than_recorded_as_nothing() {
        assert!(parse_seconds("100ms").is_err());
        // Rounding is to nearest, so half a second and above survives.
        assert_eq!(parse_seconds("600ms").unwrap(), 1);
    }

    #[test]
    fn formatting_omits_zero_components() {
        assert_eq!(format(0), "0s");
        assert_eq!(format(45), "45s");
        assert_eq!(format(5_100), "1h 25m");
        assert_eq!(format(3_600), "1h");
        assert_eq!(format(90_000), "1d 1h");
        assert_eq!(format(788_400), "1w 2d 3h");
        assert_eq!(format_compact(5_100), "1h25m");
    }

    /// Bug this prevents: a formatter whose output its own parser rejects. Anyone piping
    /// `gea times list` into `gea times add` would hit it, and so would every doc example.
    #[test]
    fn every_formatted_duration_parses_back_to_itself() {
        for secs in [1i64, 45, 60, 61, 3_599, 3_600, 5_100, 86_399, 86_400, 604_800, 788_400] {
            let text = format(secs);
            assert_eq!(parse_seconds(&text).unwrap(), secs, "{text:?} did not round-trip");
            let compact = format_compact(secs);
            assert_eq!(parse_seconds(&compact).unwrap(), secs, "{compact:?} did not round-trip");
        }
    }
}

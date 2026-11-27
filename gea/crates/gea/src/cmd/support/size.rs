//! Byte sizes, in and out.
//!
//! Sizes — package files, blob limits — are `int64` bytes on the wire and nobody types
//! `10737418240`. This module is the two directions of that: [`human`] for reading and [`parse`]
//! for writing.
//!
//! # `-1` is unlimited
//!
//! Gitea spells "no limit" as `-1`, and a rule with limit `-1` is the normal way to *exempt*
//! a group from a default. Rendering that as `-1 B` — or, worse, as `18.4 EiB` after an unsigned
//! cast — is the single most confusing thing this module could do, so [`human`] special-cases it
//! and [`parse`] accepts both `-1` and the word `unlimited`.
//!
//! # Binary units, labelled as such
//!
//! `1 KiB` is 1024 bytes and `1 kB` is 1000. Gitea's web UI shows binary units, so that is
//! what [`human`] prints — and it prints `KiB`, not `KB`, because a size labelled ambiguously is
//! how a 7% error creeps into a capacity plan. [`parse`] accepts either spelling and treats a
//! bare `K`/`M`/`G` as binary, which is what someone typing `--limit 10G` at a shell means.

use gitea_core::error::Result;

/// "no limit", as Gitea spells it.
pub const UNLIMITED: i64 = -1;

const KIB: f64 = 1024.0;

/// A byte count for a human: `1.5 GiB`, `912 B`, `unlimited`.
pub fn human(bytes: i64) -> String {
    if bytes == UNLIMITED {
        return "unlimited".to_owned();
    }
    // Any other negative is not something Gitea documents. Printing it rather than clamping
    // keeps the surprise visible instead of turning it into a plausible-looking number.
    if bytes < 0 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] {
        if value < KIB || unit == "PiB" {
            return if unit == "B" {
                format!("{bytes} B")
            } else if value < 10.0 {
                // One decimal below 10 so `1.5 GiB` and `9.8 GiB` keep their precision, and
                // none above it so a table column does not wobble between `12.0` and `12`.
                format!("{value:.1} {unit}")
            } else {
                format!("{:.0} {unit}", value)
            };
        }
        value /= KIB;
    }
    unreachable!("the loop returns on PiB")
}

/// A size a user typed: `10GiB`, `500 MB`, `1024`, `-1`, `unlimited`.
///
/// A bare number is bytes, matching the API. That is safe here in a way it is not for durations:
/// the API's unit *is* bytes and there is no plausible second reading.
pub fn parse(input: &str) -> Result<i64> {
    let text = input.trim();
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), "unlimited" | "none" | "-1" | "inf" | "infinite") {
        return Ok(UNLIMITED);
    }
    let digits =
        text.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-').unwrap_or(text.len());
    if digits == 0 {
        return Err(bad(input));
    }
    let number: f64 = text[..digits].parse().map_err(|_| bad(input))?;
    if number < 0.0 {
        return Err(bad(input));
    }
    let suffix = text[digits..].trim().to_ascii_lowercase();
    let multiplier: f64 = match suffix.as_str() {
        "" | "b" => 1.0,
        // `kb` is accepted as binary rather than decimal on purpose: someone typing `--limit
        // 10GB` at a shell means the same thing they mean by `10G`, and a 7% surprise in a quota
        // is worse than a pedantic unit.
        "k" | "kb" | "kib" => KIB,
        "m" | "mb" | "mib" => KIB * KIB,
        "g" | "gb" | "gib" => KIB * KIB * KIB,
        "t" | "tb" | "tib" => KIB * KIB * KIB * KIB,
        "p" | "pb" | "pib" => KIB * KIB * KIB * KIB * KIB,
        _ => return Err(bad(input)),
    };
    let bytes = (number * multiplier).round();
    if bytes > i64::MAX as f64 {
        return Err(bad(input));
    }
    Ok(bytes as i64)
}

fn bad(input: &str) -> gitea_core::Error {
    gitea_core::Error::new(gitea_core::ErrorKind::Usage(format!(
        "{input:?} is not a size; write bytes (1048576) or a unit (512MiB, 10G, 1.5TiB), or \
         `unlimited` / -1 for no limit"
    )))
}

/// `used / limit` as a percentage, or `None` when there is no limit to be a fraction of.
pub fn percent(used: i64, limit: i64) -> Option<f64> {
    if limit <= 0 {
        return None;
    }
    Some(used as f64 / limit as f64 * 100.0)
}

/// A short usage bar, for the terminal view: `[####------]  42%`.
pub fn bar(used: i64, limit: i64, width: usize) -> String {
    match percent(used, limit) {
        None => "unlimited".to_owned(),
        Some(pct) => {
            let filled = ((pct / 100.0) * width as f64).round().clamp(0.0, width as f64) as usize;
            format!(
                "[{}{}] {pct:>3.0}%",
                "#".repeat(filled),
                "-".repeat(width.saturating_sub(filled))
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents — the one the task singles out: rendering Gitea's `-1` as a size.
    /// Cast to unsigned it becomes 16 EiB, which looks like a real, generous quota; printed
    /// literally it is `-1 B`, which looks like a bug. Neither tells the truth.
    #[test]
    fn minus_one_is_unlimited_not_a_size() {
        assert_eq!(human(UNLIMITED), "unlimited");
        assert_eq!(parse("-1").unwrap(), UNLIMITED);
        assert_eq!(parse("unlimited").unwrap(), UNLIMITED);
        assert_eq!(parse("none").unwrap(), UNLIMITED);
        assert_eq!(percent(1_000, UNLIMITED), None);
        assert_eq!(bar(1_000, UNLIMITED, 10), "unlimited");
    }

    #[test]
    fn sizes_render_in_binary_units_labelled_as_binary() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(912), "912 B");
        assert_eq!(human(1_024), "1.0 KiB");
        assert_eq!(human(1_536), "1.5 KiB");
        assert_eq!(human(10 * 1_024), "10 KiB");
        assert_eq!(human(1_610_612_736), "1.5 GiB");
        // KiB rather than KB, because a mislabelled unit is a 2.4% error per level and nobody
        // notices until a capacity plan is 7% wrong.
        assert!(!human(1_024).contains("KB"));
    }

    #[test]
    fn units_are_accepted_in_every_spelling_someone_types() {
        assert_eq!(parse("1024").unwrap(), 1_024);
        assert_eq!(parse("1KiB").unwrap(), 1_024);
        assert_eq!(parse("1kb").unwrap(), 1_024);
        assert_eq!(parse("1K").unwrap(), 1_024);
        assert_eq!(parse("512MiB").unwrap(), 536_870_912);
        assert_eq!(parse("10G").unwrap(), 10_737_418_240);
        assert_eq!(parse("1.5TiB").unwrap(), 1_649_267_441_664);
        assert_eq!(parse(" 500 MB ").unwrap(), 524_288_000);
    }

    #[test]
    fn malformed_sizes_are_usage_errors_that_name_the_forms() {
        for input in ["", "big", "10 quatloos", "-5G", "1.2.3M"] {
            let e = parse(input).unwrap_err();
            assert_eq!(e.exit_code(), 2, "{input:?}");
            assert!(e.to_string().contains("unlimited"), "{input:?}: {e}");
        }
    }

    /// Bug this prevents: every formatted size failing to parse back, which would make
    /// `gea quota rules list --json limit` unusable as input to `gea quota rules edit`.
    #[test]
    fn round_numbers_round_trip_through_both_directions() {
        for bytes in [0i64, 1_024, 536_870_912, 10_737_418_240] {
            let text = human(bytes);
            assert_eq!(parse(&text).unwrap(), bytes, "{text:?}");
        }
    }

    #[test]
    fn the_bar_fills_in_proportion_and_never_overflows() {
        assert_eq!(bar(0, 100, 10), "[----------]   0%");
        assert_eq!(bar(50, 100, 10), "[#####-----]  50%");
        assert_eq!(bar(100, 100, 10), "[##########] 100%");
        // Over quota is the state someone runs this command *in*, so it must render rather than
        // panic on a filled count larger than the width.
        assert_eq!(bar(250, 100, 10), "[##########] 250%");
    }
}

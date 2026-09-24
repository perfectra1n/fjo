//! Timestamps, and the Go zero-time trap.
//!
//! Gitea is written in Go, and Go's `time.Time` zero value marshals to
//! `"0001-01-01T00:00:00Z"`. Gitea sends that for *unset* timestamps — `merged_at` on an
//! unmerged pull request, `closed_at` on an open issue, and so on.
//!
//! A naive `Option<Timestamp>` deserializer turns that into `Some(year 1)`, which then renders
//! as "2025 years ago" in a table. So every optional timestamp goes through
//! [`opt_timestamp`], which maps the zero time (and `null`, and `""`) to `None`.
//!
//! Unparseable timestamps also become `None` rather than a hard error: a malformed date in
//! one field of one row must not fail the whole command.

use std::fmt;

use jiff::Timestamp as JiffTimestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An instant in time, serialized as RFC 3339.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(JiffTimestamp);

impl Timestamp {
    pub const fn from_jiff(t: JiffTimestamp) -> Self {
        Self(t)
    }

    pub const fn as_jiff(self) -> JiffTimestamp {
        self.0
    }

    pub fn now() -> Self {
        Self(JiffTimestamp::now())
    }

    /// True for Go's zero time and the unix epoch, both of which Gitea uses to mean
    /// "unset". Callers that bypass [`opt_timestamp`] should check this.
    pub fn is_unset(self) -> bool {
        // jiff exposes the year via a civil datetime in UTC.
        self.0.to_zoned(jiff::tz::TimeZone::UTC).year() <= 1 || self.0.as_second() == 0
    }
}

impl Default for Timestamp {
    /// The unix epoch — a value [`Timestamp::is_unset`] already reports as unset, so a
    /// defaulted timestamp renders as "—" rather than as 1970.
    ///
    /// This exists for the generated models. Every generated struct derives `Default` so that
    /// `{}` deserializes, and one field in the specification is a *required* `date-time`
    /// (`EditDeadlineOption.due_date`), i.e. a bare `Timestamp` rather than an `Option`. Without
    /// `Default` here, that one field would make its struct — and therefore the container-level
    /// `#[serde(default)]` the whole forward-compatibility story rests on — impossible to derive.
    fn default() -> Self {
        Self(JiffTimestamp::UNIX_EPOCH)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<JiffTimestamp> for Timestamp {
    fn from(t: JiffTimestamp) -> Self {
        Self(t)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<JiffTimestamp>().map(Self).map_err(serde::de::Error::custom)
    }
}

/// The deserializer every optional timestamp field uses.
///
/// Maps to `None`: JSON `null`, the empty string, Go's zero time (any year <= 1), the unix
/// epoch, and anything unparseable. Never fails.
pub fn opt_timestamp<'de, D>(d: D) -> Result<Option<Timestamp>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    let Some(s) = raw else { return Ok(None) };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s.parse::<JiffTimestamp>() {
        Ok(t) => {
            let t = Timestamp(t);
            Ok(if t.is_unset() { None } else { Some(t) })
        }
        Err(_) => {
            crate::error::compat::note_unparsed("timestamp", s);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn de(json: &str) -> Option<Timestamp> {
        #[derive(Deserialize)]
        struct W {
            #[serde(default, deserialize_with = "opt_timestamp")]
            t: Option<Timestamp>,
        }
        serde_json::from_str::<W>(json).unwrap().t
    }

    #[test]
    fn go_zero_time_is_none() {
        // The whole reason this module exists.
        assert_eq!(de(r#"{"t":"0001-01-01T00:00:00Z"}"#), None);
    }

    #[test]
    fn null_empty_and_missing_are_none() {
        assert_eq!(de(r#"{"t":null}"#), None);
        assert_eq!(de(r#"{"t":""}"#), None);
        assert_eq!(de(r#"{}"#), None);
    }

    #[test]
    fn epoch_is_none() {
        assert_eq!(de(r#"{"t":"1970-01-01T00:00:00Z"}"#), None);
    }

    #[test]
    fn garbage_is_none_not_an_error() {
        assert_eq!(de(r#"{"t":"not a date"}"#), None);
    }

    #[test]
    fn real_timestamp_survives() {
        let t = de(r#"{"t":"2026-09-12T10:30:00Z"}"#).expect("should parse");
        assert!(!t.is_unset());
        assert_eq!(t.to_string(), "2026-09-12T10:30:00Z");
    }

    #[test]
    fn the_default_is_unset_rather_than_a_plausible_date() {
        // The generated models derive `Default`, so this value ends up in a struct built by
        // `Foo::default()`. If it were "now" or some arbitrary date, a caller who never set the
        // field would send a real deadline to the server.
        assert!(Timestamp::default().is_unset());
    }

    #[test]
    fn round_trips() {
        let t = de(r#"{"t":"2026-09-12T10:30:00Z"}"#).unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), r#""2026-09-12T10:30:00Z""#);
    }
}

//! Enums that tolerate values this build has never heard of.
//!
//! The spec we generate from is pinned to one Gitea release, but users point `gea` at
//! whatever their instance runs. If a newer server answers with an enum value the spec did
//! not list, a `#[derive(Deserialize)]` enum fails the whole request — the user sees a
//! deserialization error instead of their pull request list, over a field they may not even
//! have asked for.
//!
//! So every spec enum becomes an *open* enum with an `Unknown(String)` arm. Four properties
//! matter, and all four are load-bearing:
//!
//! 1. **It never hard-fails.** A future `"draft"` state becomes `Unknown("draft")`.
//! 2. **It round-trips verbatim.** Serializing `Unknown("draft")` yields `"draft"`, so a
//!    read-modify-write through `gea` cannot corrupt a value it did not understand. This is
//!    why `#[serde(other)]` is deliberately *not* used — that discards the original value.
//! 3. **It is observable.** Deserializing an unknown value records a note via
//!    [`gitea_core::error::compat`], which the binary drains into one grouped message at
//!    exit. Tolerant parsing must not mean silent parsing.
//! 4. **Adding a variant is not a breaking change.** `#[non_exhaustive]` forces downstream
//!    matches to carry a `_` arm, so a spec bump that adds a state does not break callers of
//!    this published crate.
//!
//! Note the `FromStr` impl has `Err = Infallible`: any string is a valid value. Layer-2 CLI
//! flags use `KNOWN` only to *suggest* values for completion, never to reject input, because
//! the server may well accept something our spec does not list.

/// Declares an open enum. See the module docs for why these are not plain derived enums.
///
/// ```ignore
/// open_enum! {
///     /// State of an issue or pull request.
///     pub enum StateType {
///         "open"   => Open,
///         "closed" => Closed,
///     }
///     default = Open;
/// }
/// ```
#[macro_export]
macro_rules! open_enum {
    (
        $(#[$emeta:meta])*
        pub enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $wire:literal => $variant:ident
            ),* $(,)?
        }
        default = $default:ident;
    ) => {
        $(#[$emeta])*
        ///
        /// This is an *open* enum: a value absent from the specification this build was
        /// generated from deserializes into `Unknown` and re-serializes unchanged.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum $name {
            $(
                $(#[$vmeta])*
                $variant,
            )*
            /// A value this build does not recognise. Round-trips verbatim.
            Unknown(::std::string::String),
        }

        impl $name {
            /// Every value the specification listed. Used for shell completion and for
            /// "did you mean" suggestions — never for validation.
            pub const KNOWN: &'static [&'static str] = &[$($wire),*];

            /// The wire representation. For `Unknown`, the original string.
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)*
                    Self::Unknown(s) => s.as_str(),
                }
            }

            /// Whether this value came from the specification rather than an unknown server.
            pub fn is_known(&self) -> bool {
                !matches!(self, Self::Unknown(_))
            }
        }

        impl ::std::default::Default for $name {
            fn default() -> Self {
                Self::$default
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            /// Infallible: every string maps to a value, unknown ones to `Unknown`.
            type Err = ::std::convert::Infallible;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                ::std::result::Result::Ok(match s {
                    $($wire => Self::$variant,)*
                    other => Self::Unknown(other.to_owned()),
                })
            }
        }

        impl ::std::convert::From<&str> for $name {
            fn from(s: &str) -> Self {
                match s {
                    $($wire => Self::$variant,)*
                    other => Self::Unknown(other.to_owned()),
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                s: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                d: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let s = <::std::string::String as ::serde::Deserialize>::deserialize(d)?;
                let v = Self::from(s.as_str());
                if let Self::Unknown(ref u) = v {
                    // Tolerant, but not silent: the binary reports these once at exit.
                    ::gitea_core::error::compat::note_unknown_enum(
                        ::std::stringify!($name),
                        u,
                    );
                }
                ::std::result::Result::Ok(v)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    crate::open_enum! {
        /// State of an issue or pull request.
        pub enum StateType {
            "open"   => Open,
            "closed" => Closed,
        }
        default = Open;
    }

    #[test]
    fn known_values_deserialize_to_variants() {
        assert_eq!(serde_json::from_str::<StateType>(r#""open""#).unwrap(), StateType::Open);
        assert_eq!(serde_json::from_str::<StateType>(r#""closed""#).unwrap(), StateType::Closed);
    }

    #[test]
    fn a_newer_server_does_not_break_us() {
        // The bug this whole module prevents: a Gitea release adding a state must not turn
        // `gea pr list` into a deserialization error.
        let v: StateType = serde_json::from_str(r#""draft""#).unwrap();
        assert_eq!(v, StateType::Unknown("draft".into()));
        assert!(!v.is_known());
    }

    #[test]
    fn unknown_round_trips_verbatim() {
        // Read-modify-write must not corrupt a value we did not understand.
        let v: StateType = serde_json::from_str(r#""draft""#).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), r#""draft""#);
    }

    #[test]
    fn unknown_is_reported_to_the_compat_collector() {
        gitea_core::error::compat::drain(); // clear anything from other tests
        let _: StateType = serde_json::from_str(r#""quantum""#).unwrap();
        let notes = gitea_core::error::compat::drain();
        assert!(
            notes.iter().any(|n| matches!(
                n,
                gitea_core::error::compat::Note::UnknownEnum { type_name: "StateType", value }
                    if value == "quantum"
            )),
            "unknown value should be recorded, got {notes:?}"
        );
    }

    #[test]
    fn display_and_fromstr_agree() {
        for s in ["open", "closed", "something-new"] {
            assert_eq!(StateType::from_str(s).unwrap().to_string(), s);
        }
    }

    #[test]
    fn known_list_is_the_spec_values() {
        assert_eq!(StateType::KNOWN, &["open", "closed"]);
    }

    #[test]
    fn default_is_the_declared_variant() {
        assert_eq!(StateType::default(), StateType::Open);
    }
}

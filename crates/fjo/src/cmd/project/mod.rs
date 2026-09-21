//! `fjo project` — Forgejo's kanban boards.
//!
//! These are layer 0 commands: Projects has no REST API, so every one of them speaks to
//! Forgejo's web routes with a session cookie. See `docs/layers.md`.
//!
//! What this adds over `fjo web`, which can reach the same routes: **everything is named by
//! title.** Forgejo's project routes are all `/projects/{id}/{columnID}`, and nobody knows that
//! a board called `Roadmap` is id 3 or that its `Done` column is id 12. The same reasoning put
//! `milestone_by_title` behind `fjo milestone`, and the same corollary applies — an ambiguous
//! title is refused with the matches listed, never guessed, because Forgejo is perfectly happy
//! to hold two boards called `Roadmap`.

pub mod parse;

pub use parse::{Board, Card, Column, parse_board};

//! Shared domain types used by both the hand-written runtime and the generated code.

pub mod ids;
pub mod scope;
pub mod slug;
pub mod timestamp;

pub use scope::{Access, Scope};
pub use slug::{RepoRef, RepoSlug};
pub use timestamp::{Timestamp, opt_timestamp};

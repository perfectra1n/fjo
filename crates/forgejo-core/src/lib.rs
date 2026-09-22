//! Runtime for Forgejo API clients.
//!
//! This crate is hand-written and never generated. It supplies everything the generated
//! client cannot express from the spec alone: authentication, pagination (the spec declares
//! zero response headers, yet the API paginates via `Link` and `x-total-count`), retry
//! policy, error classification, and the configuration and git context a CLI needs.
#![forbid(unsafe_code)]

pub mod capabilities;
pub mod config;
pub mod context;
pub mod error;
pub mod http;
pub mod oauth;
pub mod types;
pub mod web;

pub use error::{Error, ErrorKind, Result};

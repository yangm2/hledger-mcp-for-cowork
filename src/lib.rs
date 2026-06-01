//! hledger MCP server for Claude Cowork — library crate.
//!
//! M0 (walking skeleton): a stdio MCP server with two synthetic tools plus the
//! logging/observability plumbing. The hledger backend arrives in M1+. See
//! `docs/development/milestones/`.

#![forbid(unsafe_code)]

pub mod logging;
pub mod protocol;
pub mod server;
pub mod tools;

pub use server::HledgerMcp;

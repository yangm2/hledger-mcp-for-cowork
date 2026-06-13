//! hledger MCP server for Claude Cowork — library crate.
//!
//! M0 (walking skeleton): a stdio MCP server with two synthetic tools plus the
//! logging/observability plumbing. The hledger backend arrives in M1+. See
//! `docs/development/milestones/`.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod domain;
pub mod epoch;
pub mod flags;
pub mod git;
pub mod hledger;
pub mod logging;
pub mod protocol;
pub mod resources;
pub mod server;
pub mod tools;
pub mod write;

pub use server::HledgerMcp;

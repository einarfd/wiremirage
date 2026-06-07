//! Shared types and the typed REST client that talks to a `wm-host`.
//!
//! `wm-cli` uses this client to drive the host; future `wm-mcp` and any
//! other Rust-side tooling are expected to use the same surface so the
//! User-Agent header consistently labels traffic by client.
//!
//! The client is async (built on `reqwest`) — call sites in synchronous
//! contexts (like a typical CLI binary) bring up a small tokio
//! runtime via `#[tokio::main]` or `Runtime::new`.

pub mod client;
pub mod models;
pub mod spec;

pub use client::{Client, ClientBuilder, ClientError};
pub use models::*;

//! WireMirage host runtime.
//!
//! Surface: WIT bindings, the `Bucket` / `Storage` abstraction (in-memory
//! today, Valkey landing in slice 2), the `HostState` that wires them into
//! wasmtime, the slice-1 hardcoded-component axum server.

pub mod api;
pub mod api_filters;
pub mod auth;
pub mod auth_api;
pub mod bindings;
pub mod callout;
pub mod capabilities;
pub mod dry_run;
pub mod egress;
pub mod github_oauth;
pub mod host_state;
pub mod journal;
pub mod journal_filter;
pub mod lifecycle;
pub mod local_auth;
pub mod log;
pub mod login_throttle;
pub mod mcp;
pub mod mcp_oauth;
pub mod metrics;
pub mod naming;
pub mod pattern;
pub mod registry;
pub mod route_table;
pub mod runtime;
pub mod server;
pub mod session;
pub mod store;
pub mod telemetry;
pub mod ts_transpile;
pub mod ui;
pub mod wire;

/// Handler bindings version this build of the host accepts. Pre-compiled
/// wasm uploads must declare this exact value; mismatches are rejected.
/// Bumped per the protocol in `script-api-wit.md`'s "Stability policy"
/// when the WIT contract changes shape.
pub const SUPPORTED_BINDINGS_VERSION: &str = "0.1.0";

pub use bindings::Handler;
pub use bindings::wiremirage::handler::http::{Header, PathParam, Request, Response};
pub use host_state::HostState;
pub use log::{LogCapture, LogLevel, LogRecord};
pub use runtime::{BucketHandles, Runtime};
pub use server::{AppState, router};
pub use store::{Bucket, Storage, StoreError};

//! WireMirage host runtime.
//!
//! Surface: WIT bindings, the `Bucket` / `Storage` abstraction (in-memory
//! today, Valkey landing in slice 2), the `HostState` that wires them into
//! wasmtime, the slice-1 hardcoded-component axum server.

pub mod api;
pub mod api_filters;
pub mod auth;
pub mod bindings;
pub mod compiler;
pub mod dry_run;
pub mod host_state;
pub mod journal;
pub mod journal_filter;
pub mod lifecycle;
pub mod log;
pub mod mcp;
pub mod pattern;
pub mod registry;
pub mod route_table;
pub mod runtime;
pub mod server;
pub mod store;
pub mod telemetry;

/// Handler bindings version this build of the host accepts. Compilers
/// (sidecar) and pre-compiled-component uploads must declare this exact
/// value; mismatches are rejected. Bumped per the protocol in
/// `script-api-wit.md`'s "Stability policy" when the WIT contract changes
/// shape.
pub const SUPPORTED_BINDINGS_VERSION: &str = "0.1.0";

pub use bindings::Handler;
pub use bindings::wiremirage::handler::http::{Header, PathParam, Request, Response};
pub use host_state::HostState;
pub use log::{LogCapture, LogLevel, LogRecord};
pub use runtime::{BucketHandles, Runtime};
pub use server::{AppState, router};
pub use store::{Bucket, Storage, StoreError};

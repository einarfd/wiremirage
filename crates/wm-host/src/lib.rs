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
pub mod bus;
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
pub mod oidc;
pub mod pattern;
pub mod registry;
pub mod route_table;
pub mod runtime;
pub mod server;
pub mod session;
pub mod store;
pub mod telemetry;
pub mod ui;
pub mod wire;

/// Handler bindings version this build of the host accepts. Pre-compiled
/// wasm uploads must declare this exact value; mismatches are rejected.
/// Bumped per the protocol in `script-api-wit.md`'s "Stability policy"
/// when the WIT contract changes shape.
pub const SUPPORTED_BINDINGS_VERSION: &str = "0.1.0";

/// This build's version, from the crate manifest.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// This build's short git commit, when the build knew one — `build.rs`
/// takes it from `WM_BUILD_SHA` (how the container build receives it) or
/// from `git rev-parse` locally, and stamps nothing when neither is
/// available.
///
/// The version alone cannot identify a build: every commit between two
/// releases reports the version of the last bump, so two different images
/// both say `0.1.2`. This is what maps a running host to a deployed
/// artifact, since `sha-<short>` is the image tag CI publishes. It is
/// always a commit, never a tag — the host image is retagged by digest
/// rather than rebuilt on release, so no tag exists when it is compiled.
pub const BUILD_SHA: Option<&str> = option_env!("WM_BUILD_SHA");

/// `0.1.2 (a73e574)`, or just `0.1.2` for an unstamped build.
pub fn build_id() -> String {
    match BUILD_SHA {
        Some(sha) => format!("{HOST_VERSION} ({sha})"),
        None => HOST_VERSION.to_string(),
    }
}

pub use bindings::Handler;
pub use bindings::wiremirage::handler::http::{Header, PathParam, Request, Response};
pub use host_state::HostState;
pub use log::{LogCapture, LogLevel, LogRecord};
pub use runtime::{BucketHandles, Runtime};
pub use server::{AppState, router};
pub use store::{Bucket, Storage, StoreError};

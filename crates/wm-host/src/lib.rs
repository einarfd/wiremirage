//! WireMirage host runtime.
//!
//! Surface: WIT bindings, the `Bucket` / `Storage` abstraction (in-memory
//! today, Valkey landing in slice 2), the `HostState` that wires them into
//! wasmtime, the slice-1 hardcoded-component axum server.

pub mod bindings;
pub mod host_state;
pub mod log;
pub mod runtime;
pub mod server;
pub mod store;

pub use bindings::Handler;
pub use bindings::wiremirage::handler::http::{Header, PathParam, Request, Response};
pub use host_state::HostState;
pub use log::{LogCapture, LogLevel, LogRecord};
pub use runtime::{BucketHandles, Runtime};
pub use server::{AppState, router};
pub use store::{Bucket, Storage, StoreError};

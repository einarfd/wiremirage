//! WireMirage host runtime.
//!
//! Slice 1 surface: WIT bindings, an in-memory `MemBucket` backing for the
//! `store.bucket` resource, and a `HostState` that wires both into wasmtime.
//! The axum server, route table, and Valkey-backed storage arrive in
//! subsequent slices.

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
pub use store::{MemBucket, StoreError};

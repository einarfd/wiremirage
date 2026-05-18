use std::path::Path;
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Result, Store};

use crate::bindings::Handler;
use crate::host_state::{HandlerLimits, HostState};
use crate::store::{Bucket, Storage};

/// Per-call fuel budget (~1 instruction per unit). Calibrated for
/// componentize-js handlers, where SpiderMonkey embed startup alone
/// burns tens of millions of units before the handler runs. 10 B
/// fuel ≈ 1-2 s of pure CPU on aarch64 — generous for legitimate
/// mocks, strict enough that a `while(true)` traps with `out of
/// fuel` in bounded wall-clock time. Combined with the epoch
/// deadline below, whichever limit fires first wins.
pub const HANDLER_FUEL: u64 = 10_000_000_000;

/// Maximum linear-memory bytes per handler instance. componentize-js
/// components carry a full SpiderMonkey + steady-state ~16-32 MiB;
/// 64 MiB caps the abusable headroom without strangling legitimate
/// JSON-heavy handlers. wasm32 is 32-bit linear memory so the
/// architectural ceiling is 4 GiB regardless — this picks a
/// meaningful floor.
pub const HANDLER_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Wall-clock deadline expressed in epoch ticks. With
/// `EPOCH_TICK_INTERVAL_MS = 10`, 100 ticks ≈ 1s. The deadline is
/// a backstop for handlers that consume fuel slowly (heavy I/O via
/// host imports doesn't burn fuel) or use SIMD operations that have
/// a low fuel cost relative to their wall time.
pub const HANDLER_EPOCH_TICKS: u64 = 100;

/// Period between `Engine::increment_epoch()` calls from the
/// background ticker. 10 ms is the standard cadence for
/// epoch-interruption-based deadlines.
pub const EPOCH_TICK_INTERVAL_MS: u64 = 10;

/// Wraps a wasmtime `Engine`, a `Linker` configured with all WireMirage
/// host imports, and the `Storage` backend that mints per-request buckets.
/// One `Runtime` is shared across requests; per-request state lives in
/// `HostState` instances created via `instantiate`.
pub struct Runtime {
    engine: Engine,
    linker: Linker<HostState>,
    storage: Storage,
    /// Per-call fuel budget applied in `instantiate_with_buckets`.
    /// Configurable via the test-only `with_handler_fuel` constructor;
    /// production code uses [`HANDLER_FUEL`].
    fuel: u64,
    /// Per-call epoch-tick deadline. Same story as `fuel` — defaults
    /// to [`HANDLER_EPOCH_TICKS`], overridable in tests.
    epoch_ticks: u64,
    /// Per-instance linear-memory cap. Defaults to
    /// [`HANDLER_MAX_MEMORY_BYTES`].
    max_memory_bytes: usize,
}

impl Runtime {
    pub fn new(storage: Storage) -> Result<Self> {
        Self::with_limits(
            storage,
            HANDLER_FUEL,
            HANDLER_EPOCH_TICKS,
            HANDLER_MAX_MEMORY_BYTES,
        )
    }

    /// Test-only constructor that lets the caller override the
    /// per-call limits. Production code uses [`Runtime::new`].
    /// Public because the host's integration-test crate is a
    /// separate target.
    pub fn with_limits(
        storage: Storage,
        fuel: u64,
        epoch_ticks: u64,
        max_memory_bytes: usize,
    ) -> Result<Self> {
        let mut config = Config::new();
        // Both flags must be on at engine-construction time —
        // toggling them on a live engine isn't supported. The matching
        // `Store::set_fuel` / `Store::set_epoch_deadline` calls in
        // `instantiate_with_buckets` make them effective per call.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let mut linker: Linker<HostState> = Linker::new(&engine);
        Handler::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |s| s)?;
        Ok(Self {
            engine,
            linker,
            storage,
            fuel,
            epoch_ticks,
            max_memory_bytes,
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn handler_fuel(&self) -> u64 {
        self.fuel
    }

    /// Spawn a background tokio task that advances the engine's
    /// epoch on a fixed cadence. Required when
    /// `epoch_interruption(true)` is set on the engine config —
    /// without it, `Store::set_epoch_deadline` is configured but
    /// the deadline never fires. Returns the spawn handle; production
    /// code drops it (the task lives for the engine's lifetime).
    /// Tests can hold it if they want to verify shutdown behavior.
    pub fn spawn_epoch_ticker(&self) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        let interval = Duration::from_millis(EPOCH_TICK_INTERVAL_MS);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                engine.increment_epoch();
            }
        })
    }

    /// Compile a component from a file (typically a `.component.wasm`
    /// produced by `wasm-tools component new`).
    pub fn load_component(&self, path: &Path) -> Result<Component> {
        Component::from_file(&self.engine, path)
    }

    /// Instantiate the component with fresh buckets for the given
    /// `(group, route)` scope. Returns the `Handler`, the wasmtime `Store`
    /// (which owns the state and resource handles), and the bucket
    /// resources to pass to `call_handle`.
    pub fn instantiate(
        &self,
        component: &Component,
        group_ulid: &str,
        route_ulid: &str,
    ) -> Result<(Handler, Store<HostState>, BucketHandles)> {
        let route_bucket = self
            .storage
            .route_bucket(group_ulid, route_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open route bucket: {e}")))?;
        let group_bucket = self
            .storage
            .group_bucket(group_ulid)
            .map_err(|e| wasmtime::Error::msg(format!("open group bucket: {e}")))?;
        self.instantiate_with_buckets(component, route_bucket, group_bucket)
    }

    /// Lower-level entry point used by tests that want to construct buckets
    /// explicitly (e.g., to seed state before instantiation).
    pub fn instantiate_with_buckets(
        &self,
        component: &Component,
        route_bucket: Bucket,
        group_bucket: Bucket,
    ) -> Result<(Handler, Store<HostState>, BucketHandles)> {
        let mut store = Store::new(
            &self.engine,
            HostState::new(HandlerLimits::new(self.max_memory_bytes)),
        );
        // Fuel + epoch deadline + memory limiter must all be set
        // before the handler runs. Skipping any of them silently
        // drops back to "no limit" behavior — that's why these live
        // here rather than at the call site.
        store.set_fuel(self.fuel)?;
        store.set_epoch_deadline(self.epoch_ticks);
        store.limiter(|state| &mut state.limits);

        let route = store.data_mut().push_bucket(route_bucket)?;
        let group = store.data_mut().push_bucket(group_bucket)?;
        let handler = Handler::instantiate(&mut store, component, &self.linker)?;
        Ok((handler, store, BucketHandles { route, group }))
    }
}

/// Resource handles returned alongside an instantiated `Handler` so the
/// caller can pass them as the `route-store` / `group-store` arguments to
/// `call_handle`.
pub struct BucketHandles {
    pub route: wasmtime::component::Resource<Bucket>,
    pub group: wasmtime::component::Resource<Bucket>,
}

//! Tier-2 integration test: load the echo-handler fixture component, call
//! `handle` directly through the wasmtime bindings, verify the response.
//!
//! This test exercises the full WIT round-trip (request struct in, response
//! struct out, plus the `borrow<bucket>` resource arguments) without going
//! through axum. The HTTP-level test lives elsewhere and runs on top of the
//! axum server once that's wired up.

use std::path::PathBuf;

use wm_host::bindings::wiremirage::handler::http::Request;
use wm_host::{Runtime, Storage};

const ECHO_COMPONENT_PATH: &str = env!("WM_FIXTURE_ECHO_HANDLER_COMPONENT");

#[test]
fn echo_handler_round_trip() {
    let runtime = Runtime::new(Storage::in_memory()).expect("runtime");
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");

    let (handler, mut store, handles) = runtime
        .instantiate(&component, "test-group", "test-route")
        .expect("instantiate");

    let req = Request {
        method: "POST".into(),
        path: "/v1/charges".into(),
        matched_pattern: "/v1/charges".into(),
        path_params: vec![],
        query: vec![],
        headers: vec![("content-type".into(), "application/json".into())],
        body: br#"{"amount":1000}"#.to_vec(),
    };

    let response = handler
        .call_handle(&mut store, &req, handles.route, handles.group)
        .expect("call handle");

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"echo: POST /v1/charges");
    assert!(
        response
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "text/plain"),
        "expected content-type: text/plain header in {:?}",
        response.headers
    );
}

/// Slice 46 / F-1: handlers run with a per-call fuel budget so a
/// runaway loop doesn't hang a worker thread forever. We can't
/// build a deliberately-looping fixture cheaply, but we can prove
/// the wiring works by starving the *existing* fixture: with fuel
/// set absurdly low, even legitimate startup-into-handle work
/// blows past it and the call traps with `all fuel consumed`.
#[test]
fn handler_traps_when_fuel_budget_is_exhausted() {
    let runtime = Runtime::with_limits(
        Storage::in_memory(),
        // 1 unit of fuel — not enough to do any meaningful work.
        // The echo handler's first instruction will trigger the
        // trap. Real production fuel is ~10 billion.
        1,
        100,
        64 * 1024 * 1024,
    )
    .expect("runtime");
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");

    let (handler, mut store, handles) = runtime
        .instantiate(&component, "test-group", "test-route")
        .expect("instantiate");

    let req = Request {
        method: "POST".into(),
        path: "/v1/charges".into(),
        matched_pattern: "/v1/charges".into(),
        path_params: vec![],
        query: vec![],
        headers: vec![],
        body: vec![],
    };

    let err = handler
        .call_handle(&mut store, &req, handles.route, handles.group)
        .expect_err("starved fuel must trap");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("fuel") || msg.contains("trap"),
        "trap message names the fuel exhaustion: {msg}",
    );
}

/// Memory limit applies on `memory_growing` calls; the host denies
/// any grow that would push past `max_memory_bytes`. The echo
/// fixture is tiny but componentize-js-shaped handlers (and even
/// our Rust echo, on first instantiation) need at least one page
/// of linear memory. Setting the cap to a few bytes denies the
/// very first grow, so instantiation itself fails.
#[test]
fn handler_traps_when_memory_cap_denies_grow() {
    let runtime = Runtime::with_limits(
        Storage::in_memory(),
        10_000_000_000,
        100,
        // 1 byte — less than a single wasm page (64 KiB). The first
        // memory_growing callback returns false and instantiation
        // surfaces a trap.
        1,
    )
    .expect("runtime");
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");

    let outcome = runtime.instantiate(&component, "test-group", "test-route");
    let err = match outcome {
        Ok(_) => panic!("memory-starved instantiation must fail"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("memory") || msg.contains("limit") || msg.contains("trap"),
        "instantiation error names the memory cap: {msg}",
    );
}

/// Sanity check on the success path: with production-shaped limits,
/// a successful call leaves the journal-bound resource accounting
/// in a coherent state — fuel was consumed (non-zero), memory peak
/// is non-zero, and both are under the configured caps.
#[test]
fn successful_handler_call_records_resource_usage() {
    let runtime = Runtime::new(Storage::in_memory()).expect("runtime");
    let component = runtime
        .load_component(&PathBuf::from(ECHO_COMPONENT_PATH))
        .expect("load component");

    let fuel_budget = runtime.handler_fuel();
    let (handler, mut store, handles) = runtime
        .instantiate(&component, "test-group", "test-route")
        .expect("instantiate");

    let req = Request {
        method: "POST".into(),
        path: "/v1/charges".into(),
        matched_pattern: "/v1/charges".into(),
        path_params: vec![],
        query: vec![],
        headers: vec![],
        body: vec![],
    };
    handler
        .call_handle(&mut store, &req, handles.route, handles.group)
        .expect("call ok");

    // Resource accounting that the server.rs / dry_run.rs paths
    // forward to journal entries. Mirrors what those callers do.
    let remaining = store.get_fuel().expect("fuel available");
    let consumed = fuel_budget - remaining;
    let peak_memory = store.data().limits.peak_memory_bytes;

    assert!(
        consumed > 0,
        "successful call should burn *some* fuel, got {consumed}",
    );
    assert!(
        peak_memory > 0,
        "successful call should grow linear memory at least once",
    );
    assert!(
        peak_memory < 64 * 1024 * 1024,
        "echo handler shouldn't approach the cap: peak={peak_memory}",
    );
}

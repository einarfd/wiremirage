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

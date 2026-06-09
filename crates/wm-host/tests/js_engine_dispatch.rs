//! Tier-2 dispatch test for slice 57 (shared-engine route dispatch).
//!
//! Boots a real wm-host with the vendored js-engine.wasm wired in,
//! registers a `language: "javascript"` route directly via the
//! registry (no compile path involved — the source-language route
//! stores source verbatim), and hits the mock-traffic path with a
//! real HTTP request. Asserts the response came back through the
//! shared engine.

use std::path::PathBuf;
use std::sync::Arc;

use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::{NewRoute, Registry};
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

fn vendored_engine_path() -> Option<PathBuf> {
    // build.rs sets WM_JS_ENGINE_WASM to the OUT_DIR path of the
    // engine wasm. Pre-slice-59 this used to point at
    // crates/wm-host/vendored/, which is gone — the engine is now
    // built at cargo build time per ADR-0020 slice C.
    let p = PathBuf::from(env!("WM_JS_ENGINE_WASM"));
    if p.exists() { Some(p) } else { None }
}

async fn start_with_js_route(
    source: &str,
) -> Option<(std::net::SocketAddr, String, tokio::task::JoinHandle<()>)> {
    let engine_path = vendored_engine_path()?;
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Runtime::new(storage.clone())
        .expect("runtime")
        .with_js_engine(&engine_path)
        .expect("attach engine");
    let runtime = Arc::new(runtime);
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/echo".into(),
            language: "javascript".into(),
            bindings_version: "0.1.0".into(),
            // Engine routes have no per-route component; the
            // compiled_wasm field stays empty.
            compiled_wasm: Vec::new(),
            source: Some(source.to_string()),
            owner_id: "test-owner".into(),
        })
        .expect("create route");
    // The route landed in an auto-named implicit group; the mock-traffic
    // virtual host (ADR-0030) is `{group}.localhost`, so hand the group
    // name back to the caller.
    let group = route.group_name.clone();
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes, auth, journal));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    Some((addr, group, server))
}

/// As `start_with_js_route`, but with a custom engine epoch budget and
/// streaming max, and with the epoch ticker running so deadlines
/// actually fire (the default harness doesn't run it). Lets the
/// streaming-budget behaviour be exercised in well under a second
/// instead of waiting out the real ~30 s engine epoch.
async fn start_with_js_route_limited(
    source: &str,
    epoch_ticks: u64,
    stream_max: std::time::Duration,
) -> Option<(std::net::SocketAddr, String, tokio::task::JoinHandle<()>)> {
    let engine_path = vendored_engine_path()?;
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", "wmt_test")
        .expect("bootstrap");
    let runtime = Runtime::new(storage.clone())
        .expect("runtime")
        .with_js_engine(&engine_path)
        .expect("attach engine")
        .with_engine_stream_limits(epoch_ticks, stream_max);
    let runtime = Arc::new(runtime);
    // Detached: dropping the handle doesn't stop the ticker.
    runtime.spawn_epoch_ticker();
    let registry = Arc::new(Registry::new(storage.clone()));
    let route = registry
        .create_route(NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/echo".into(),
            language: "javascript".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: Vec::new(),
            source: Some(source.to_string()),
            owner_id: "test-owner".into(),
        })
        .expect("create route");
    let group = route.group_name.clone();
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    routes.refresh_after_create(route);
    let journal = Journal::new(storage);
    let app = router(AppState::new(runtime, routes, auth, journal));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("axum::serve");
    });
    Some((addr, group, server))
}

#[tokio::test]
async fn javascript_route_dispatches_through_shared_engine() {
    let source = r#"
        function handle(req, route, group) {
          const body = new TextEncoder().encode(
            "engine: " + req.method + " " + req.path
          );
          return {
            status: 200,
            headers: [["content-type", "text/plain; charset=utf-8"]],
            body,
          };
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");
    assert_eq!(body, "engine: POST /v1/echo");
    server.abort();
}

#[tokio::test]
async fn javascript_route_can_persist_kv_state_via_shared_engine() {
    // Round-trip route.kv via the engine. Proves the bucket
    // resource is plumbed through correctly: the engine's host
    // import for store still reaches our backing Storage.
    let source = r#"
        function handle(req, route, group) {
          const n = route.incr("hits", 1n);
          const body = new TextEncoder().encode("hits=" + n.toString());
          return { status: 200, headers: [], body };
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        return;
    };

    let client = reqwest::Client::new();
    for expected in 1..=3 {
        let resp = client
            .post(format!("http://{addr}/v1/echo"))
            .header(reqwest::header::HOST, format!("{group}.localhost"))
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.expect("body");
        assert_eq!(body, format!("hits={expected}"));
    }
    server.abort();
}

#[tokio::test]
async fn javascript_route_handler_error_surfaces_as_500_with_message() {
    let source = r#"
        function handle(req, route, group) {
          throw new Error("intentional");
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("intentional"),
        "error body surfaces the message: {body}"
    );
    server.abort();
}

// -- ADR-0021: clock primitives ---------------------------------------------

#[tokio::test]
async fn javascript_route_can_use_clock_primitives() {
    // Single handler exercises all three clock imports:
    //   - host.sleep blocks for ~250ms; the test measures elapsed
    //     time below to confirm the host actually slept.
    //   - host.monotonicMs is called before and after the sleep;
    //     the second value must be >= the first + 250 (allow for a
    //     little jitter — we assert >= 200ms of delta).
    //   - host.wallTimeMs returns a value past Jan 2023
    //     (1672531200000ms = 2023-01-01) — sanity that the host is
    //     plumbing real time.
    //
    // The handler returns the values as JSON; the test parses them
    // and asserts on each piece.
    let source = r#"
        function handle(req, route, group) {
          const mono1 = host.monotonicMs();
          host.sleep(250);
          const mono2 = host.monotonicMs();
          const wall = host.wallTimeMs();
          const body = new TextEncoder().encode(JSON.stringify({
            mono1: mono1,
            mono2: mono2,
            mono_delta: mono2 - mono1,
            wall: wall,
          }));
          return {
            status: 200,
            headers: [["content-type", "application/json"]],
            body,
          };
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        return;
    };

    let started = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("send");
    let elapsed = started.elapsed();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.expect("json");

    let mono_delta = body["mono_delta"].as_u64().expect("mono_delta u64");
    let wall = body["wall"].as_u64().expect("wall u64");

    // monotonic-ms increased by at least ~200ms across the sleep.
    // Allowing for jitter on a busy test runner.
    assert!(
        mono_delta >= 200,
        "monotonic delta across host.sleep(250) was {mono_delta}ms (expected >= 200)"
    );
    // The actual request elapsed time should be >= the slept time
    // too (it's the host clock measuring the same span).
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "request took {elapsed:?} which is less than the requested sleep"
    );
    // Wall time is plausible — past 2023-01-01.
    assert!(
        wall > 1_672_531_200_000,
        "wall_time_ms returned {wall}, which is before 2023-01-01"
    );

    server.abort();
}

#[tokio::test]
async fn javascript_route_monotonic_ms_is_non_decreasing_across_requests() {
    // Two requests against the same route. Each request returns
    // host.monotonicMs(); the second request's value must be >= the
    // first's. Catches any regression where monotonic accidentally
    // resets per-request or per-instance.
    let source = r#"
        function handle(req, route, group) {
          const v = host.monotonicMs();
          return {
            status: 200,
            headers: [["content-type", "text/plain"]],
            body: new TextEncoder().encode(String(v)),
          };
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        return;
    };

    let client = reqwest::Client::new();
    let first: u64 = client
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("send 1")
        .text()
        .await
        .expect("body 1")
        .parse()
        .expect("u64 1");
    // Tiny tokio sleep just to ensure wall-time has actually moved
    // forward between requests — without it the two monotonic
    // readings could be sub-ms-apart and equal (which is still
    // valid for "non-decreasing", but makes the assertion weaker).
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let second: u64 = client
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .send()
        .await
        .expect("send 2")
        .text()
        .await
        .expect("body 2")
        .parse()
        .expect("u64 2");

    assert!(
        second >= first,
        "monotonic should be non-decreasing across requests; got {first} then {second}"
    );
    server.abort();
}

// -- ADR-0022: streaming responses (prototype) ------------------------------

#[tokio::test]
async fn javascript_route_can_stream_response_via_host_response_stream() {
    // ADR-0022 prototype: a handler emits its body through
    // `host.responseStream` (the response-stream.start / write-chunk /
    // finish host imports) instead of returning a buffered response.
    // The host currently collects the chunks and flushes them as one
    // response (incremental wire-streaming is the follow-up); this
    // proves the new engine host imports are wired end to end through a
    // rebuilt js-engine.wasm.
    let source = r#"
        function handle(req, route, group) {
          const out = host.responseStream({
            status: 201,
            headers: [["content-type", "text/event-stream"]],
          });
          for (let i = 0; i < 3; i++) {
            out.write("data: chunk" + i + "\n\n");
          }
          out.close();
          // No return value — the host uses the streamed chunks.
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 201, "streamed status comes from start()");
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "streamed headers come from start()"
    );
    let body = resp.text().await.expect("body");
    assert_eq!(
        body, "data: chunk0\n\ndata: chunk1\n\ndata: chunk2\n\n",
        "body is the concatenation of the written chunks"
    );
    server.abort();
}

#[tokio::test]
async fn javascript_route_streams_chunks_incrementally_over_time() {
    // True incremental streaming (ADR-0022): the handler writes a
    // chunk, sleeps 120ms, repeats. The chunks must reach the client
    // *as they're written* (chunked transfer-encoding), not buffered
    // and flushed at the end — so the wall-clock spread between the
    // first and last received chunk reflects the handler's sleeps.
    use futures::StreamExt;

    let source = r#"
        function handle(req, route, group) {
          const out = host.responseStream({
            status: 200,
            headers: [["content-type", "text/event-stream"]],
          });
          for (let i = 0; i < 3; i++) {
            out.write("data: " + i + "\n\n");
            host.sleep(120);
          }
          out.close();
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);

    let start = std::time::Instant::now();
    let mut stream = resp.bytes_stream();
    let mut first_at: Option<std::time::Duration> = None;
    let mut last_at = std::time::Duration::ZERO;
    let mut body = Vec::new();
    while let Some(item) = stream.next().await {
        let bytes = item.expect("stream chunk");
        if bytes.is_empty() {
            continue;
        }
        let now = start.elapsed();
        first_at.get_or_insert(now);
        last_at = now;
        body.extend_from_slice(&bytes);
    }

    let spread = last_at - first_at.expect("at least one chunk");
    // Three writes with two 120ms sleeps between them → ~240ms ideal.
    // Assert a generous floor so a buffered-then-flushed implementation
    // (spread ≈ 0) fails but timing jitter doesn't.
    assert!(
        spread >= std::time::Duration::from_millis(150),
        "chunks should arrive spread over time (handler sleeps between writes); spread={spread:?}"
    );
    assert_eq!(
        String::from_utf8(body).expect("utf8"),
        "data: 0\n\ndata: 1\n\ndata: 2\n\n"
    );
    server.abort();
}

// -- ADR-0022 slice 2: streaming budget ------------------------------------

#[tokio::test]
async fn streaming_handler_runs_past_the_non_streaming_epoch() {
    // The engine epoch is set to 20 ticks (~200ms) — the buffered cap.
    // A streaming handler runs ~600ms (6 chunks × 100ms sleeps), which
    // is well past that cap but under the 5s stream budget. The
    // epoch-deadline callback must re-extend while streaming so the
    // handler completes and the client sees all six chunks, rather than
    // trapping at ~200ms.
    let source = r#"
        function handle(req, route, group) {
          const out = host.responseStream({ status: 200, headers: [] });
          for (let i = 0; i < 6; i++) {
            out.write("data: " + i + "\n\n");
            host.sleep(100);
          }
          out.close();
        }
    "#;
    let Some((addr, group, server)) =
        start_with_js_route_limited(source, 20, std::time::Duration::from_secs(5)).await
    else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    let body = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("data: 5"),
        "streaming handler should run to completion past the 200ms buffered epoch; body={body:?}"
    );
    server.abort();
}

#[tokio::test]
async fn non_streaming_handler_still_traps_at_the_epoch() {
    // Sibling to the above: with the same 200ms epoch and the ticker
    // running, a *non-streaming* handler that sleeps 800ms must still
    // trap at the deadline (proving the epoch is actually enforced, so
    // the streaming test's survival is meaningful and not just "the
    // ticker never fired").
    let source = r#"
        function handle(req, route, group) {
          host.sleep(800);
          return { status: 200, headers: [], body: new Uint8Array() };
        }
    "#;
    let Some((addr, group, server)) =
        start_with_js_route_limited(source, 20, std::time::Duration::from_secs(5)).await
    else {
        return;
    };

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        500,
        "non-streaming handler over the epoch budget should trap"
    );
    server.abort();
}

// -- Handler logging + network-global stubs (MCP field report) --------------

#[tokio::test]
async fn handler_logging_reaches_journal_and_fetch_is_catchable() {
    // Two fixes from the 2026-06-09 MCP field report, proven end to end:
    //   #1 the `log` host import is now surfaced to handlers as `log.*` and
    //      `console.*`, and the lines land in the journal entry's
    //      `handler_logs` (previously `log` was undefined → ReferenceError).
    //   #2 unsupported network globals (`fetch`, …) throw a *catchable*
    //      Error instead of hard-trapping the wasm instance.
    let source = r#"
        function handle(req, route, group) {
          log.info("hello from handler");
          console.log("via console");
          let fetchMsg = "fetch did not throw";
          try {
            fetch("https://example.com/");
          } catch (e) {
            fetchMsg = "caught: " + e.message;
          }
          return {
            status: 200,
            headers: [],
            body: new TextEncoder().encode(fetchMsg),
          };
        }
    "#;
    let Some((addr, group, server)) = start_with_js_route(source).await else {
        eprintln!("skipping: vendored js-engine.wasm not present");
        return;
    };

    // Dispatch on the group subdomain — writes a journal entry whose
    // handler_logs should now carry the emitted lines.
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/v1/echo"))
        .header(reqwest::header::HOST, format!("{group}.localhost"))
        .body("{}")
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status(),
        200,
        "a handler using log/console and try/catch around fetch returns 200, not a trap"
    );
    let body = resp.text().await.expect("body");
    assert!(
        body.starts_with("caught:") && body.contains("network access"),
        "fetch threw a catchable Error the handler could handle: {body}"
    );

    // Read the journal back via the control-plane API (bootstrap admin token,
    // addressed at the apex / direct host) and confirm the log lines landed.
    let listed: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{addr}/api/journal/{group}"))
        .header(reqwest::header::AUTHORIZATION, "Bearer wmt_test")
        .send()
        .await
        .expect("get journal")
        .json()
        .await
        .expect("json");
    let entry = &listed["entries"].as_array().expect("entries")[0];
    let logged = entry["handler_logs"]
        .as_array()
        .expect("handler_logs")
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        logged.contains("hello from handler") && logged.contains("via console"),
        "log.info + console.log lines reached the journal's handler_logs: {logged}"
    );
    server.abort();
}

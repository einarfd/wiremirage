//! Tier-3 binary smoke tests: exec the `wm` binary end-to-end against
//! a real `wm-host` booted in-process. Asserts stdout/stderr/exit
//! code so we know the binary actually wires through, not just the
//! library underneath.
//!
//! Bulk of coverage lives at tier-1 (`wm-core/tests/client_smoke.rs`)
//! and tier-2 (`wm-cli/tests/wm_core_against_host.rs`). Here we keep
//! it deliberately small — two or three commands, enough to catch
//! "the binary doesn't actually start" or "a clap arg got renamed
//! and the output drift broke" regressions.

use std::sync::Arc;

use assert_cmd::Command;
use wm_host::auth::Auth;
use wm_host::journal::Journal;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_TOKEN: &str = "wmt_test_bootstrap_token";

struct Harness {
    host_url: String,
    state: AppState,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn start() -> Harness {
    let storage = Storage::in_memory();
    let auth = Auth::new(storage.clone());
    auth.bootstrap_admin("bootstrap", BOOTSTRAP_TOKEN)
        .expect("bootstrap admin");
    let runtime = Arc::new(Runtime::new(storage.clone()).expect("runtime"));
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone()).expect("table");
    let journal = Journal::new(storage);
    let state = AppState::new(runtime, routes, auth, journal);
    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        host_url: format!("http://{addr}"),
        state,
        server,
    }
}

#[tokio::test]
async fn wm_health_against_real_host() {
    let h = start().await;
    let host = h.host_url.clone();
    // assert_cmd is sync; jump out of the tokio runtime to run it.
    let output = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args(["--host", &host, "health"])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");

    assert!(
        output.status.success(),
        "wm health exited {:?}: stdout={:?} stderr={:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status: ok"), "stdout was: {stdout}");
}

#[tokio::test]
async fn wm_health_json_format_is_machine_parseable() {
    let h = start().await;
    let host = h.host_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args(["--host", &host, "--json", "health"])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(output.status.success());
    let stdout = std::str::from_utf8(&output.stdout).expect("utf-8");
    let parsed: serde_json::Value = serde_json::from_str(stdout).expect("json parses");
    assert_eq!(parsed["status"], "ok");
}

#[tokio::test]
async fn wm_groups_create_then_list_then_delete() {
    let h = start().await;
    let host = h.host_url.clone();
    let token = BOOTSTRAP_TOKEN.to_string();

    // Create.
    let host_c = host.clone();
    let token_c = token.clone();
    let create = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host_c,
                "--token",
                &token_c,
                "--json",
                "groups",
                "create",
                "stripe-mock",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(
        create.status.success(),
        "create stderr: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&create.stdout).expect("create json parses");
    assert_eq!(parsed["name"], "stripe-mock");

    // List.
    let host_l = host.clone();
    let token_l = token.clone();
    let list = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host", &host_l, "--token", &token_l, "--json", "groups", "list",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(list.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list json parses");
    let names: Vec<&str> = parsed["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"stripe-mock"));

    // Delete.
    let host_d = host.clone();
    let token_d = token.clone();
    let del = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host_d,
                "--token",
                &token_d,
                "groups",
                "delete",
                "stripe-mock",
                "--force",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(del.status.success());
}

#[tokio::test]
async fn wm_match_against_real_host() {
    let h = start().await;
    // Plant a route via the registry (no wasm validation).
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            owner_id: "test-owner".into(),
        })
        .expect("create_route");
    h.state.routes().refresh_after_create(route);

    let host = h.host_url.clone();
    let token = BOOTSTRAP_TOKEN.to_string();

    // Hit case via JSON.
    let host_h = host.clone();
    let token_h = token.clone();
    let hit = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host_h,
                "--token",
                &token_h,
                "--json",
                "match",
                "POST",
                "/v1/charges",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(hit.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&hit.stdout).expect("hit json parses");
    assert_eq!(parsed["matched"], true);
    assert_eq!(parsed["route"]["path"], "/v1/charges");

    // Miss case via human format.
    let host_m = host;
    let token_m = token;
    let miss = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host_m,
                "--token",
                &token_m,
                "match",
                "GET",
                "/v1/charges",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(miss.status.success());
    let stdout = String::from_utf8_lossy(&miss.stdout);
    assert!(stdout.contains("no match"), "stdout was: {stdout}");
    assert!(
        stdout.contains("method_mismatch"),
        "expected method_mismatch reason, stdout was: {stdout}"
    );
}

#[tokio::test]
async fn wm_without_token_exits_4_on_authed_command() {
    let h = start().await;
    let host = h.host_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args(["--host", &host, "groups", "list"])
            // Don't inherit env so any developer-set WM_TOKEN doesn't
            // pollute the test.
            .env_remove("WM_TOKEN")
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert_eq!(output.status.code(), Some(4));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no token"),
        "expected 'no token' hint, got: {stderr}"
    );
}

#[tokio::test]
async fn wm_routes_update_changes_path() {
    let h = start().await;
    // Plant a route directly through the registry so we don't need a
    // real wasm fixture in this smoke test.
    let route = h
        .state
        .routes()
        .registry()
        .create_route(wm_host::registry::NewRoute {
            group: None,
            methods: vec!["POST".into()],
            path: "/v1/charges".into(),
            language: "wasm".into(),
            bindings_version: "0.1.0".into(),
            compiled_wasm: b"FAKE".to_vec(),
            owner_id: h
                .state
                .auth()
                .get_user_by_name("bootstrap")
                .expect("read bootstrap")
                .expect("bootstrap exists")
                .id,
        })
        .expect("create_route");
    let slug = format!("{}/{}", route.group_name, route.number);
    h.state.routes().refresh_after_create(route);

    let host = h.host_url.clone();
    let token = BOOTSTRAP_TOKEN.to_string();
    let slug_for_cmd = slug.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host,
                "--token",
                &token,
                "--json",
                "routes",
                "update",
                &slug_for_cmd,
                "--path",
                "/v1/refunds",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");

    assert!(
        out.status.success(),
        "wm routes update exited {:?}: stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("update json parses");
    assert_eq!(parsed["path"], "/v1/refunds");
}

#[tokio::test]
async fn wm_routes_update_requires_at_least_one_field() {
    let h = start().await;
    let host = h.host_url.clone();
    let token = BOOTSTRAP_TOKEN.to_string();
    let out = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host",
                &host,
                "--token",
                &token,
                "routes",
                "update",
                "some-group/1",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    // Validation error, not 0 — CLI returns exit code 1 for generic
    // validation failures (route doesn't exist either, but the local
    // check short-circuits before the host call).
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("requires at least one of"),
        "expected usage hint, got: {stderr}"
    );
}

#[tokio::test]
async fn wm_users_list_create_delete_round_trip() {
    let h = start().await;
    let host = h.host_url.clone();
    let token = BOOTSTRAP_TOKEN.to_string();

    // Create alice.
    let host_c = host.clone();
    let token_c = token.clone();
    let create = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host", &host_c, "--token", &token_c, "--json", "users", "create", "alice",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(
        create.status.success(),
        "create stderr: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&create.stdout).expect("create json parses");
    assert_eq!(parsed["name"], "alice");
    assert_eq!(parsed["is_admin"], false);

    // List should now contain bootstrap + alice.
    let host_l = host.clone();
    let token_l = token.clone();
    let list = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host", &host_l, "--token", &token_l, "--json", "users", "list",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(list.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&list.stdout).expect("list json parses");
    let names: Vec<&str> = parsed["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alice"));
    assert!(names.contains(&"bootstrap"));

    // Delete alice.
    let host_d = host;
    let token_d = token;
    let del = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args([
                "--host", &host_d, "--token", &token_d, "users", "delete", "alice", "--force",
            ])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(del.status.success());
}

#[tokio::test]
async fn wm_completion_emits_shell_specific_output() {
    // Doesn't need a host — `wm completion` is purely local.
    let bash = tokio::task::spawn_blocking(|| {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args(["completion", "bash"])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(bash.status.success());
    let stdout = String::from_utf8_lossy(&bash.stdout);
    // Bash completion scripts start with `_<bin>()` shell function.
    assert!(stdout.contains("_wm()"), "bash output was: {stdout}");

    let zsh = tokio::task::spawn_blocking(|| {
        Command::cargo_bin("wm")
            .expect("locate wm binary")
            .args(["completion", "zsh"])
            .output()
            .expect("run wm")
    })
    .await
    .expect("blocking");
    assert!(zsh.status.success());
    let stdout = String::from_utf8_lossy(&zsh.stdout);
    // zsh scripts start with `#compdef <bin>`.
    assert!(stdout.contains("#compdef wm"), "zsh output was: {stdout}");
}

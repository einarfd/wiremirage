use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tracing_subscriber::EnvFilter;
use wm_host::{AppState, Runtime, Storage, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let listen_addr = env::var("WM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let wasm_path = env::var("WM_FIXTURE_WASM")
        .context("WM_FIXTURE_WASM must point to a compiled .component.wasm")?;
    let storage = build_storage()?;

    let runtime = Arc::new(Runtime::new(storage)?);
    let component = runtime.load_component(&PathBuf::from(&wasm_path))?;

    let app = router(AppState::new(runtime, component));

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, wasm = %wasm_path, "wm-host listening");

    axum::serve(listener, app).await?;
    Ok(())
}

/// Resolve the storage backend from `WM_STORAGE`. No silent fallback: if the
/// var is unset or the value isn't recognised, we fail with a message naming
/// the variable. Misconfigured deployments fail at startup; tests opt in to
/// the in-memory backend explicitly.
fn build_storage() -> anyhow::Result<Storage> {
    let raw = env::var("WM_STORAGE").map_err(|_| {
        anyhow!(
            "WM_STORAGE is not set. Required values:\n  \
             memory                    use the in-memory backend (state lost on restart)\n  \
             redis://host:port[/db]    Valkey/Redis-compatible URL\n  \
             rediss://host:port[/db]   same, with TLS"
        )
    })?;

    if raw == "memory" {
        tracing::warn!(
            "WM_STORAGE=memory: in-memory backend, state is lost on restart and not shared across hosts"
        );
        return Ok(Storage::in_memory());
    }

    if raw.starts_with("redis://") || raw.starts_with("rediss://") {
        return Storage::valkey(&raw)
            .map_err(|e| anyhow!("WM_STORAGE points at Valkey but the connection failed: {e}"));
    }

    Err(anyhow!(
        "WM_STORAGE={raw:?} is not a recognised value. Use \"memory\", \"redis://...\", or \"rediss://...\"."
    ))
}

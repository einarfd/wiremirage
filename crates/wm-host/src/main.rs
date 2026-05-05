use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use wm_host::{AppState, Runtime, router};

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

    let runtime = Arc::new(Runtime::new()?);
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

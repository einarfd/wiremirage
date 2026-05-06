use std::env;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tracing_subscriber::EnvFilter;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    require_insecure_no_auth_acknowledgement()?;
    let listen_addr = env::var("WM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let storage = build_storage()?;

    let runtime = Arc::new(Runtime::new(storage.clone())?);
    let registry = Arc::new(Registry::new(storage));
    let routes = RouteTable::warm(registry, runtime.engine().clone())?;

    let app = router(AppState::new(runtime, routes));

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "wm-host listening");

    axum::serve(listener, app).await?;
    Ok(())
}

/// Slice 3 ships without auth: `POST /__api/routes` is fully open. To
/// prevent that from being deployed by accident, the host refuses to start
/// unless `WM_INSECURE_NO_AUTH=1` acknowledges the foot-gun. Real bearer-
/// token auth lands in a follow-up slice; once it does, this gate is
/// either retired or kept as an opt-in for trusted networks.
fn require_insecure_no_auth_acknowledgement() -> anyhow::Result<()> {
    match env::var("WM_INSECURE_NO_AUTH").as_deref() {
        Ok("1") => {
            tracing::warn!(
                "WM_INSECURE_NO_AUTH=1: REST API is open without authentication. \
                 Do not expose this host to untrusted networks."
            );
            Ok(())
        }
        _ => Err(anyhow!(
            "WM_INSECURE_NO_AUTH is not set to 1.\n\n  \
             This build of WireMirage has no authentication on its REST API.\n  \
             Anyone who can reach the listener can create and delete routes.\n\n  \
             Set WM_INSECURE_NO_AUTH=1 to acknowledge this and continue, or\n  \
             wait for the auth slice to land."
        )),
    }
}

/// Resolve the storage backend from `WM_STORAGE`. No silent fallback: if
/// the var is unset or the value isn't recognised, we fail with a message
/// naming the variable. Misconfigured deployments fail at startup.
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

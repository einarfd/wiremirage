use std::env;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tracing_subscriber::EnvFilter;
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_USER_NAME: &str = "bootstrap";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let listen_addr = env::var("WM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let storage = build_storage()?;
    let auth = Auth::new(storage.clone());
    bootstrap_admin_if_requested(&auth)?;

    let runtime = Arc::new(Runtime::new(storage.clone())?);
    let registry = Arc::new(Registry::new(storage));
    let routes = RouteTable::warm(registry, runtime.engine().clone())?;

    let mut state = AppState::new(runtime, routes, auth);
    if let Some(compiler) = CompilerClient::from_env() {
        tracing::info!(url = compiler.base_url(), "compiler sidecar configured");
        state = state.with_compiler(compiler);
    } else {
        tracing::info!(
            "WM_COMPILER_URL is not set; only `language: \"wasm\"` requests will be accepted by /__api/routes"
        );
    }
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "wm-host listening");

    axum::serve(listener, app).await?;
    Ok(())
}

/// Honour `WM_BOOTSTRAP_TOKEN` on first startup: create an admin user
/// named `bootstrap` whose token is the supplied plaintext. Idempotent —
/// subsequent starts with the same env var are no-ops.
///
/// The host will start without `WM_BOOTSTRAP_TOKEN` if at least one user
/// already exists; otherwise it errors so a fresh deployment doesn't
/// silently come up with no way to authenticate.
fn bootstrap_admin_if_requested(auth: &Auth) -> anyhow::Result<()> {
    match env::var("WM_BOOTSTRAP_TOKEN") {
        Ok(plaintext) if plaintext.is_empty() => Err(anyhow!(
            "WM_BOOTSTRAP_TOKEN is set but empty. Either supply a non-empty token or unset the variable."
        )),
        Ok(plaintext) => {
            if !plaintext.starts_with("wmt_") {
                tracing::warn!(
                    "WM_BOOTSTRAP_TOKEN does not start with `wmt_`; tokens by convention use that prefix"
                );
            }
            let created = auth
                .bootstrap_admin(BOOTSTRAP_USER_NAME, &plaintext)
                .map_err(|e| anyhow!("failed to bootstrap admin: {e}"))?;
            if created {
                tracing::warn!(
                    "Bootstrapped admin user {BOOTSTRAP_USER_NAME:?}; the supplied token is now its API token. \
                     Treat WM_BOOTSTRAP_TOKEN like a credential."
                );
            } else {
                tracing::info!(
                    "WM_BOOTSTRAP_TOKEN provided but bootstrap user already exists; ignoring \
                     (rotate via /__api/tokens or by deleting the bootstrap user first)"
                );
            }
            Ok(())
        }
        Err(_) => {
            // No bootstrap token supplied. Fine if some user already
            // exists (e.g., this is a restart against a populated
            // backing store); otherwise refuse to start so a fresh
            // deployment doesn't silently come up with no way to
            // authenticate.
            if auth
                .any_user_exists()
                .map_err(|e| anyhow!("failed to check user count: {e}"))?
            {
                Ok(())
            } else {
                Err(anyhow!(
                    "no users exist and WM_BOOTSTRAP_TOKEN is not set. \
                     Set WM_BOOTSTRAP_TOKEN=wmt_<plaintext> on first startup \
                     to provision an admin user named {BOOTSTRAP_USER_NAME:?}."
                ))
            }
        }
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

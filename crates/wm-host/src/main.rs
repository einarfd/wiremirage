use std::env;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use wm_host::auth::Auth;
use wm_host::compiler::CompilerClient;
use wm_host::github_oauth::GitHubConfig;
use wm_host::journal::Journal;
use wm_host::lifecycle::Sweeper;
use wm_host::local_auth::LocalAuth;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::telemetry;
use wm_host::{AppState, Runtime, Storage, router};

const BOOTSTRAP_USER_NAME: &str = "bootstrap";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut telemetry_guard = telemetry::init()?;

    let listen_addr = env::var("WM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let storage = build_storage()?;
    let auth = Auth::new(storage.clone());
    bootstrap_admin_if_requested(&auth)?;

    let runtime = Arc::new(Runtime::new(storage.clone())?);
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone())?;
    let journal = Journal::new(storage.clone());

    let mut state = AppState::new(runtime, routes, auth, journal);
    if let Some(compiler) = CompilerClient::from_env() {
        tracing::info!(url = compiler.base_url(), "compiler sidecar configured");
        state = state.with_compiler(compiler);
    } else {
        tracing::info!(
            "WM_COMPILER_URL is not set; only `language: \"wasm\"` requests will be accepted by /__api/routes"
        );
    }

    // Local auth (slice 20). Parse WM_LOCAL_AUTH and wire SESSION_SECRET.
    // Both are independent — operators can configure either, neither,
    // or both. When `WM_LOCAL_AUTH` is set, `SESSION_SECRET` becomes
    // required so the login flow can mint cookies; we refuse to start
    // in that case if it's missing rather than silently 503ing later.
    state = configure_local_auth(state, storage)?;

    // GitHub OAuth (slice 50, ADR-0010). Optional. When configured,
    // the login page shows a "Continue with GitHub" button and the
    // `/__auth/start/github` + `/__auth/callback` routes are live.
    // SESSION_SECRET is required when GitHub login is enabled — the
    // callback flow can't mint a cookie otherwise. Errors at parse
    // time (partial credentials, missing allow rules) bubble up here
    // so misconfiguration surfaces at startup.
    if let Some(gh) = GitHubConfig::from_env().context("parse GitHub OAuth config")? {
        if state.sessions().is_none() {
            return Err(anyhow!(
                "WM_GITHUB_CLIENT_ID is set but SESSION_SECRET is missing. \
                 GitHub login can't mint cookies without a signing key — \
                 set SESSION_SECRET to at least 32 bytes of secret material."
            ));
        }
        tracing::info!(
            allow_users = gh.allow_users.len(),
            allow_orgs = gh.allow_orgs.len(),
            admin_users = gh.admin_users.len(),
            "GitHub OAuth configured"
        );
        state = state.with_github_oauth(gh);
    } else {
        tracing::info!(
            "WM_GITHUB_CLIENT_ID is not set; the login page will not offer GitHub login"
        );
    }

    // Slice 44: opt-in hardening flags for deployments behind a TLS
    // edge + reverse proxy. Defaults stay safe for plain-HTTP dev
    // workflows (no `Secure` cookies, no `X-Forwarded-For` trust).
    if parse_env_bool("WM_SECURE_COOKIES") {
        tracing::info!("WM_SECURE_COOKIES=1; emitting `Secure` on session + CSRF cookies");
        state = state.with_secure_cookies(true);
    }
    if parse_env_bool("WM_TRUST_FORWARDED_HEADERS") {
        tracing::info!(
            "WM_TRUST_FORWARDED_HEADERS=1; honoring X-Forwarded-For for the login throttle key"
        );
        state = state.with_trust_forwarded_headers(true);
    }

    // Shutdown signal that long-lived handlers (the SSE journal tail,
    // primarily) race against so a browser tab pointed at the live
    // view doesn't pin the host open during graceful shutdown.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    state = state.with_shutdown(shutdown_rx);

    let app = router(state.clone());

    // Spawn the lifecycle sweeper. It walks the route table on its
    // cadence and reaps the children of any group whose Valkey TTL
    // has fired. The handle is intentionally dropped — tokio cleans
    // it up on process shutdown along with the runtime.
    let _sweeper = Sweeper::new(state.routes().clone()).spawn();

    // Spawn the wasmtime epoch ticker (slice 46 / F-1). Required by
    // the `epoch_interruption(true)` flag on the engine — without a
    // ticker advancing the epoch, the per-call deadline configured
    // on each store would never fire. Handle dropped; the task runs
    // for the engine's lifetime.
    let _epoch_ticker = state.runtime().spawn_epoch_ticker();

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("bind {listen_addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "wm-host listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Tell streaming handlers to wrap up *before* axum's
            // graceful-shutdown waits for in-flight requests to
            // drain — otherwise the SSE tail keeps every browser
            // EventSource alive indefinitely and the host hangs.
            let _ = shutdown_tx.send(true);
        })
        .await?;

    // Flush in-flight spans before the process exits. The Drop impl
    // would catch this too, but doing it explicitly surfaces any flush
    // error in the logs while logging is still wired up.
    telemetry_guard.shutdown();
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM. axum drains in-flight requests, then
/// `main` returns and the telemetry guard flushes.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

/// Parse an env var as a boolean flag. Accepts `1`, `true`, `yes`,
/// `on` (case-insensitive) as true; any other value (including
/// `0`, `false`, empty, unset) is false. Lets operators write
/// `WM_SECURE_COOKIES=true` or `=1` interchangeably without
/// surprising the next reader.
fn parse_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Parse `WM_LOCAL_AUTH` + `SESSION_SECRET` and attach the resulting
/// local-auth map and session store to `state`. Fail-fast on bad
/// input — a misconfigured login surface should surface at boot,
/// not on the first failed login.
fn configure_local_auth(mut state: AppState, storage: Storage) -> anyhow::Result<AppState> {
    let raw_local = env::var("WM_LOCAL_AUTH").unwrap_or_default();
    let local_auth = LocalAuth::parse(&raw_local).map_err(|e| anyhow!("WM_LOCAL_AUTH: {e}"))?;
    let local_configured = !local_auth.is_empty();
    state = state.with_local_auth(local_auth);

    match env::var("SESSION_SECRET") {
        Ok(secret) if !secret.is_empty() => {
            let sessions = SessionStore::new(storage, secret.as_bytes())
                .map_err(|e| anyhow!("SESSION_SECRET: {e}"))?;
            tracing::info!("session store configured (TTL={}s)", sessions.ttl_seconds());
            state = state.with_sessions(sessions);
        }
        _ => {
            if local_configured {
                return Err(anyhow!(
                    "WM_LOCAL_AUTH is set but SESSION_SECRET is missing. \
                     Local login can't mint cookies without a signing key — \
                     set SESSION_SECRET to at least 32 bytes of secret material."
                ));
            }
            tracing::info!(
                "SESSION_SECRET unset and no browser-login methods configured; \
                 `/__api/*` will only accept bearer tokens"
            );
        }
    }

    if local_configured {
        tracing::warn!(
            "WM_LOCAL_AUTH is configured. This auth mode is for testing and \
             trusted-network deployments only — see ADR-0018."
        );
    }
    Ok(state)
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

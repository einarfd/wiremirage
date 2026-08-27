use std::env;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use wm_host::auth::Auth;
use wm_host::github_oauth::GitHubConfig;
use wm_host::journal::Journal;
use wm_host::lifecycle::Sweeper;
use wm_host::local_auth::LocalAuth;
use wm_host::oidc::OidcConfig;
use wm_host::registry::Registry;
use wm_host::route_table::RouteTable;
use wm_host::session::SessionStore;
use wm_host::telemetry;
use wm_host::{AppState, Runtime, Storage, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut telemetry_guard = telemetry::init()?;

    let listen_addr = env::var("WM_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let storage = build_storage()?;
    let auth = Auth::new(storage.clone());
    bootstrap_admin_if_requested(&auth)?;

    // The shared JS engine component (ADR-0020). Built at cargo build
    // time by `build.rs` (via `compiler/js-engine/Dockerfile`) and
    // embedded into the binary. The `WM_JS_ENGINE_WASM` env var is set
    // by build.rs to the absolute path of the artifact in cargo's
    // OUT_DIR.
    const JS_ENGINE_BYTES: &[u8] = include_bytes!(env!("WM_JS_ENGINE_WASM"));
    let runtime = Runtime::new(storage.clone())?
        .with_js_engine_bytes(JS_ENGINE_BYTES)
        .map_err(|e| anyhow!("load embedded js-engine.wasm: {e}"))?;
    let runtime = Arc::new(runtime);
    let registry = Arc::new(Registry::new(storage.clone()));
    let routes = RouteTable::warm(registry, runtime.engine().clone())?;
    let journal = Journal::new(storage.clone());

    let mut state = AppState::new(runtime, routes, auth, journal);

    // Local auth (slice 20). Parse WM_LOCAL_AUTH and wire SESSION_SECRET.
    // Both are independent — operators can configure either, neither,
    // or both. When `WM_LOCAL_AUTH` is set, `SESSION_SECRET` becomes
    // required so the login flow can mint cookies; we refuse to start
    // in that case if it's missing rather than silently 503ing later.
    state = configure_local_auth(state, storage)?;

    // GitHub OAuth (slice 50, ADR-0010). Optional. When configured,
    // the login page shows a "Continue with GitHub" button and the
    // `/auth/start/github` + `/auth/callback` routes are live.
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

    // Generic OIDC (ADR-0035). Optional. When configured, the login
    // page shows a "Continue with {display name}" button and the
    // `/auth/start/oidc` + `/auth/callback/oidc` routes are live.
    // The issuer's discovery document is fetched here, once — an
    // unreachable or mismatched IdP refuses startup instead of
    // 503ing the first user who clicks the button.
    if let Some(oidc_config) = OidcConfig::from_env().context("parse OIDC config")? {
        if state.sessions().is_none() {
            return Err(anyhow!(
                "WM_OIDC_ISSUER is set but SESSION_SECRET is missing. \
                 OIDC login can't mint cookies without a signing key — \
                 set SESSION_SECRET to at least 32 bytes of secret material."
            ));
        }
        let provider = oidc_config
            .discover()
            .await
            .context("resolve OIDC discovery document (is the IdP reachable and WM_OIDC_ISSUER exactly its issuer URL?)")?;
        tracing::info!(
            issuer = %provider.config.issuer,
            display_name = %provider.config.display_name,
            allow_all = provider.config.allow_all,
            allow_emails = provider.config.allow_emails.len(),
            allow_domains = provider.config.allow_domains.len(),
            allow_groups = provider.config.allow_groups.len(),
            "OIDC login configured"
        );
        state = state.with_oidc(provider);
    } else {
        tracing::info!("WM_OIDC_ISSUER is not set; the login page will not offer OIDC login");
    }

    // Final fail-fast: refuse to come up if no users exist AND no
    // login method is configured. Deferred until here so the check
    // can see the configured state of bootstrap, local-auth, and
    // GitHub OAuth all at once.
    ensure_login_method_available(&state)?;

    // ADR-0027: one switch for "behind a trusted TLS-terminating proxy".
    // `WM_TRUSTED_PROXY=<host[,host...]>` turns on the whole behind-a-proxy
    // posture together — `Secure` cookies, trusting `X-Forwarded-*` (throttle
    // IP + OAuth proto/host), and allowlisting the public hostname(s) for the
    // MCP `Host`-header check. Unset = direct-exposure defaults. One setting
    // so it can't be half-configured.
    let trusted = trusted_proxy_hosts();

    // ADR-0030 virtual-host routing: the apex hostname. Mock traffic is
    // served on `{group}.{apex}`; the apex is control-plane only. Derived
    // once here so dispatch can resolve which group a request targets.
    let apex = apex_host(trusted.as_deref());
    tracing::info!(apex = %apex, "apex host set (mock traffic served on group subdomains; ADR-0030)");
    state = state.with_apex_host(apex);

    if let Some(hosts) = trusted {
        tracing::info!(
            hosts = ?hosts,
            "WM_TRUSTED_PROXY set; behind-a-proxy mode: Secure cookies, trusting \
             X-Forwarded-*, and allowlisting these hosts for MCP"
        );
        state = state
            .with_secure_cookies(true)
            .with_trust_forwarded_headers(true)
            .with_mcp_allowed_hosts(hosts);
    }

    // ADR-0034: outbound-callback egress policy. Off unless `WM_EGRESS` is
    // set; even on, a hardcoded special-use default-deny applies and each
    // group must opt in via `callout_enabled`. A malformed
    // `WM_EGRESS_ALLOW`/`WM_EGRESS_DENY` list fails fast here.
    let egress = wm_host::egress::EgressPolicy::from_env().context("parse WM_EGRESS* config")?;
    if egress.is_enabled() {
        tracing::info!(
            "WM_EGRESS on: outbound handler callbacks available (per-group opt-in via \
             callout_enabled; special-use ranges denied by default — ADR-0034)"
        );
    } else {
        tracing::info!("WM_EGRESS off: outbound handler callbacks disabled (default; ADR-0034)");
    }
    state = state.with_egress(egress);

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

    // Spawn the route-invalidation subscriber (ADR-0037). Deletes and
    // source updates are the cases the read-through floor can't cover —
    // a stale route still matches, so the request never reaches the
    // miss path — so siblings are told directly. Returns None on
    // in-memory storage, where there are no siblings. Handle dropped
    // like the others; the task owns its own reconnect loop.
    let _invalidation_subscriber = wm_host::bus::spawn_route_invalidation_subscriber(
        state.runtime().storage().clone(),
        state.routes().clone(),
    );

    // Spawn the journal fan-out subscriber (ADR-0037). Without it a
    // live tail sees only the traffic its own replica dispatched —
    // roughly 1/N of matching requests, failing silently. Subscribes
    // lazily: it idles until something is actually tailing here, so a
    // replica with no tails deserializes nothing.
    let _journal_subscriber = wm_host::bus::spawn_journal_subscriber(state.journal().clone());

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

/// Parse `WM_TRUSTED_PROXY` (ADR-0027): a comma-separated list of the
/// public hostname(s) the trusted TLS edge serves. Returns `Some(hosts)`
/// when at least one non-empty hostname is present, else `None` (unset /
/// empty → direct-exposure defaults). Presence is the switch; the
/// hostnames feed the MCP `Host`-header allowlist.
fn trusted_proxy_hosts() -> Option<Vec<String>> {
    let raw = env::var("WM_TRUSTED_PROXY").ok()?;
    let hosts: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!hosts.is_empty()).then_some(hosts)
}

/// Resolve the apex hostname (ADR-0030 virtual-host routing). `WM_APEX_HOST`
/// takes precedence (explicit override / dev knob); else the first
/// `WM_TRUSTED_PROXY` host (prod names the public apex there, so no separate
/// var is needed); else `localhost` for local dev. Lowercased.
fn apex_host(trusted: Option<&[String]>) -> String {
    if let Ok(raw) = env::var("WM_APEX_HOST") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_ascii_lowercase();
        }
    }
    if let Some(first) = trusted.and_then(|hosts| hosts.first()) {
        return first.to_ascii_lowercase();
    }
    "localhost".to_string()
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
                 `/api/*` will only accept bearer tokens"
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
/// identified by `WM_BOOTSTRAP_EMAIL` whose token is the supplied
/// plaintext. Both variables are required together — accounts are keyed
/// by email, so a token without an identity is a config error.
/// Idempotent — subsequent starts with the same env vars are no-ops,
/// and a pre-email deployment's legacy `bootstrap` record is adopted
/// (gains the email; its token keeps working) rather than duplicated.
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
            let email = env::var("WM_BOOTSTRAP_EMAIL").map_err(|_| {
                anyhow!(
                    "WM_BOOTSTRAP_TOKEN is set but WM_BOOTSTRAP_EMAIL is not. Accounts are keyed \
                 by email — set WM_BOOTSTRAP_EMAIL to the admin's email address (logging in \
                 via OAuth/OIDC with the same verified email reaches the same account)."
                )
            })?;
            let email = email.trim().to_string();
            if email.is_empty() || !email.contains('@') {
                return Err(anyhow!(
                    "WM_BOOTSTRAP_EMAIL must be an email address, got {email:?}"
                ));
            }
            let created = auth
                .bootstrap_admin(&email, &plaintext)
                .map_err(|e| anyhow!("failed to bootstrap admin: {e}"))?;
            if created {
                tracing::warn!(
                    "Bootstrapped admin user {email:?}; the supplied token is now its API token. \
                     Treat WM_BOOTSTRAP_TOKEN like a credential."
                );
            } else {
                tracing::info!(
                    "WM_BOOTSTRAP_TOKEN provided but the bootstrap admin already exists; ignoring \
                     (rotate via /api/tokens or by deleting the bootstrap user first)"
                );
            }
            Ok(())
        }
        Err(_) => {
            // No bootstrap token. We can't decide whether this is OK
            // yet — a browser-login method (local-auth or GitHub OAuth)
            // might be configured below, in which case the first login
            // will provision the first user. Defer the "no way to
            // authenticate" check to `ensure_login_method_available`
            // after all login-method env vars have been parsed.
            Ok(())
        }
    }
}

/// Fail-fast guard that runs *after* all login-method setup. Refuses
/// to start only when no users exist AND no login method is wired up,
/// so a fresh deployment can't silently come up unreachable.
///
/// "Login method configured" means any of:
///   - `WM_BOOTSTRAP_TOKEN` was set (handled in
///     [`bootstrap_admin_if_requested`]; if it ran, the bootstrap user
///     exists and `any_user_exists` returns true).
///   - `WM_LOCAL_AUTH` is non-empty (parsed into `state.local_auth()`
///     by [`configure_local_auth`]).
///   - GitHub OAuth is configured (parsed into `state.github_config()`
///     by `GitHubConfig::from_env()` in main).
///   - OIDC login is configured (`WM_OIDC_ISSUER` etc., ADR-0035).
///
/// First-login-creates-first-user covers the bootstrap gap for the
/// browser-login paths, so an operator who only wants GitHub OAuth
/// doesn't need to also supply a one-shot bootstrap token. Existing
/// users always make this a no-op.
fn ensure_login_method_available(state: &AppState) -> anyhow::Result<()> {
    if state
        .auth()
        .any_user_exists()
        .map_err(|e| anyhow!("failed to check user count: {e}"))?
    {
        return Ok(());
    }
    let has_local = !state.local_auth().is_empty();
    let has_github = state.github_oauth().is_some();
    let has_oidc = state.oidc().is_some();
    if has_local || has_github || has_oidc {
        let mut methods = Vec::new();
        if has_local {
            methods.push("local-password");
        }
        if has_github {
            methods.push("GitHub OAuth");
        }
        if has_oidc {
            methods.push("OIDC");
        }
        let methods = methods.join(" and ");
        tracing::info!(
            "no users exist yet; first browser login ({methods}) will create the first user"
        );
        return Ok(());
    }
    Err(anyhow!(
        "no users exist and no login method is configured. \
         Either set WM_BOOTSTRAP_TOKEN=wmt_<plaintext> plus \
         WM_BOOTSTRAP_EMAIL=<email> to provision a bearer-token admin, \
         or configure WM_LOCAL_AUTH, GitHub OAuth (WM_GITHUB_CLIENT_ID + \
         WM_GITHUB_CLIENT_SECRET + allow rules), or OIDC \
         (WM_OIDC_ISSUER + client credentials + allow rules) so a \
         browser login can create the first user."
    ))
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

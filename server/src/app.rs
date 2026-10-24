//! The HTTP/WebSocket application: shared state, router, middleware and the `serve` entry
//! point. `main.rs` only parses the command line and calls in here.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::{delete, get, post, put},
    Json, Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::config::ServeArgs;
use crate::storage::Storage;
use crate::vault::DocRegistry;
use crate::{guest, limits, remote, sharing, account, blobs, ws};

pub struct AppState {
    pub serve: ServeArgs,
    pub storage: Storage,
    pub docs: Arc<DocRegistry>,
    pub limits: Arc<limits::Limits>,
    pub guest_sessions: parking_lot::Mutex<guest::GuestSessions>,
}

pub type SharedState = Arc<AppState>;

/// Run the server until Ctrl-C.
pub async fn serve(args: ServeArgs, storage: Storage) -> anyhow::Result<()> {
    // Zero-config first start: create an owner user and print the token once.
    if storage.registry().user_count()? == 0 {
        let u = storage.registry().create_user("owner")?;
        let token = storage.registry().create_token(&u.id, "initial")?;
        tracing::info!("first start: created user 'owner'");
        match &args.initial_token_file {
            Some(path) => {
                std::fs::write(path, format!("{token}\n")).with_context(|| format!("writing {}", path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                tracing::info!("  owner token written to {} (mode 0600); it is not in this log", path.display());
            }
            None => {
                tracing::info!("========================================================");
                tracing::info!("  token: {token}");
                tracing::info!("  paste the server URL and this token into the plugin");
                tracing::info!("  add more users with: samsyn user add <name>");
                tracing::info!("  (this is the only copy; --initial-token-file keeps it out of logs)");
                tracing::info!("========================================================");
            }
        }
    }

    let identity = storage.identity()?;
    let node_id = identity.public().to_string();
    tracing::info!("server node id: {node_id}");

    // Peer-to-peer endpoint for server-to-server shares (phase 2).
    let endpoint = if args.no_p2p {
        tracing::info!("p2p disabled; tickets only work between vaults on this server");
        None
    } else {
        match remote::bind_endpoint(identity, &args.relay, args.p2p_port).await {
            Ok(ep) => {
                let relays_off = args.relay.iter().any(|r| r.trim() == "none");
                tracing::info!("p2p endpoint bound ({})", if relays_off { "direct only, no relay" } else { "relay + direct" });
                {
                    // Relay registration can take a few seconds; do not hold up HTTP for it.
                    let ep = ep.clone();
                    tokio::spawn(async move {
                        if !relays_off && tokio::time::timeout(Duration::from_secs(20), ep.online()).await.is_err() {
                            tracing::warn!("p2p endpoint not online after 20 s (relay unreachable?); direct connections may still work");
                        }
                        tracing::info!("p2p addresses: {:?}", remote::addr_strings(&ep));
                    });
                }
                Some(ep)
            }
            Err(e) => {
                // A busy UDP port must not take the sync server down; only server-to-server
                // shares are affected.
                tracing::error!("p2p endpoint unavailable (port {} busy?): {e:#}; continuing without server-to-server sharing", args.p2p_port);
                None
            }
        }
    };

    let max_blob = args.max_blob_mb * 1024 * 1024;
    let limits = Arc::new(limits::Limits::default());
    let docs = Arc::new(DocRegistry::new(storage.clone(), node_id, endpoint.clone(), max_blob, args.max_vaults_per_user, args.max_doc_mb * 1024 * 1024));
    let router = endpoint.map(|ep| {
        iroh::protocol::Router::builder(ep)
            .accept(remote::ALPN, remote::ShareProtocol::new(docs.clone(), limits.clone(), args.max_conns_per_peer))
            .spawn()
    });
    let state: SharedState = Arc::new(AppState { docs, storage, serve: args, limits, guest_sessions: Default::default() });
    match &state.serve.public_url {
        Some(u) => tracing::info!("guest mode on: tickets carry public URL {u}"),
        None => tracing::info!("guest mode off (set --public-url to let people without a server join shares)"),
    }

    state.docs.start_presence_mirror();
    state.docs.preload_bridged();

    // Everything a user token protects sits behind `require_user`, so a bad or missing token is
    // answered before any body is read or parsed (anonymous callers cannot make us parse JSON).
    let user_api = Router::new()
        .route("/me", get(ws::whoami))
        .route("/ws/{vault}", get(ws::ws_handler))
        .route("/api/vaults/{vault}/sharing", get(sharing::get_sharing))
        .route("/api/vaults/{vault}/shares", post(sharing::create_share))
        .route("/api/shares/{id}", delete(sharing::delete_share).patch(sharing::move_share))
        .route("/api/shares/{id}/tickets", post(sharing::new_ticket))
        .route("/api/shares/{id}/grants/{label}", delete(sharing::revoke_ticket))
        .route("/api/shares/{id}/grants/{label}/restore", post(sharing::restore_ticket))
        .route("/api/vaults/{vault}/mounts", post(sharing::create_mount))
        .route("/api/mounts/{id}", delete(sharing::delete_mount).patch(sharing::move_mount))
        .route("/api/mounts/{id}/forward", put(sharing::set_mount_forward))
        .route("/api/tickets/inspect", post(sharing::inspect_ticket))
        .route("/api/me/tokens", get(account::list_tokens).post(account::create_token))
        .route("/api/me/tokens/{label}", delete(account::revoke_token))
        .route(
            "/blobs/{vault}/{hash}",
            put(blobs::put_blob).get(blobs::get_blob).layer(DefaultBodyLimit::max(max_blob)),
        )
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_user));

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .merge(user_api)
        .route("/guest/session/{id}", post(guest::session))
        .route("/guest/shares/{id}", get(guest::info))
        .route("/guest/ws/{id}", get(guest::ws))
        .route(
            "/guest/blobs/{id}/{hash}",
            put(guest::put_blob).get(guest::get_blob).layer(DefaultBodyLimit::max(max_blob)),
        )
        .layer(cors_layer(&state.serve))
        // Span without the query string: guest routes carry the ticket secret there.
        .layer(TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
            tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
        }))
        .with_state(state.clone());

    if state.serve.idle_unload_minutes > 0 {
        let st = state.clone();
        tokio::spawn(async move {
            let idle = Duration::from_secs(st.serve.idle_unload_minutes * 60);
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                st.docs.unload_idle(idle, st.serve.snapshot_keep);
            }
        });
    }

    if state.serve.snapshot_hours > 0 {
        let st = state.clone();
        tokio::spawn(async move {
            let every = Duration::from_secs(st.serve.snapshot_hours * 3600);
            loop {
                tokio::time::sleep(every).await;
                st.docs.snapshot_all(st.serve.snapshot_keep);
            }
        });
    }

    tracing::info!("data dir: {}", state.storage.root().display());

    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
        });
    }

    match (&state.serve.tls_cert, &state.serve.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| format!("loading TLS cert {} / key {}", cert.display(), key.display()))?;
            tracing::info!("samsyn listening on https://{} (TLS)", state.serve.bind);
            axum_server::bind_rustls(state.serve.bind, tls)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
        _ => {
            tracing::info!("samsyn listening on http://{}", state.serve.bind);
            tracing::info!("note: iOS needs https; put this behind Caddy/Tailscale or pass --tls-cert/--tls-key");
            axum_server::bind(state.serve.bind)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
    }

    if let Some(r) = router {
        let _ = r.shutdown().await;
    }
    tracing::info!("flushing snapshots before exit");
    state.docs.snapshot_all(state.serve.snapshot_keep);
    Ok(())
}

/// CORS: Obsidian's own origins plus whatever the operator adds. The plugin itself uses
/// Obsidian's native request API and WebSockets, neither of which is subject to CORS, so this
/// only matters for browsers; `--cors-any` restores the permissive behaviour.
fn cors_layer(args: &ServeArgs) -> CorsLayer {
    use axum::http::HeaderValue;
    use tower_http::cors::AllowOrigin;
    let base = CorsLayer::new().allow_methods(Any).allow_headers(Any);
    if args.cors_any {
        return base.allow_origin(Any);
    }
    let mut origins: Vec<HeaderValue> = ["app://obsidian.md", "capacitor://localhost", "http://localhost", "ionic://localhost"]
        .iter()
        .map(|o| HeaderValue::from_static(o))
        .collect();
    for o in &args.cors_origin {
        match HeaderValue::from_str(o.trim()) {
            Ok(v) if !o.trim().is_empty() => origins.push(v),
            _ => tracing::warn!("ignoring invalid --cors-origin {o:?}"),
        }
    }
    base.allow_origin(AllowOrigin::list(origins))
}

/// Reject requests without a valid user token before the handler (and its body extractors) run.
/// Failures are budgeted per client address so probing is cheap for us and loud in the log
/// only the first few times. A valid token always passes: the budget slows guessing, and a
/// correct credential is not a guess (behind a proxy many clients share one address, so one
/// device with a stale token must not lock the others out).
async fn require_user(
    State(state): State<SharedState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let ip = if state.serve.trust_proxy {
        req.headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next_back())
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(peer.ip())
    } else {
        peer.ip()
    };
    let query_token = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .map(str::to_string);
    let token = crate::config::extract_token(req.headers(), query_token.as_deref());
    let ok = token.map(|t| state.storage.registry().authenticate(&t).ok().flatten().is_some()).unwrap_or(false);
    if ok {
        return next.run(req).await;
    }
    let mut fails = state.limits.auth_fails.lock();
    if fails.blocked(&ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed attempts; try again later").into_response();
    }
    fails.record_failure(ip);
    // Path only, never the query (it may carry the token). At most the budget's worth of lines
    // per address and window, then the 429s above are silent.
    tracing::warn!(%ip, path = %req.uri().path(), "bad or missing user token");
    (StatusCode::UNAUTHORIZED, "bad token").into_response()
}

async fn index() -> &'static str {
    "samsyn is running. Connect the Obsidian Samsyn plugin to this URL."
}

async fn health(State(state): State<SharedState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "version": crate::VERSION,
            "vaults_loaded": state.docs.loaded_count(),
            "node_id": state.docs.node_id,
        })),
    )
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

//! The bench's tool interface over HTTP (replaces NATS since 14.11.2026).
//
//   GET  /health              → {ok, service, version, instance, memory}   (no token)
//   POST /call/<operation>    → the tool's JSON reply; 404 for an unknown one
//   *    /mcp                 → MCP over Streamable HTTP (list_operations,
//                               set_target, send_message – crate bench-tools)
//
// /call and /mcp need `Authorization: Bearer <token>`; the token lives in the
// data folder (api-token) and is created on first start. That matters even
// on 127.0.0.1: exec and fs are powerful, and any local web page could post
// to localhost. bench-mcp (stdio bridge for Claude Desktop) reads the same
// file on this machine; other machines get the token in their targets.json.

use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bench_tools::{BenchMcp, BoxFuture, Forward};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::ctx::Ctx;
use crate::dispatcher::{self, Reply};
use crate::events_log::EventLog;
use crate::observe::UiSink;
use crate::roots;
use crate::settings::BenchSettings;
use crate::telemetry::ServerStatus;

/// Shared state of the interface: the current tool context and what every
/// call needs for telemetry.
pub struct Api {
    ctx: RwLock<Arc<Ctx>>,
    instance: RwLock<String>,
    settings: Arc<RwLock<BenchSettings>>,
    memory: Result<Arc<oosmem::Memory>, String>,
    ui: UiSink,
    event_log: Arc<EventLog>,
    token: String,
}

impl Api {
    pub fn new(
        snapshot: &BenchSettings,
        settings: Arc<RwLock<BenchSettings>>,
        memory: Result<Arc<oosmem::Memory>, String>,
        ui: UiSink,
        event_log: Arc<EventLog>,
        token: String,
    ) -> Arc<Self> {
        let ctx = build_ctx(snapshot, &settings, &memory);
        Arc::new(Self {
            ctx: RwLock::new(ctx),
            instance: RwLock::new(snapshot.instance_name.clone()),
            settings,
            memory,
            ui,
            event_log,
            token,
        })
    }

    /// Apply saved settings (allowed roots, instance name).
    pub async fn reload(&self, s: &BenchSettings) {
        *self.ctx.write().await = build_ctx(s, &self.settings, &self.memory);
        *self.instance.write().await = s.instance_name.clone();
    }

    /// Run one operation in-process.
    pub async fn call(&self, subject: &str, args: Value) -> Reply {
        let ctx = self.ctx.read().await.clone();
        dispatcher::call(subject, args, &ctx, &self.ui, &self.event_log).await
    }
}

fn build_ctx(
    s: &BenchSettings,
    settings: &Arc<RwLock<BenchSettings>>,
    memory: &Result<Arc<oosmem::Memory>, String>,
) -> Arc<Ctx> {
    let root_paths: Vec<String> = s.roots.iter().map(|r| r.path.clone()).collect();
    Arc::new(Ctx {
        roots: Arc::new(roots::build_root_registry(&root_paths)),
        settings: settings.clone(),
        memory: memory.clone(),
    })
}

/// Read the token from `path`, or create a new random one (0600 on Unix).
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let token = text.trim();
        if token.len() >= 32 {
            return Ok(token.to_string());
        }
    }
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let token = data_encoding::HEXLOWER.encode(&bytes);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(path)?.write_all(token.as_bytes())?;
    Ok(token)
}

/// Bind and serve for the app's lifetime. Port and network mode are read once;
/// changing them needs a restart.
pub async fn serve(api: Arc<Api>, port: u16, network: bool) {
    let bind = if network { "0.0.0.0" } else { "127.0.0.1" };
    let shown = format!("http://{}:{port}", if network { "0.0.0.0" } else { "127.0.0.1" });
    let status = |up: bool, note: String| {
        api.ui.status(ServerStatus { name: "Schnittstelle".into(), url: note, connected: up });
    };

    // MCP: stateless JSON replies are enough for request/response tools. On
    // the network the Host check would reject the machine's own name; the
    // bearer token is the guard there.
    let mut config = StreamableHttpServerConfig::default();
    config.stateful_mode = false;
    config.json_response = true;
    if network {
        config = config.disable_allowed_hosts();
    }
    let mcp_api = api.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(BenchMcp::new(Arc::new(LocalForward(mcp_api.clone())), "bench")),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    let protected = Router::new()
        .route("/call/{subject}", post(call))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(api.clone(), auth));
    let app = Router::new().route("/health", get(health)).merge(protected).with_state(api.clone());

    let listener = match tokio::net::TcpListener::bind((bind, port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[bench] Schnittstelle {bind}:{port}: {e}");
            status(false, format!("{shown} – belegt? {e}"));
            return;
        }
    };
    eprintln!("[bench] Schnittstelle auf {shown} (/mcp, /call, /health)");
    status(true, format!("{shown}/mcp"));
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("[bench] Schnittstelle beendet: {e}");
        status(false, format!("{shown} – beendet: {e}"));
    }
}

async fn health(State(api): State<Arc<Api>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "bench",
        "version": env!("CARGO_PKG_VERSION"),
        "instance": api.instance.read().await.clone(),
        "memory": api.memory.is_ok(),
    }))
}

async fn call(State(api): State<Arc<Api>>, UrlPath(subject): UrlPath<String>, body: Bytes) -> Response {
    let args: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let json_type = [(header::CONTENT_TYPE, "application/json")];
    match api.call(&subject, args).await {
        Reply::Done { text } => (json_type, text).into_response(),
        Reply::Unknown(text) => (StatusCode::NOT_FOUND, json_type, text).into_response(),
    }
}

async fn auth(State(api): State<Arc<Api>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(token) if same(token.as_bytes(), api.token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing or wrong bearer token" }))).into_response(),
    }
}

/// Constant-time comparison, so the token cannot be guessed byte by byte.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// /mcp answers in-process; other instances are reached through bench-mcp.
struct LocalForward(Arc<Api>);

impl LocalForward {
    async fn check_target(api: &Api, target: Option<String>) -> Result<(), String> {
        let Some(target) = target.filter(|t| !t.is_empty()) else { return Ok(()) };
        let me = api.instance.read().await.clone();
        if target == me {
            Ok(())
        } else {
            Err(format!(
                "this endpoint is the bench \"{me}\"; reach \"{target}\" through bench-mcp (~/.config/bench-mcp/targets.json) or its own /mcp"
            ))
        }
    }
}

impl Forward for LocalForward {
    fn send(&self, target: Option<String>, subject: String, payload: Value) -> BoxFuture<Result<String, String>> {
        let api = self.0.clone();
        Box::pin(async move {
            Self::check_target(&api, target).await?;
            Ok(match api.call(&subject, payload).await {
                Reply::Done { text } | Reply::Unknown(text) => text,
            })
        })
    }

    fn set_target(&self, target: String) -> BoxFuture<Result<String, String>> {
        let api = self.0.clone();
        Box::pin(async move {
            Self::check_target(&api, Some(target)).await?;
            Ok("This endpoint serves only this bench.".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_vergleich() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }

    #[test]
    fn token_bleibt() {
        let dir = std::env::temp_dir().join(format!("bench-token-{}", uuid::Uuid::new_v4()));
        let path = dir.join("api-token");
        let first = load_or_create_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(load_or_create_token(&path).unwrap(), first);
        let _ = std::fs::remove_dir_all(dir);
    }
}

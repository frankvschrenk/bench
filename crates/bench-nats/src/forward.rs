//! Forwarding tool calls to a bench over HTTP.
//
// Targets: ~/.config/bench-nats/targets.json maps instance names to a bench:
//
//   { "macos": { "url": "http://127.0.0.1:4320" },
//     "linux": { "url": "http://192.168.8.135:4320", "token": "…" } }
//
// Without a token the local bench's token file is used (same user, same
// machine). The empty target "" is the local bench: BENCH_URL (default
// http://127.0.0.1:4320) with BENCH_TOKEN or the token file. The active
// target persists in ~/.config/bench-nats/session.json as before, so a
// rebuilt binary keeps the choice.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bench_mcp::{reply_timeout, BoxFuture, Forward};
use serde::Deserialize;

const DEFAULT_URL: &str = "http://127.0.0.1:4320";

#[derive(Clone, Debug, Default, Deserialize)]
struct Target {
    url: String,
    #[serde(default)]
    token: Option<String>,
}

pub struct HttpForward {
    http: reqwest::Client,
}

impl HttpForward {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }
}

fn config_dir() -> PathBuf {
    // Deliberately ~/.config on every platform (not Application Support), as
    // before: the session file stays where the old binary wrote it.
    dirs::home_dir().unwrap_or_default().join(".config").join("bench-nats")
}

/// Token of the bench on this machine: BENCH_TOKEN, else the bench's token
/// file (BENCH_DATA_DIR or the platform data folder + com.onisin.bench).
fn local_token() -> Option<String> {
    if let Ok(t) = std::env::var("BENCH_TOKEN") {
        return Some(t.trim().to_string());
    }
    let dir = std::env::var_os("BENCH_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("com.onisin.bench")))?;
    std::fs::read_to_string(dir.join("api-token")).ok().map(|t| t.trim().to_string())
}

async fn load_targets() -> BTreeMap<String, Target> {
    tokio::fs::read(config_dir().join("targets.json"))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

async fn load_active() -> String {
    match tokio::fs::read_to_string(config_dir().join("session.json")).await {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("target").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default(),
        Err(_) => std::env::var("BENCH_DEFAULT_TARGET").unwrap_or_default(),
    }
}

async fn save_active(target: &str) -> std::io::Result<()> {
    let dir = config_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let body = serde_json::json!({ "target": target });
    tokio::fs::write(dir.join("session.json"), body.to_string()).await
}

/// Resolve a target name to (url, token).
async fn resolve(name: &str) -> Result<(String, Option<String>), String> {
    let targets = load_targets().await;
    if let Some(t) = targets.get(name) {
        let token = t.token.clone().or_else(local_token);
        return Ok((t.url.trim_end_matches('/').to_string(), token));
    }
    if name.is_empty() {
        let url = std::env::var("BENCH_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
        return Ok((url.trim_end_matches('/').to_string(), local_token()));
    }
    let known: Vec<&String> = targets.keys().collect();
    Err(format!(
        "unknown target \"{name}\" – known: {known:?} (configure in {})",
        config_dir().join("targets.json").display()
    ))
}

impl Forward for HttpForward {
    fn send(&self, target: Option<String>, subject: String, payload: serde_json::Value) -> BoxFuture<Result<String, String>> {
        let http = self.http.clone();
        Box::pin(async move {
            let name = match target {
                Some(t) => t,
                None => load_active().await,
            };
            let (url, token) = resolve(&name).await?;
            let mut req = http
                .post(format!("{url}/call/{subject}"))
                .timeout(reply_timeout(&subject, &payload))
                .json(&payload);
            if let Some(token) = token {
                req = req.bearer_auth(token);
            }
            let resp = req.send().await.map_err(|e| format!("bench at {url} not reachable: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.map_err(|e| format!("reading reply from {url}: {e}"))?;
            match status.as_u16() {
                401 => Err(format!("bench at {url} rejected the token (Einstellungen → Schnittstelle)")),
                // 200 = result, 404/422 = the bench's own {"error": …} reply.
                _ => Ok(text),
            }
        })
    }

    fn set_target(&self, target: String) -> BoxFuture<Result<String, String>> {
        Box::pin(async move {
            resolve(&target).await?;
            save_active(&target).await.map_err(|e| format!("cannot persist session: {e}"))?;
            Ok(if target.is_empty() {
                "Subsequent calls go to the default bench.".to_string()
            } else {
                format!("Subsequent calls go to the \"{target}\" bench instance.")
            })
        })
    }
}

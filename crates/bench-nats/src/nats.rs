//! NATS transport details shared by the message tools: the session
//! file that pins the active bench target, subject routing, and a lazily
//! established NATS client.
//!
//! Why lazy: Claude.app spawns this process; it must come up and answer
//! list_operations even when no NATS server is reachable yet. Only the
//! message tools touch the network, and they surface a connect error in
//! their reply rather than failing at startup.

use std::path::PathBuf;
use std::time::Duration;

use async_nats::Client;
use tokio::sync::Mutex;

/// NATS server URL, overridable via BENCH_NATS_URL.
fn nats_url() -> String {
    std::env::var("BENCH_NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string())
}

/// Path of the per-user session file. Deliberately ~/.config (not the
/// macOS-native Application Support dir) so a freshly built binary reads
/// the very same target the running one already wrote.
fn session_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config").join("bench-nats").join("session.json")
}

/// The active bench instance name, or "" for "any bench".
pub async fn load_target() -> String {
    match tokio::fs::read_to_string(session_path()).await {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("target").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default(),
        // No session yet -> fall back to the boot default if one is set.
        Err(_) => std::env::var("BENCH_DEFAULT_TARGET").unwrap_or_default(),
    }
}

/// Persist the active target, creating the config dir if needed.
pub async fn save_target(target: &str) -> std::io::Result<()> {
    let path = session_path();
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let body = serde_json::json!({ "target": target });
    let text = format!("{}\n", serde_json::to_string_pretty(&body).unwrap());
    tokio::fs::write(&path, text).await
}

/// Build the full subject. With a target set, bench's dispatcher answers
/// only on the prefixed form (e.g. "macos.bench.fs.read"); without one,
/// any bench subscribed to "bench.>" replies.
pub fn build_subject(subject: &str, target: &str) -> String {
    if target.is_empty() {
        subject.to_string()
    } else {
        format!("{target}.{subject}")
    }
}

/// Lazily established, cached NATS client. async-nats reconnects on its
/// own once connected, so a single successful connect is reused for the
/// life of the process.
pub struct NatsHolder {
    client: Mutex<Option<Client>>,
}

impl NatsHolder {
    pub fn new() -> Self {
        Self { client: Mutex::new(None) }
    }

    /// Return the cached client, connecting on first use. The 5 s connect
    /// timeout mirrors the TS original; the 30 s request timeout is the
    /// default applied to every send_message round-trip.
    pub async fn client(&self) -> Result<Client, String> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let url = nats_url();
        let client = async_nats::ConnectOptions::new()
            .connection_timeout(Duration::from_secs(5))
            .request_timeout(Some(Duration::from_secs(30)))
            .connect(&url)
            .await
            .map_err(|e| format!("cannot connect to {url}: {e}"))?;
        *guard = Some(client.clone());
        Ok(client)
    }
}

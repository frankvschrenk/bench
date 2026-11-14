//! Observability surface: the bridge to the window, the OTLP receiver that
//! feeds it, and the reload loop that applies saved settings.
//
// Ported from the Tauri bench. The mapping:
//   - UiSink replaces the Tauri event bus ("tool-event" / "log-record" /
//     "server-status"): events go over a channel the egui app drains each
//     frame, and every send wakes the window with request_repaint. The channel
//     buffers on its own, so the old buffer-until-ready dance is gone.
//   - AppState keeps the live settings snapshot and the reconnect poke;
//     save_settings is a plain method the window calls instead of a command.
//   - supervisor() applies saved settings to the running tool interface.
//   - the OTLP receiver binds once, in boot, so changing otlpPort still needs
//     a restart.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};

use crate::settings::{self, BenchSettings};
use crate::telemetry::{LogRecord, ServerStatus, ToolEvent};

// ─── Window bridge ──────────────────────────────────────────────

/// One update for the window.
pub enum UiEvent {
    Tool(ToolEvent),
    Log(LogRecord),
    Status(ServerStatus),
}

/// Sending half of the window channel. Cheap to clone into every task.
#[derive(Clone)]
pub struct UiSink {
    tx: std_mpsc::Sender<UiEvent>,
    ctx: egui::Context,
}

impl UiSink {
    pub fn new(ctx: egui::Context) -> (Self, std_mpsc::Receiver<UiEvent>) {
        let (tx, rx) = std_mpsc::channel();
        (Self { tx, ctx }, rx)
    }

    fn send(&self, event: UiEvent) {
        // A closed channel means the window is gone; nothing left to show.
        if self.tx.send(event).is_ok() {
            self.ctx.request_repaint();
        }
    }

    pub fn tool(&self, event: ToolEvent) {
        self.send(UiEvent::Tool(event));
    }

    pub fn log(&self, record: LogRecord) {
        self.send(UiEvent::Log(record));
    }

    pub fn status(&self, status: ServerStatus) {
        self.send(UiEvent::Status(status));
    }
}

// ─── App state ──────────────────────────────────────────────────

/// Shared state between the window and the supervisor.
pub struct AppState {
    /// Live settings snapshot. Shared with every tool Ctx so pg.* sees the
    /// freshest DSN the moment save_settings writes it, without a reconnect.
    pub settings: Arc<RwLock<BenchSettings>>,
    /// Where save_settings persists (settings.json in the data dir).
    settings_path: PathBuf,
    /// Pokes the supervisor to apply the saved settings.
    reconnect_tx: mpsc::Sender<()>,
}

impl AppState {
    pub fn new(
        settings: Arc<RwLock<BenchSettings>>,
        settings_path: PathBuf,
        reconnect_tx: mpsc::Sender<()>,
    ) -> Self {
        Self {
            settings,
            settings_path,
            reconnect_tx,
        }
    }

    /// Persist updated settings, update the live snapshot, and apply them.
    /// Ports, network mode and the memory folder take effect on the next start.
    pub async fn save_settings(&self, settings: BenchSettings) -> Result<(), String> {
        settings::save(&self.settings_path, &settings)?;
        // Update the shared snapshot first so pg.* tools observe the new DSN
        // immediately.
        *self.settings.write().await = settings;
        // Best-effort poke: a full channel means a reload is already queued.
        let _ = self.reconnect_tx.try_send(());
        Ok(())
    }
}

// ─── Reload on save ─────────────────────────────────────────────

/// Apply saved settings for the app's lifetime: every poke from save_settings
/// rebuilds the tool context (allowed roots, instance name). Port, network
/// mode and the memory folder are bound at boot and need a restart. Returns
/// when all senders are dropped (app shutdown).
pub async fn supervisor(api: Arc<crate::api::Api>, state: Arc<AppState>, mut reload_rx: mpsc::Receiver<()>) {
    while reload_rx.recv().await.is_some() {
        let snapshot = state.settings.read().await.clone();
        api.reload(&snapshot).await;
    }
}

// ─── OTLP HTTP receiver ────────────────────────────────────────
//
// oosl ships structured logs as OTLP/HTTP-JSON; we map them to LogRecord and
// push to the window's Logs view. A minimal hand-rolled HTTP/1.1 reader keeps
// the dependency surface flat (no axum/hyper) for what is a localhost-only,
// best-effort sink: like the Bun receiver, it answers 200 to everything and
// silently drops anything it can't parse.

/// Bind the OTLP receiver on the given port for the app's lifetime. Called once
/// at boot; a port change needs a restart.
pub fn start_otlp(port: u16, ui: UiSink) {
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{port}");
        let listener = match TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("[bench] OTLP bind {addr} failed: {err}");
                return;
            }
        };
        eprintln!("[bench] OTLP receiver on http://{addr}");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let ui = ui.clone();
            tokio::spawn(async move { serve_otlp(stream, &ui).await });
        }
    });
}

async fn serve_otlp(mut stream: TcpStream, ui: &UiSink) {
    let Some((head, body)) = read_request(&mut stream).await else {
        return;
    };

    // Answer 200 to everything before doing any work, exactly like the Bun
    // receiver (it returned 200 for non-POST and on parse errors too).
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
    let _ = stream.shutdown().await;

    if !head.starts_with("POST") || body.is_empty() {
        return;
    }
    if let Ok(json) = serde_json::from_slice::<Value>(&body) {
        for record in parse_otlp_logs(&json) {
            ui.log(record);
        }
    }
}

// Read one HTTP/1.1 request: headers up to CRLFCRLF, then Content-Length bytes
// of body. Returns (header-block, body) or None on a closed/oversized request.
async fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    const HEADER_CAP: usize = 64 * 1024;
    const BODY_CAP: usize = 16 * 1024 * 1024;

    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    let header_end = loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > HEADER_CAP {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = content_length(&head).unwrap_or(0).min(BODY_CAP);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some((head, body))
}

fn content_length(head: &str) -> Option<usize> {
    head.split("\r\n").find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// Map an OTLP/JSON logs payload to LogRecords, mirroring the Bun startOtlp:
// service.name from the resource, log.source from the record attributes, the
// remaining attributes flattened into fields, body.stringValue as the message.
fn parse_otlp_logs(body: &Value) -> Vec<LogRecord> {
    let mut out = Vec::new();
    for rl in array(body, "resourceLogs") {
        let resource_attrs = rl
            .get("resource")
            .map(|r| array(r, "attributes"))
            .unwrap_or_default();
        let service = str_attr(&resource_attrs, "service.name").unwrap_or_else(|| "unknown".into());

        for sl in array(&rl, "scopeLogs") {
            for lr in array(&sl, "logRecords") {
                let attrs = array(&lr, "attributes");
                let source = str_attr(&attrs, "log.source").unwrap_or_default();

                let mut fields = Map::new();
                for attr in &attrs {
                    let key = attr.get("key").and_then(Value::as_str).unwrap_or("");
                    if key.is_empty() || key == "log.source" || key == "service.name" {
                        continue;
                    }
                    if let Some(value) = flatten_value(attr.get("value")) {
                        fields.insert(key.to_string(), value);
                    }
                }

                let ts = lr
                    .get("timeUnixNano")
                    .and_then(as_u64)
                    .map(nanos_to_iso)
                    .unwrap_or_else(now_iso);
                let level = lr
                    .get("severityText")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .to_lowercase();
                let message = lr
                    .get("body")
                    .and_then(|b| b.get("stringValue"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                out.push(LogRecord {
                    ts,
                    level,
                    service: service.clone(),
                    source,
                    message,
                    fields: (!fields.is_empty()).then_some(fields),
                });
            }
        }
    }
    out
}

fn array(value: &Value, key: &str) -> Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn str_attr(attrs: &[Value], key: &str) -> Option<String> {
    attrs.iter().find_map(|a| {
        (a.get("key").and_then(Value::as_str) == Some(key))
            .then(|| a.get("value").and_then(|v| v.get("stringValue")).and_then(Value::as_str))
            .flatten()
            .map(str::to_string)
    })
}

// OTLP AnyValue → the first scalar present, matching the Bun
// `stringValue ?? intValue ?? boolValue`. The JSON type is preserved.
fn flatten_value(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    for key in ["stringValue", "intValue", "boolValue"] {
        if let Some(inner) = value.get(key) {
            if !inner.is_null() {
                return Some(inner.clone());
            }
        }
    }
    None
}

// timeUnixNano is a proto int64, which OTLP/JSON encodes as a string; accept a
// JSON number too for lenience.
fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn nanos_to_iso(nanos: u64) -> String {
    chrono::DateTime::from_timestamp_millis((nanos / 1_000_000) as i64)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(now_iso)
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

//! Observability surface: the bridge to the window, the consumers that feed
//! it, and the NATS reconnect supervisor.
//
// Ported from the Tauri bench. The mapping:
//   - UiSink replaces the Tauri event bus ("tool-event" / "log-record" /
//     "server-status"): events go over a channel the egui app drains each
//     frame, and every send wakes the window with request_repaint. The channel
//     buffers on its own, so the old buffer-until-ready dance is gone.
//   - AppState keeps the live settings snapshot and the reconnect poke;
//     save_settings is a plain method the window calls instead of a command.
//   - supervisor() owns the live NATS connections for the app's lifetime and
//     rebuilds them on every save.
//   - the OTLP receiver binds once, in boot, so changing otlpPort still needs
//     a restart.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use async_nats::{Client, ConnectOptions, Event};
use chrono::{SecondsFormat, Utc};
use futures::StreamExt;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;

use crate::ctx::Ctx;
use crate::dispatcher::{self, DEBUG_SUBJECT};
use crate::events_log::EventLog;
use crate::roots;
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

    fn tool(&self, event: ToolEvent) {
        self.send(UiEvent::Tool(event));
    }

    fn log(&self, record: LogRecord) {
        self.send(UiEvent::Log(record));
    }

    fn status(&self, status: ServerStatus) {
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
    /// Pokes the supervisor to tear down and rebuild the NATS connections.
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

    /// Persist updated settings, update the live snapshot, and trigger a NATS
    /// reconnect. otlpPort changes take effect on the next app start.
    pub async fn save_settings(&self, settings: BenchSettings) -> Result<(), String> {
        settings::save(&self.settings_path, &settings)?;
        // Update the shared snapshot first so pg.* tools observe the new DSN
        // immediately, even before the reconnect finishes.
        *self.settings.write().await = settings;
        // Best-effort poke: a full channel means a reconnect is already queued.
        let _ = self.reconnect_tx.try_send(());
        Ok(())
    }
}

// ─── Reconnect supervisor ─────────────────────────────────────

/// Everything the supervisor needs that doesn't change across reconnects.
pub struct Boot {
    pub ui: UiSink,
    pub state: Arc<AppState>,
    pub event_log: Arc<EventLog>,
}

// The live connections + subscription loops of one connect cycle. Dropped and
// rebuilt on every reconnect.
struct Live {
    clients: Vec<Client>,
    tasks: Vec<JoinHandle<()>>,
}

impl Live {
    async fn shutdown(self) {
        for task in &self.tasks {
            task.abort();
        }
        for client in self.clients {
            // drain() flushes in-flight replies before closing, so a call mid
            // handler still gets its answer out before the socket goes away.
            let _ = client.drain().await;
        }
    }
}

/// Own the NATS connections for the app's lifetime. Connects once, then rebuilds
/// on every poke from save_settings. Returns when all senders are dropped
/// (app shutdown).
pub async fn supervisor(boot: Boot, mut reconnect_rx: mpsc::Receiver<()>) {
    let mut live = connect_all(&boot).await;
    while reconnect_rx.recv().await.is_some() {
        live.shutdown().await;
        live = connect_all(&boot).await;
    }
    live.shutdown().await;
}

// Read the current settings, connect to every server, build the tool Ctx on the
// first connected client, wire the responder subscriptions and the debug
// consumer. Faithful to the Bun doConnect: an empty/unconnectable server set
// leaves the bench idle rather than failing.
async fn connect_all(boot: &Boot) -> Live {
    let snapshot = boot.state.settings.read().await.clone();
    let root_paths: Vec<String> = snapshot.roots.iter().map(|r| r.path.clone()).collect();
    let roots = Arc::new(roots::build_root_registry(&root_paths));

    let mut clients = Vec::new();
    for srv in &snapshot.servers {
        match connect_one(boot, srv).await {
            Some(client) => {
                eprintln!("[bench] connected to {} ({})", srv.name, srv.url);
                boot.ui.status(ServerStatus {
                    name: srv.name.clone(),
                    url: srv.url.clone(),
                    connected: true,
                });
                clients.push(client);
            }
            None => {
                eprintln!("[bench] connect failed for {} ({})", srv.name, srv.url);
                boot.ui.status(ServerStatus {
                    name: srv.name.clone(),
                    url: srv.url.clone(),
                    connected: false,
                });
            }
        }
    }

    if clients.is_empty() {
        eprintln!("[bench] no NATS servers connected — dispatcher idle (Einstellungen → NATS-Server)");
        return Live {
            clients,
            tasks: Vec::new(),
        };
    }

    // First connected client is the outbound path for memory/task → oosmem and
    // the source for the debug consumer (the Bun connectLogNats used servers[0]).
    let ctx = Arc::new(Ctx {
        roots: roots.clone(),
        settings: boot.state.settings.clone(),
        nats: clients[0].clone(),
    });

    let mut tasks = Vec::new();
    for client in &clients {
        tasks.extend(dispatcher::subscribe_responders(
            client.clone(),
            snapshot.instance_name.clone(),
            ctx.clone(),
            boot.event_log.clone(),
        ));
    }
    tasks.push(spawn_debug_consumer(clients[0].clone(), boot.ui.clone()));

    Live { clients, tasks }
}

// Connect to one server, attaching an event callback that keeps the window's
// status surface live across drops/reconnects (the Bun nc.closed() analog).
async fn connect_one(boot: &Boot, srv: &settings::NatsServer) -> Option<Client> {
    let ui = boot.ui.clone();
    let name = srv.name.clone();
    let url = srv.url.clone();

    let opts = ConnectOptions::new().event_callback(move |event| {
        let ui = ui.clone();
        let name = name.clone();
        let url = url.clone();
        async move {
            let connected = match event {
                Event::Connected => true,
                Event::Disconnected => false,
                _ => return,
            };
            ui.status(ServerStatus { name, url, connected });
        }
    });

    opts.connect(srv.url.as_str()).await.ok()
}

// Subscribe to oos.bench.debug and forward each ToolEvent to the window. Lets
// the Tools view work even when bench-nats isn't in the loop, and shows events
// from other instances on the same bus. Faithful to the Bun connectLogNats.
fn spawn_debug_consumer(client: Client, ui: UiSink) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut sub = match client.subscribe(DEBUG_SUBJECT).await {
            Ok(sub) => sub,
            Err(err) => {
                eprintln!("[bench] subscribe {DEBUG_SUBJECT} failed: {err}");
                return;
            }
        };
        while let Some(msg) = sub.next().await {
            // Ignore malformed events, like the Bun try/catch.
            if let Ok(event) = serde_json::from_slice::<ToolEvent>(&msg.payload) {
                ui.tool(event);
            }
        }
    })
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

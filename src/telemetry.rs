//! Wire + IPC data shapes for the observability surface.
//
// ToolEvent is also stored as JSON in the audit log (events.db); the camelCase
// rename keeps it byte-compatible with what the Bun/Tauri benches wrote.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One tool-call event published on oos.bench.debug after each handled request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEvent {
    pub id: String,
    /// The fully-resolved subject, e.g. "bench.fs.read".
    pub tool: String,
    pub args: Value,
    pub duration_ms: u64,
    /// "ok" or "error" — mirrors the Bun ToolResult.isError split.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// Byte length of the JSON reply. The Bun bench used the JS string length
    /// (UTF-16 units); for the mostly-ASCII JSON replies the two agree.
    pub result_size: usize,
    pub ts: String,
}

/// One structured log record, mapped from an OTLP log record on its way to the
/// window's Logs tab.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogRecord {
    pub ts: String,
    /// "error" | "warn" | "info" | "debug" — lowercased OTLP severityText.
    pub level: String,
    pub service: String,
    pub source: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fields: Option<Map<String, Value>>,
}

/// Status of one local service (tool interface, memory store), pushed to the
/// window so the side bar can show what is running. `url` is the hover text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: String,
    pub url: String,
    pub connected: bool,
}

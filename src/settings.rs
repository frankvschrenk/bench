//! bench settings — persisted as settings.json in the data dir and read at
//! boot to configure the dispatcher.
//
// The file keeps the tauri-plugin-store layout of the Tauri bench
// ({"settings": {...}}) in the same folder (com.onisin.bench), so the egui
// bench picks up an existing configuration unchanged. The camelCase serde
// names keep the JSON byte-compatible with what the old settings UI wrote.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AllowedRoot {
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BenchSettings {
    /// Human-readable name for this instance, e.g. "macos". Shown in the
    /// window and in /health; bench-mcp picks instances by its own targets.
    pub instance_name: String,
    /// Port of the tool interface (/mcp, /call/<operation>, /health).
    pub api_port: u16,
    /// false = only this machine (127.0.0.1); true = all interfaces, e.g. for a
    /// test machine Claude reaches through bench-mcp on the Mac. The bearer
    /// token is required either way.
    pub api_network: bool,
    /// Folder of the memory store (oosmem). Empty = <data dir>/oosmem, the
    /// folder the former oosmem service used, so the existing store is kept.
    pub memory_dir: String,
    /// Ollama URL for episode summaries. Empty = summaries off.
    pub llm_url: String,
    /// Ollama model for episode summaries.
    pub llm_model: String,
    pub roots: Vec<AllowedRoot>,
    pub otlp_port: u16,
    /// Host-only PostgreSQL DSN, no dbname segment. bench appends "/<appDatabase>"
    /// for pg.* tools. Empty disables the pg.* tools.
    pub dsn: String,
    /// Application database name the pg.* tools operate on (typically "onisin").
    pub app_database: String,
    /// Issuer-URL des Dev-IdP, z. B. "http://192.168.1.71:5556". Leer = "http://localhost:5556"
    /// (nur dieser Rechner). Steht ein anderer Host als localhost darin, lauscht der IdP
    /// im Netz (0.0.0.0) – so erreichen ihn Server und Browser auf anderen Rechnern.
    pub iam_issuer: String,
}

impl Default for BenchSettings {
    // otlpPort defaults to the OTLP/HTTP convention 4318; everything else is
    // empty until the user fills Settings. #[serde(default)] above pulls each
    // missing field from here, so rows written by older versions never crash.
    fn default() -> Self {
        Self {
            instance_name: String::new(),
            api_port: 4320,
            api_network: false,
            memory_dir: String::new(),
            llm_url: "http://127.0.0.1:11434".into(),
            llm_model: default_llm_model().into(),
            roots: Vec::new(),
            otlp_port: 4318,
            dsn: String::new(),
            app_database: String::new(),
            iam_issuer: String::new(),
        }
    }
}

/// MLX models only run on Apple silicon; elsewhere the plain Ollama build.
fn default_llm_model() -> &'static str {
    if cfg!(target_os = "macos") {
        oosmem::DEFAULT_LLM_MODEL
    } else {
        "gemma4:e4b"
    }
}

/// Folder of the memory store: the setting, else <platform data dir>/oosmem.
pub fn memory_dir(s: &BenchSettings) -> std::path::PathBuf {
    let dir = s.memory_dir.trim();
    if !dir.is_empty() {
        return std::path::PathBuf::from(dir);
    }
    dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("oosmem")
}

/// Load settings from settings.json, falling back to defaults when the file or
/// the "settings" key is absent (first boot) or malformed.
pub fn load(path: &Path) -> BenchSettings {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|mut file| file.get_mut("settings").map(Value::take))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Persist settings to the file the boot path reads. Other top-level keys a
/// tauri-plugin-store file may carry are kept.
pub fn save(path: &Path, settings: &BenchSettings) -> Result<(), String> {
    let mut file = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    file["settings"] = serde_json::to_value(settings).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

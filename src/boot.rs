//! Backend boot: settings, memory store, event log, IdP, OTLP receiver and the
//! tool interface (HTTP: /mcp, /call). Counterpart of the Tauri setup() hook.

use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::{mpsc, RwLock};

use crate::events_log::EventLog;
use crate::iam::{self, IamState};
use crate::observe::{self, AppState, UiEvent, UiSink};
use crate::settings::{self, BenchSettings};
use crate::telemetry::ServerStatus;

/// How often the memory store rewrites its header (events are durable per append).
const MEMORY_SNAPSHOT_EVERY: Duration = Duration::from_secs(30 * 60);

/// Handles the window keeps to talk to the running backend.
pub struct Backend {
    pub state: Arc<AppState>,
    pub iam: Arc<IamState>,
    pub events: Receiver<UiEvent>,
    /// Settings as loaded at boot; the window keeps its own copy from here.
    pub settings: BenchSettings,
    /// Bearer token of the tool interface (shown in Einstellungen → Schnittstelle).
    pub api_token: String,
}

pub fn start(runtime: &Runtime, ctx: egui::Context) -> Backend {
    let data_dir = crate::data_dir();
    let _ = std::fs::create_dir_all(&data_dir);

    // Settings are loaded once and shared (RwLock) so the pg.* tools and the
    // reload loop read the same live snapshot. Ports, network mode and the
    // memory folder are captured up front: they bind once at boot.
    let settings_path = data_dir.join("settings.json");
    let loaded = settings::load(&settings_path);
    let shared = Arc::new(RwLock::new(loaded.clone()));

    // save_settings → reload channel.
    let (reload_tx, reload_rx) = mpsc::channel::<()>(8);
    let state = Arc::new(AppState::new(shared.clone(), settings_path, reload_tx));
    let (ui, events) = UiSink::new(ctx);

    // Dev-IdP (ehemals services/oosiam): Schlüssel im Datenordner der Bench,
    // damit die kid über Neustarts gleich bleibt.
    let iam_state = Arc::new(IamState::default());
    let key_path = data_dir.join("iam").join("signing-key.pem");
    runtime.spawn(iam::start(loaded.clone(), key_path, iam_state.clone()));

    // Memory store (formerly the oosmem service), opened in-process.
    let memory = open_memory(&loaded, &ui);
    if let Ok(mem) = &memory {
        let mem = mem.clone();
        runtime.spawn(async move {
            let mut every = tokio::time::interval(MEMORY_SNAPSHOT_EVERY);
            every.tick().await;
            loop {
                every.tick().await;
                if let Err(e) = mem.snapshot() {
                    eprintln!("[bench] oosmem snapshot: {e}");
                }
            }
        });
    }

    let token = match crate::api::load_or_create_token(&data_dir.join("api-token")) {
        Ok(t) => t,
        Err(e) => {
            // Without a stored token the interface still runs, with one that
            // only lives until the next start.
            eprintln!("[bench] api-token: {e} – Token nur für diese Sitzung");
            uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()
        }
    };

    // Local SQLite audit log, then OTLP and the tool interface.
    let db_path = data_dir.join("events.db");
    let boot_state = state.clone();
    let boot_token = token.clone();
    let window_settings = loaded.clone();
    runtime.spawn(async move {
        let event_log = Arc::new(EventLog::open(&db_path).await);
        observe::start_otlp(loaded.otlp_port, ui.clone());
        let api = crate::api::Api::new(&loaded, shared, memory, ui, event_log, boot_token);
        tokio::spawn(crate::api::serve(api.clone(), loaded.api_port, loaded.api_network));
        observe::supervisor(api, boot_state, reload_rx).await;
    });

    Backend {
        state,
        iam: iam_state,
        events,
        settings: window_settings,
        api_token: token,
    }
}

fn open_memory(s: &BenchSettings, ui: &UiSink) -> Result<Arc<oosmem::Memory>, String> {
    let dir = settings::memory_dir(s);
    let cfg = oosmem::MemoryConfig {
        data_dir: dir.to_string_lossy().into_owned(),
        dim: oosmem::DEFAULT_DIM,
        llm_url: s.llm_url.trim().to_string(),
        llm_model: s.llm_model.trim().to_string(),
        llm_timeout: Duration::from_secs(60),
    };
    match oosmem::Memory::open(cfg) {
        Ok(mem) => {
            ui.status(ServerStatus { name: "Gedächtnis".into(), url: dir.display().to_string(), connected: true });
            Ok(Arc::new(mem))
        }
        Err(e) => {
            let msg = format!("{}: {e}", dir.display());
            eprintln!("[bench] oosmem: {msg}");
            ui.status(ServerStatus { name: "Gedächtnis".into(), url: msg.clone(), connected: false });
            Err(msg)
        }
    }
}

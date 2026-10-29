//! Backend boot: settings, event log, IdP, OTLP receiver and the NATS
//! supervisor. Counterpart of the Tauri setup() hook.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::{mpsc, RwLock};

use crate::events_log::EventLog;
use crate::iam::{self, IamState};
use crate::observe::{self, AppState, UiEvent, UiSink};
use crate::settings::{self, BenchSettings};

/// Handles the window keeps to talk to the running backend.
pub struct Backend {
    pub state: Arc<AppState>,
    pub iam: Arc<IamState>,
    pub events: Receiver<UiEvent>,
    /// Settings as loaded at boot; the window keeps its own copy from here.
    pub settings: BenchSettings,
}

pub fn start(runtime: &Runtime, ctx: egui::Context) -> Backend {
    let data_dir = crate::data_dir();
    let _ = std::fs::create_dir_all(&data_dir);

    // Settings are loaded once and shared (RwLock) so the pg.* tools and the
    // reconnect supervisor read the same live snapshot. The OTLP port is
    // captured up front: the receiver binds once at boot, so a port change
    // needs a restart.
    let settings_path = data_dir.join("settings.json");
    let loaded = settings::load(&settings_path);
    let otlp_port = loaded.otlp_port;
    let shared = Arc::new(RwLock::new(loaded.clone()));

    // save_settings → supervisor reconnect channel.
    let (reconnect_tx, reconnect_rx) = mpsc::channel::<()>(8);
    let state = Arc::new(AppState::new(shared, settings_path, reconnect_tx));
    let (ui, events) = UiSink::new(ctx);

    // Dev-IdP (ehemals services/oosiam): Schlüssel im Datenordner der Bench,
    // damit die kid über Neustarts gleich bleibt.
    let iam_state = Arc::new(IamState::default());
    let key_path = data_dir.join("iam").join("signing-key.pem");
    runtime.spawn(iam::start(loaded.clone(), key_path, iam_state.clone()));

    // Local SQLite audit log, then OTLP and the NATS supervisor.
    let db_path = data_dir.join("events.db");
    let boot_state = state.clone();
    runtime.spawn(async move {
        let event_log = Arc::new(EventLog::open(&db_path).await);
        observe::start_otlp(otlp_port, ui.clone());
        let boot = observe::Boot {
            ui,
            state: boot_state,
            event_log,
        };
        observe::supervisor(boot, reconnect_rx).await;
    });

    Backend {
        state,
        iam: iam_state,
        events,
        settings: loaded,
    }
}

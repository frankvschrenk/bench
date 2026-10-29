//! bench desktop app (egui + carbon-egui).
//
// bench's backend IS a NATS request-reply responder. Its tools touch the local
// filesystem, spawn processes and reach Postgres, so the dispatcher boots with
// the app and answers bench.* for the app's lifetime; the window is a status
// surface fed over a channel, not the data path.
//
// Port of the Tauri bench (onisin/apps/bench): the backend modules are carried
// over unchanged where they never touched Tauri; settings, the window bridge
// and the IdP commands were rewired for egui (see boot.rs and observe.rs).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod boot;
mod ctx;
mod db;
mod dispatcher;
mod error;
mod events_log;
mod iam;
mod observe;
mod roots;
mod settings;
mod telemetry;
mod tools;

use std::path::PathBuf;

const TITLE: &str = "Onisin OS - Bench";

/// Home directory as a string ("" if unknown). Replaces $HOME, which Windows
/// does not set.
pub(crate) fn home_dir() -> String {
    dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Data folder of the bench: settings.json, events.db, iam/. Same place the
/// Tauri bench used (app data dir + identifier com.onisin.bench), so an
/// existing installation keeps its settings and signing key. BENCH_DATA_DIR
/// points a second instance (tests, a demo next to the real bench) elsewhere.
pub(crate) fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BENCH_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("com.onisin.bench")
}

fn main() -> eframe::Result {
    // The backend runs on its own multi-threaded runtime; the window thread
    // only hands work to it and drains the result channels.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bench-rt")
        .build()
        .expect("start tokio runtime");

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id("com.onisin.bench")
        .with_inner_size([1100.0, 750.0])
        .with_min_inner_size([800.0, 500.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/256x256.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "com.onisin.bench",
        options,
        Box::new(move |cc| {
            carbon_egui::install(&cc.egui_ctx);
            let backend = boot::start(&runtime, cc.egui_ctx.clone());
            Ok(Box::new(app::BenchApp::new(runtime, backend)))
        }),
    )
}

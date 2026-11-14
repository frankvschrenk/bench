//! Shared context passed to every tool handler.
//
// Roots (the sandbox), a live settings snapshot (so pg.* tools see the
// freshest DSN without a restart) and the memory store that memory/handbook
// use. Rebuilt when the settings are saved (roots may change); the memory
// store is opened once at boot and shared.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::ToolError;
use crate::roots::RootRegistry;
use crate::settings::BenchSettings;

pub struct Ctx {
    pub roots: Arc<RootRegistry>,
    pub settings: Arc<RwLock<BenchSettings>>,
    /// The memory store, or why it could not be opened.
    pub memory: Result<Arc<oosmem::Memory>, String>,
}

impl Ctx {
    pub fn memory(&self) -> Result<&oosmem::Memory, ToolError> {
        self.memory
            .as_deref()
            .map_err(|e| ToolError::Msg(format!("Gedächtnis (oosmem) nicht verfügbar: {e}")))
    }
}

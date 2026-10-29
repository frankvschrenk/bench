//! Shared context passed to every tool handler.
//
// Mirrors the Bun Ctx: roots (the sandbox), a live settings snapshot (so pg.*
// tools see the freshest DSN without a restart), and the outbound NATS client
// that memory/task tools use to delegate to oosmem under oos.cmd.mem.*. Tools
// never own a NATS connection — the dispatcher does, and lends it here.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::roots::RootRegistry;
use crate::settings::BenchSettings;

pub struct Ctx {
    pub roots: Arc<RootRegistry>,
    pub settings: Arc<RwLock<BenchSettings>>,
    pub nats: async_nats::Client,
}

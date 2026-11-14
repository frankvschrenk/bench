//! oosmem — the episodic memory store of the bench (handbook entries, notes,
//! search), used in-process.
//
// Rust port of the Go service (onisin_old/apps/oosmem). The on-disk binary
// format is reproduced byte-for-byte so this build reads the existing store in
// place; the HNSW cache file is not ported (it was a rebuildable cache), and
// vector search is an exact in-RAM cosine scan rather than ANN -- the corpus
// is small enough that exact is both simpler and faster.
//
// Until 14.11.2026 this was a separate binary answering oos.cmd.mem.* over
// NATS. The bench now opens the store itself (`Memory::open`) and calls the
// same handlers directly (`Memory::call`), so neither NATS nor a second
// process is needed.

// dead_code is allowed on the foundational modules: they carry the full
// on-disk + vector vocabulary (enum mappings, format constants, the
// inverse-ref graph) faithfully ported from Go. Some of it no handler consumes
// yet, but it documents the format and seeds the graph-aware recall we may add
// later; pruning it now would just have to be re-added.
#[allow(dead_code)]
mod format;
mod payload;
mod server;
#[allow(dead_code)]
mod store;
mod synth;
#[allow(dead_code)]
mod vector;

pub use server::*;

/// Vector dimension for a fresh store (an existing store keeps its own).
pub const DEFAULT_DIM: u16 = 128;
/// Default summarizer model (Ollama). MLX models only exist on Apple silicon.
pub const DEFAULT_LLM_MODEL: &str = "gemma4:e4b-mlx";

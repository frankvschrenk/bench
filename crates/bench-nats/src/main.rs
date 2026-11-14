//! bench-nats -- stdio MCP server bridging Claude to a running bench
//! desktop app over NATS. Rust port of the Bun/TypeScript original
//! (apps/bench-nats in onisin_old).
//!
//! Why a faithful port and not a redesign: this process is the live MCP
//! transport Claude itself speaks through. The five tool names, their
//! schemas, the ~/.config/bench-nats/session.json file, and the subject
//! routing stay wire-identical so swapping the binary is invisible to
//! the client.
//!
//! Critical invariant: stdout carries the JSON-RPC protocol frames.
//! Nothing else may be written there -- any diagnostics go to stderr.

mod nats;
mod operations;
mod server;

use rmcp::{transport::stdio, ServiceExt};

use crate::server::BenchNats;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // serve() runs the MCP initialize handshake and dispatches tool
    // calls until the transport closes. NATS is connected lazily on the
    // first message tool, so startup never depends on a reachable bus.
    let service = BenchNats::new()
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP stdio serve failed: {e}"))?;
    service.waiting().await?;
    Ok(())
}

//! bench-nats -- stdio MCP server that bridges Claude Desktop to a running
//! bench. Claude starts it as a child process; every tool call goes over
//! HTTP to the chosen bench instance (`POST <url>/call/<operation>` with a
//! bearer token).
//!
//! Until 14.11.2026 the transport was NATS (hence the name). The tools
//! themselves live in the shared crate bench-mcp, which the bench also
//! serves directly at `/mcp` for clients that speak Streamable HTTP.
//!
//! Critical invariant: stdout carries the JSON-RPC protocol frames.
//! Nothing else may be written there -- any diagnostics go to stderr.

mod forward;

use std::sync::Arc;

use bench_mcp::BenchMcp;
use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let forward = Arc::new(forward::HttpForward::new());
    let service = BenchMcp::new(forward, "bench-nats")
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP stdio serve failed: {e}"))?;
    service.waiting().await?;
    Ok(())
}

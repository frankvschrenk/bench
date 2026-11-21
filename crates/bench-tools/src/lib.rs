//! The bench's MCP surface, shared by both ways in:
//!
//! - the bench itself serves it over Streamable HTTP (`/mcp`) and answers
//!   every call in-process;
//! - `bench-mcp` serves it over stdio for Claude Desktop and forwards each
//!   call over HTTP to the chosen bench instance.
//!
//! Until 14.11.2026 the transport was NATS. Tool names and argument shapes
//! (`list_operations`, `set_target`, `send_message {subject, payload,
//! target?}`) are unchanged, so existing habits and handbooks keep working;
//! only `watch_subject`/`unwatch_subject` are gone (they were NATS pub/sub).

pub mod operations;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;

pub use operations::manifest_json;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Where a call goes. The bench answers in-process, the bench-mcp bridge forwards over HTTP.
pub trait Forward: Send + Sync + 'static {
    /// Run one bench operation. `target` None = the active target. Ok carries
    /// the bench's JSON reply verbatim (a tool error is `{"error": …}` in
    /// there); Err is a transport failure (bench not reachable, bad token).
    fn send(&self, target: Option<String>, subject: String, payload: serde_json::Value) -> BoxFuture<Result<String, String>>;
    /// Pick the active target for later calls. Ok carries a note for Claude.
    fn set_target(&self, target: String) -> BoxFuture<Result<String, String>>;
}

/// Server instructions shown to the MCP client.
pub const INSTRUCTIONS: &str = "bench gives Claude files, search, exec, git, Postgres and the project memory on a \
running bench desktop app. Call list_operations to discover the bench surface, set_target to pick an instance, \
and send_message to run an operation.\n\
Project knowledge lives in oosmem, not in CLAUDE.md files. Before working in a repo or folder, and again after \
the conversation was summarized, call send_message bench.context.brief {repo: <path>}: it returns the repo \
handbook (current facts: map, build, quirks, conventions, open threads) and the active tasks with their latest \
notes. Keep it true: when something lasting changes, replace the entry with bench.handbook.set; log progress \
with bench.task.note.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTargetArgs {
    #[schemars(description = "Instance name, e.g. \"macos\" or \"linux\". Empty string = the default bench.")]
    pub target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageArgs {
    #[schemars(description = "Operation, e.g. \"bench.fs.read\" (see list_operations)")]
    pub subject: String,
    #[schemars(description = "JSON payload matching the operation's input schema")]
    pub payload: serde_json::Value,
    #[schemars(description = "Override the active target for this call only (e.g. \"linux\")")]
    pub target: Option<String>,
}

fn text_result(text: impl Into<String>, is_error: bool) -> CallToolResult {
    let content = vec![Content::text(text.into())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

fn json_text(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// The MCP handler. `name` is what the client sees as server name.
#[derive(Clone)]
pub struct BenchMcp {
    // Read only through the macro-generated dispatch.
    #[allow(dead_code)]
    tool_router: ToolRouter<BenchMcp>,
    forward: Arc<dyn Forward>,
    name: &'static str,
}

#[tool_router]
impl BenchMcp {
    pub fn new(forward: Arc<dyn Forward>, name: &'static str) -> Self {
        Self { tool_router: Self::tool_router(), forward, name }
    }

    #[tool(
        description = "List all operations the bench desktop app handles, with input/output signatures. Call once at the start of a task to discover what is available. Before working in a repo or folder, send bench.context.brief {repo} first: it returns the current handbook and active tasks for that repo."
    )]
    async fn list_operations(&self) -> Result<CallToolResult, McpError> {
        Ok(text_result(manifest_json(), false))
    }

    #[tool(
        description = "Set the active bench instance for this session (e.g. \"macos\" or \"linux\", as configured in targets.json of bench-mcp). All subsequent send_message calls go to this instance unless you pass a different target. Pass an empty string \"\" to reset to the default bench."
    )]
    async fn set_target(&self, Parameters(args): Parameters<SetTargetArgs>) -> Result<CallToolResult, McpError> {
        let target = args.target.trim().to_string();
        match self.forward.set_target(target.clone()).await {
            Ok(note) => Ok(text_result(
                json_text(serde_json::json!({
                    "ok": true,
                    "target": if target.is_empty() { "(default bench)".to_string() } else { target },
                    "note": note,
                })),
                false,
            )),
            Err(e) => Ok(text_result(json_text(serde_json::json!({ "error": e })), true)),
        }
    }

    #[tool(
        description = "Run one operation on a bench instance. Use list_operations to discover the operation and payload shape. The active target set via set_target is used automatically. Pass 'target' to override for a single call. Blocks until bench replies (timeout: 30 s, or the payload's timeout_seconds + 5 s, e.g. for bench.exec.exec). Start work in a repo with subject bench.context.brief, payload {repo: <path>}."
    )]
    async fn send_message(&self, Parameters(args): Parameters<SendMessageArgs>) -> Result<CallToolResult, McpError> {
        // Some MCP clients deliver an untyped `payload` as a JSON *string*.
        // Unwrap one level so bench always receives the bare object; a
        // genuine non-JSON string is left untouched.
        let payload = match args.payload {
            serde_json::Value::String(s) => {
                serde_json::from_str::<serde_json::Value>(&s).unwrap_or(serde_json::Value::String(s))
            }
            other => other,
        };
        let target = args.target.map(|t| t.trim().to_string());
        match self.forward.send(target.clone(), args.subject.clone(), payload).await {
            Ok(text) => Ok(text_result(text, false)),
            Err(e) => Ok(text_result(
                json_text(serde_json::json!({
                    "error": e,
                    "subject": args.subject,
                    "target": target.unwrap_or_else(|| "(active)".into()),
                })),
                true,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for BenchMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut implementation = Implementation::from_build_env();
        implementation.name = self.name.to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info = implementation;
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

/// How long to wait for a reply: what the caller asked the bench to wait
/// (`timeout_seconds`, capped at an hour) plus slack, at least 30 s.
pub fn reply_timeout(subject: &str, payload: &serde_json::Value) -> std::time::Duration {
    let secs = payload
        .get("timeout_seconds")
        .and_then(|v| v.as_u64())
        .or_else(|| subject.ends_with("exec.exec").then_some(60))
        .map(|s| s.min(3600) + 5)
        .unwrap_or(30);
    std::time::Duration::from_secs(secs.max(30))
}

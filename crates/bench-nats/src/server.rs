//! The MCP server: the BenchNats handler, its five tools, and the
//! ServerHandler wiring.
//!
//! The tool names, descriptions, and input shapes are kept identical to
//! the TypeScript original so the MCP client (Claude.app) sees no change
//! across the swap. Logical failures (NATS down, request timed out) come
//! back as a normal CallToolResult with is_error set, matching the old
//! behaviour; Err(McpError) is reserved for genuine protocol faults.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::nats::{build_subject, load_target, save_target, NatsHolder};
use crate::operations::manifest_json;

// --- tool argument schemas ----------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetTargetArgs {
    #[schemars(description = "Instance name, e.g. \"macos\" or \"linux\". Empty string = any bench.")]
    pub target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageArgs {
    #[schemars(description = "NATS subject suffix, e.g. \"bench.fs.read\"")]
    pub subject: String,
    #[schemars(description = "JSON payload matching the subject's input schema")]
    pub payload: serde_json::Value,
    #[schemars(description = "Override the active target for this call only (e.g. \"linux\")")]
    pub target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WatchSubjectArgs {
    #[schemars(description = "NATS subject to subscribe to, e.g. \"oos.pipeline.done.run_123\"")]
    pub subject: String,
    #[schemars(description = "Auto-unsubscribe after this many messages (default: 1)")]
    pub max_messages: Option<u64>,
    #[schemars(description = "bench instance name override")]
    pub target: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnwatchSubjectArgs {
    #[schemars(description = "The subscription_id returned by watch_subject")]
    pub subscription_id: String,
}

// --- helpers ------------------------------------------------------

/// One JSON text content block, the only result shape bench-nats emits.
fn text_result(text: impl Into<String>, is_error: bool) -> CallToolResult {
    let content = vec![Content::text(text.into())];
    if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// Compact JSON for a small object literal, for the error/ack replies.
fn json_text(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

static SUB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_subscription_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = SUB_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("watch_{millis}_{n}")
}

// --- the handler --------------------------------------------------

#[derive(Clone)]
pub struct BenchNats {
    // Read only through the macro-generated dispatch (the #[tool_handler]
    // ServerHandler impl), which rustc's dead-code pass cannot see, so it
    // would otherwise warn. The standalone smoke confirms tools dispatch.
    #[allow(dead_code)]
    tool_router: ToolRouter<BenchNats>,
    nats: Arc<NatsHolder>,
    /// Live watch_subject background tasks, keyed by subscription id.
    subs: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

#[tool_router]
impl BenchNats {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            nats: Arc::new(NatsHolder::new()),
            subs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tool(
        description = "List all subjects the bench desktop app handles over NATS, with input/output signatures. Call once at the start of a task to discover what is available."
    )]
    async fn list_operations(&self) -> Result<CallToolResult, McpError> {
        Ok(text_result(manifest_json(), false))
    }

    #[tool(
        description = "Set the active bench instance for this session. Use the instance name configured in bench Settings (e.g. \"macos\" or \"linux\"). All subsequent send_message calls will be routed to this instance unless you explicitly pass a different target. Pass an empty string \"\" to reset to any available bench."
    )]
    async fn set_target(
        &self,
        Parameters(args): Parameters<SetTargetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = args.target.trim().to_string();
        if let Err(e) = save_target(&target).await {
            return Ok(text_result(
                json_text(serde_json::json!({ "error": format!("cannot persist session: {e}") })),
                true,
            ));
        }
        let note = if target.is_empty() {
            "Subsequent calls will be routed to any available bench instance.".to_string()
        } else {
            format!("Subsequent calls will be routed to the \"{target}\" bench instance.")
        };
        Ok(text_result(
            json_text(serde_json::json!({
                "ok": true,
                "target": if target.is_empty() { "(any bench)".to_string() } else { target.clone() },
                "note": note,
            })),
            false,
        ))
    }

    #[tool(
        description = "Send a NATS Request-Reply message to a bench instance. Use list_operations to discover the subject and payload shape. The active target set via set_target is used automatically. Pass 'target' to override for a single call. Blocks until bench replies (timeout: 30 s)."
    )]
    async fn send_message(
        &self,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let active_target = match args.target {
            Some(t) => t.trim().to_string(),
            None => load_target().await,
        };
        let full_subject = build_subject(&args.subject, &active_target);

        let client = match self.nats.client().await {
            Ok(c) => c,
            Err(e) => return Ok(text_result(json_text(serde_json::json!({ "error": e })), true)),
        };

        // Some MCP clients deliver an untyped `payload` field as a JSON
        // *string* rather than a nested object. Unwrap one level of string
        // encoding so bench always receives the bare object its tool structs
        // expect; a double-encoded payload otherwise fails deserialization
        // ("invalid type: string, expected struct ..."). A genuine,
        // non-JSON string is left untouched.
        let payload_value = match args.payload {
            serde_json::Value::String(s) => {
                serde_json::from_str::<serde_json::Value>(&s).unwrap_or(serde_json::Value::String(s))
            }
            other => other,
        };
        let payload = serde_json::to_vec(&payload_value).unwrap_or_default();
        match client.request(full_subject.clone(), payload.into()).await {
            // bench replies with bare result JSON; hand it back verbatim.
            Ok(msg) => {
                let text = String::from_utf8_lossy(&msg.payload).to_string();
                Ok(text_result(text, false))
            }
            Err(e) => Ok(text_result(
                json_text(serde_json::json!({
                    "error": format!("NATS request failed: {e}"),
                    "subject": full_subject,
                    "target": if active_target.is_empty() { "(any bench)".to_string() } else { active_target },
                })),
                true,
            )),
        }
    }

    #[tool(
        description = "Subscribe to a NATS subject and receive incoming messages as MCP log notifications. Returns immediately with a subscription_id. Each message arriving on the subject is forwarded to Claude as a notifications/message event. Use this for long-running async operations: start a job, watch the result subject, and Claude is notified when done without polling. Call unwatch_subject to stop listening."
    )]
    async fn watch_subject(
        &self,
        Parameters(args): Parameters<WatchSubjectArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let max_messages = args.max_messages.unwrap_or(1);
        let active_target = match args.target {
            Some(t) => t.trim().to_string(),
            None => load_target().await,
        };
        let full_subject = build_subject(&args.subject, &active_target);
        let subscription_id = new_subscription_id();

        let client = match self.nats.client().await {
            Ok(c) => c,
            Err(e) => return Ok(text_result(json_text(serde_json::json!({ "error": e })), true)),
        };
        let mut subscriber = match client.subscribe(full_subject.clone()).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(text_result(
                    json_text(serde_json::json!({ "error": format!("subscribe failed: {e}") })),
                    true,
                ))
            }
        };

        // Drain in the background; forward each message as an MCP log
        // notification so Claude is woken without polling.
        let peer = ctx.peer.clone();
        let subs = self.subs.clone();
        let sub_id = subscription_id.clone();
        let sub_subject = full_subject.clone();
        let handle = tokio::spawn(async move {
            let mut count: u64 = 0;
            while let Some(msg) = subscriber.next().await {
                count += 1;
                let text = String::from_utf8_lossy(&msg.payload).to_string();
                let payload_value: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
                // Match the TS wire shape: data is a JSON *string* holding
                // the envelope, since the original JSON.stringify'd it.
                let data = serde_json::json!({
                    "subscription_id": sub_id,
                    "subject": sub_subject,
                    "message_index": count,
                    "payload": payload_value,
                });
                let _ = peer
                    .notify_logging_message(LoggingMessageNotificationParam {
                        level: LoggingLevel::Info,
                        logger: None,
                        data: serde_json::Value::String(json_text(data)),
                    })
                    .await;
                if count >= max_messages {
                    break;
                }
            }
            subs.lock().await.remove(&sub_id);
        });

        self.subs.lock().await.insert(subscription_id.clone(), handle);

        Ok(text_result(
            json_text(serde_json::json!({
                "ok": true,
                "subscription_id": subscription_id,
                "subject": full_subject,
                "max_messages": max_messages,
                "note": "Messages will arrive as MCP log notifications. Claude will be notified automatically.",
            })),
            false,
        ))
    }

    #[tool(description = "Cancel an active NATS subscription started by watch_subject.")]
    async fn unwatch_subject(
        &self,
        Parameters(args): Parameters<UnwatchSubjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.subs.lock().await.remove(&args.subscription_id) {
            Some(handle) => {
                handle.abort();
                Ok(text_result(
                    json_text(serde_json::json!({
                        "ok": true,
                        "subscription_id": args.subscription_id,
                        "unsubscribed": true,
                    })),
                    false,
                ))
            }
            None => Ok(text_result(
                json_text(serde_json::json!({
                    "ok": false,
                    "error": format!("subscription {} not found", args.subscription_id),
                })),
                false,
            )),
        }
    }
}

#[tool_handler]
impl ServerHandler for BenchNats {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (InitializeResult) is #[non_exhaustive], so start
        // from Default and set the fields we care about. protocol_version
        // keeps the SDK's default (its latest supported), which the serve
        // handshake negotiates against the client.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_logging()
            .build();
        // from_build_env() resolves env!("CARGO_PKG_*") at rmcp's own
        // compile time, which reports "rmcp"/its version, not ours. Take
        // its instance and overwrite name/version with this crate's, so
        // the client sees "bench-nats" exactly like the TS original.
        let mut implementation = Implementation::from_build_env();
        implementation.name = "bench-nats".to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info = implementation;
        info.instructions = Some(
            "bench-nats bridges Claude to a running bench desktop app over NATS. \
             Call list_operations to discover the bench surface, set_target to pick \
             an instance, and send_message to issue a request-reply."
                .to_string(),
        );
        info
    }
}

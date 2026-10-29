//! NATS request-reply dispatcher — the bench's reason for existing.
//
// Subject scheme (faithful to the Bun dispatcher):
//   bench.<group>.<op>                → any instance (queue group "bench")
//   {instanceName}.bench.<group>.<op> → only this instance (queue {instanceName})
// bench-nats prefixes the subject with "{instanceName}." to target one box.
//
// Each incoming message carries JSON args; the dispatcher routes to the tool
// handler and replies with the handler's JSON result, or {"error": "..."}.
//
// After every *known* call it emits telemetry: a ToolEvent on oos.bench.debug
// (live feed for the window and bench-nats) and a bench_events row (audit log).
// Unknown subjects only get the error reply — no telemetry — matching the Bun
// handler, which returned before the event was built.
//
// This module owns only the responder side. Connection lifecycle (connect,
// reconnect, status, the debug consumer, OTLP) lives in `observe`, which calls
// subscribe_responders once per connected client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_nats::{Client, Message};
use chrono::{SecondsFormat, Utc};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::events_log::EventLog;
use crate::telemetry::ToolEvent;
use crate::tools;

/// Subject the tool telemetry is published on. The observe-side consumer
/// subscribes to the same subject to feed the window.
pub const DEBUG_SUBJECT: &str = "oos.bench.debug";

// Per-call deadline. Most handlers finish well under NATS' own 30 s
// request-reply window; a slow non-exempt one replies with an error at the
// deadline instead of letting the caller hang. The exec/push/reset subjects
// are exempt because they legitimately run longer (the caller sets the bound).
const HANDLER_DEADLINE: Duration = Duration::from_millis(25_000);

fn is_unlimited(subject: &str) -> bool {
    matches!(
        subject,
        "bench.exec.exec" | "bench.exec.exec_start" | "bench.git.push" | "bench.pg.reset"
    )
}

/// Wire the shared and (optionally) instance-targeted subscriptions on one
/// connected client, and return the JoinHandles of the two subscription loops
/// so the supervisor can abort them on reconnect.
//
// handle() is fire-and-forget per message: a slow call (large search, long
// exec) must never block the loop from dispatching the next request — otherwise
// a busy single consumer makes NATS return NoResponders (503) to fresh callers.
pub fn subscribe_responders(
    client: Client,
    instance: String,
    ctx: Arc<Ctx>,
    event_log: Arc<EventLog>,
) -> Vec<JoinHandle<()>> {
    let mut tasks = Vec::new();

    // Shared queue — any bench instance can answer.
    {
        let client = client.clone();
        let ctx = ctx.clone();
        let event_log = event_log.clone();
        tasks.push(tokio::spawn(async move {
            let mut sub = match client.queue_subscribe("bench.>", "bench".to_string()).await {
                Ok(sub) => sub,
                Err(err) => {
                    eprintln!("[bench] subscribe bench.> failed: {err}");
                    return;
                }
            };
            while let Some(msg) = sub.next().await {
                let client = client.clone();
                let ctx = ctx.clone();
                let event_log = event_log.clone();
                tokio::spawn(async move { handle(msg, &client, None, &ctx, &event_log).await });
            }
        }));
    }

    // Targeted queue — only this named instance answers.
    if !instance.is_empty() {
        let prefix = format!("{instance}.");
        let subject = format!("{prefix}bench.>");
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            let mut sub = match client.queue_subscribe(subject, instance).await {
                Ok(sub) => sub,
                Err(err) => {
                    eprintln!("[bench] subscribe targeted failed: {err}");
                    return;
                }
            };
            while let Some(msg) = sub.next().await {
                let client = client.clone();
                let ctx = ctx.clone();
                let event_log = event_log.clone();
                let prefix = prefix.clone();
                tokio::spawn(
                    async move { handle(msg, &client, Some(prefix), &ctx, &event_log).await },
                );
            }
        }));
    }

    tasks
}

async fn handle(
    msg: Message,
    client: &Client,
    strip: Option<String>,
    ctx: &Ctx,
    event_log: &EventLog,
) {
    // Strip the instance prefix so route keys always start with "bench.".
    let full = msg.subject.as_str();
    let subject = match &strip {
        Some(p) if full.starts_with(p.as_str()) => &full[p.len()..],
        _ => full,
    };

    let args: Value = serde_json::from_slice(&msg.payload).unwrap_or_else(|_| json!({}));

    let started = Instant::now();
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let args_for_event = args.clone();

    // Unknown subject: reply error and stop — no telemetry, as in the Bun bench.
    let Some(result) = route(subject, args, ctx).await else {
        if let Some(reply) = msg.reply {
            let _ = client
                .publish(reply, err_json(&format!("unknown subject: {subject}")).into())
                .await;
        }
        return;
    };

    // Render the wire reply: pretty JSON on success (matching the Bun jsonResult
    // 2-space output), compact {"error": ...} on failure.
    let (reply_text, status, error_msg): (String, &str, Option<String>) = match result {
        Ok(value) => (
            serde_json::to_string_pretty(&value).unwrap_or_else(|err| err_json(&err.to_string())),
            "ok",
            None,
        ),
        Err(err) => {
            let message = err.to_string();
            (err_json(&message), "error", Some(message))
        }
    };

    if let Some(reply) = msg.reply {
        let _ = client.publish(reply, reply_text.clone().into()).await;
    }

    let event = ToolEvent {
        id: Uuid::new_v4().to_string(),
        tool: subject.to_string(),
        args: args_for_event,
        duration_ms: started.elapsed().as_millis() as u64,
        status: status.to_string(),
        error: error_msg,
        result_size: reply_text.len(),
        ts,
    };

    // Fire-and-forget telemetry: publish to oos.bench.debug, then persist.
    if let Ok(bytes) = serde_json::to_vec(&event) {
        let _ = client.publish(DEBUG_SUBJECT, bytes.into()).await;
    }
    event_log.log(&event);
}

// Route a request, applying the deadline. Returns None for an unknown subject
// (the dispatcher renders that as an error without telemetry).
async fn route(subject: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    if is_unlimited(subject) {
        tools::route(subject, args, ctx).await
    } else {
        match tokio::time::timeout(HANDLER_DEADLINE, tools::route(subject, args, ctx)).await {
            Ok(outcome) => outcome,
            Err(_) => Some(Err(ToolError::Msg(format!(
                "handler deadline exceeded after {} ms",
                HANDLER_DEADLINE.as_millis()
            )))),
        }
    }
}

fn err_json(message: &str) -> String {
    json!({ "error": message }).to_string()
}

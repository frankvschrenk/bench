//! Tool dispatcher — the bench's reason for existing.
//
// Every call (from /mcp, from bench-mcp via POST /call/<operation>) lands in
// `call`: route to the tool handler, render the reply as JSON, and emit
// telemetry (a ToolEvent for the window's live feed and a bench_events row
// for the audit log). Unknown operations only get the error reply — no
// telemetry — matching the Bun handler. Until 14.11.2026 this answered NATS
// request-reply on bench.> / {instance}.bench.>; the reply shapes are unchanged.

use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::events_log::EventLog;
use crate::observe::UiSink;
use crate::telemetry::ToolEvent;
use crate::tools;

// Per-call deadline. A slow non-exempt handler replies with an error at the
// deadline instead of letting the caller hang. The exec/push/reset operations
// are exempt because they legitimately run longer (the caller sets the bound).
const HANDLER_DEADLINE: Duration = Duration::from_millis(25_000);

fn is_unlimited(subject: &str) -> bool {
    matches!(
        subject,
        "bench.exec.exec" | "bench.exec.exec_start" | "bench.git.push" | "bench.pg.reset"
    )
}

/// Outcome of one call.
pub enum Reply {
    /// The tool ran: rendered JSON (pretty on success, {"error"} on failure).
    Done { text: String },
    /// No such operation.
    Unknown(String),
}

/// Run one operation and record it.
pub async fn call(subject: &str, args: Value, ctx: &Ctx, ui: &UiSink, event_log: &EventLog) -> Reply {
    let started = Instant::now();
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let args_for_event = args.clone();

    let Some(result) = route(subject, args, ctx).await else {
        return Reply::Unknown(err_json(&format!("unknown subject: {subject}")));
    };

    // Render the reply: pretty JSON on success (matching the Bun jsonResult
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
    ui.tool(event.clone());
    event_log.log(&event);

    Reply::Done { text: reply_text }
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

//! Long-term memory handlers (bench.memory.*).
//
// Persistence lives in oosmem (Go service, NATS-only) under oos.cmd.mem.*.
// This module is a thin adapter: it shapes args into oosmem payloads and
// returns oosmem's reply verbatim. No DB. Faithful port of the Bun memory.ts.
//
// oosmem replies always carry an `error` field (empty on success); a non-empty
// error becomes a tool error, otherwise the whole reply passes through. The
// outbound request rides ctx.nats (the dispatcher's own client) — oosmem
// listens on the bare oos.cmd.mem.* subjects, so no instance prefix is added.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;

// Must stay in sync with apps/oosmem/internal/server/subjects.go.
pub(crate) const SUBJECT_APPEND: &str = "oos.cmd.mem.event.append";
const SUBJECT_SEARCH: &str = "oos.cmd.mem.search";
pub(crate) const SUBJECT_STREAM_EVENTS: &str = "oos.cmd.mem.stream.events";

const VALID_TRACES: [&str; 4] = ["space", "time", "action", "unknown"];

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "write" => write(args, ctx).await,
        "search" => search(args, ctx).await,
        "list" => list(args, ctx).await,
        _ => return None,
    };
    Some(result)
}

#[derive(Deserialize)]
struct WriteArgs {
    stream_id: u64,
    content: String,
    topic: Option<String>,
    trace: Option<String>,
    /// ids of events this one replaces (oosmem ref kind 1 = supersedes);
    /// superseded events drop out of search but stay in the stream history
    #[serde(default)]
    supersedes: Vec<u64>,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    k: Option<u64>,
    stream: Option<u64>,
    #[serde(default)]
    include_superseded: bool,
}

#[derive(Deserialize)]
struct ListArgs {
    stream_id: u64,
    limit: Option<u64>,
    before: Option<u64>,
}

async fn write(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: WriteArgs = serde_json::from_value(args)?;
    let trace = a.trace.unwrap_or_else(|| "unknown".to_string());
    if !VALID_TRACES.contains(&trace.as_str()) {
        return Err(ToolError::Msg(format!("memory_write: invalid trace \"{trace}\"")));
    }
    let payload = json!({
        "stream_id": a.stream_id,
        "content": a.content,
        "topic": a.topic.unwrap_or_default(),
        "trace": trace,
        "refs": a.supersedes.iter().map(|id| json!({ "target_event_id": id, "kind": 1 })).collect::<Vec<_>>(),
    });
    request(ctx, SUBJECT_APPEND, payload, "memory_write").await
}

async fn search(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: SearchArgs = serde_json::from_value(args)?;
    let mut payload = json!({ "query": a.query, "k": a.k.unwrap_or(5), "include_superseded": a.include_superseded });
    if let Some(stream) = a.stream {
        payload["stream"] = json!(stream);
    }
    request(ctx, SUBJECT_SEARCH, payload, "memory_search").await
}

async fn list(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: ListArgs = serde_json::from_value(args)?;
    let mut payload = json!({ "stream_id": a.stream_id, "limit": a.limit.unwrap_or(20) });
    if let Some(before) = a.before {
        payload["before"] = json!(before);
    }
    request(ctx, SUBJECT_STREAM_EVENTS, payload, "memory_list").await
}

// Request-reply to oosmem and unwrap. A non-empty `error` field in the reply is
// surfaced as a tool error; otherwise the parsed reply is returned verbatim.
pub(crate) async fn request(ctx: &Ctx, subject: &str, payload: Value, label: &str) -> Result<Value, ToolError> {
    let bytes = serde_json::to_vec(&payload)?;
    let msg = ctx
        .nats
        .request(subject.to_string(), bytes.into())
        .await
        .map_err(|e| ToolError::Msg(format!("{label}: {e}")))?;
    let reply: Value = serde_json::from_slice(&msg.payload)
        .map_err(|e| ToolError::Msg(format!("{label}: invalid reply: {e}")))?;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        if !err.is_empty() {
            return Err(ToolError::Msg(format!("{label}: {err}")));
        }
    }
    Ok(reply)
}

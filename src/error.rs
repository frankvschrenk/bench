//! Tool error type.
//
// Why a dedicated error (separate from any future invoke-command error): tool
// handlers answer NATS callers, not the webview. Every handler returns
// Result<Value, ToolError>; the dispatcher renders an Err as the wire shape
// {"error": "<message>"} — the same contract the Bun bench used (errResult).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    /// Caller-facing message (root escape, validation, business rule).
    #[error("{0}")]
    Msg(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid arguments: {0}")]
    Args(#[from] serde_json::Error),
    /// Postgres failure from the pg.* and task.* tools (sqlx).
    #[error("{0}")]
    Db(#[from] sqlx::Error),
}

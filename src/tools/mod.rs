//! Tool routing. The dispatcher hands us the bench.<group>.<op> subject; we
//! pick the group module and let it match the op. Returning None means "no such
//! subject", which the dispatcher renders as an unknown-subject error.
//
// All groups land now: fs + exec/search/patch/git (local) and memory/task/pg
// (memory forwards to oosmem, task/pg are Postgres-backed). The observability
// UI is the only remaining slice; it does not add tool groups.

use serde_json::Value;

use crate::ctx::Ctx;
use crate::error::ToolError;

pub mod exec;
pub mod fs;
pub mod git;
pub mod handbook;
pub mod memory;
pub mod patch;
pub mod pg;
pub mod search;
pub mod task;

pub async fn route(subject: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let rest = subject.strip_prefix("bench.")?;
    let (group, op) = rest.split_once('.')?;
    match group {
        "fs" => fs::handle(op, args, ctx).await,
        "exec" => exec::handle(op, args, ctx).await,
        "search" => search::handle(op, args, ctx).await,
        "patch" => patch::handle(op, args, ctx).await,
        "git" => git::handle(op, args, ctx).await,
        "memory" => memory::handle(op, args, ctx).await,
        "task" => task::handle(op, args, ctx).await,
        "handbook" => handbook::handle(op, args, ctx).await,
        "context" => handbook::handle_context(op, args, ctx).await,
        "pg" => pg::handle(op, args, ctx).await,
        _ => None,
    }
}

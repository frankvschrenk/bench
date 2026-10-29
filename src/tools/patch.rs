//! Apply unified diffs (bench.patch.apply_patch).
//
// Faithful port of the Bun tools/patch.ts: shells out to `patch -p<strip>`
// (with --dry-run when `check`), feeding the diff on stdin. std::process on a
// blocking thread — no extra crate. A non-zero exit surfaces patch's own
// stderr (or stdout) so the caller sees why it failed.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::roots::RootRegistry;

fn default_strip() -> i64 {
    1
}

#[derive(Deserialize)]
struct PatchArgs {
    cwd: String,
    patch: String,
    #[serde(default)]
    check: bool,
    #[serde(default = "default_strip")]
    strip: i64,
}

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    if op != "apply_patch" {
        return None;
    }
    let roots = ctx.roots.clone();
    match tokio::task::spawn_blocking(move || apply_patch(args, &roots)).await {
        Ok(outcome) => Some(outcome),
        Err(join) => Some(Err(ToolError::Msg(format!("patch task failed: {join}")))),
    }
}

fn apply_patch(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: PatchArgs = serde_json::from_value(args)?;
    let abs = roots.resolve_cwd(&a.cwd)?;

    let mut cmd = Command::new("patch");
    cmd.arg(format!("-p{}", a.strip));
    if a.check {
        cmd.arg("--dry-run");
    }
    cmd.current_dir(&abs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    // Write the diff, then drop the handle to signal EOF before waiting.
    {
        let mut sink = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::Msg("apply_patch: stdin unavailable".into()))?;
        sink.write_all(a.patch.as_bytes())?;
    }
    let out = child.wait_with_output()?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let detail = {
            let e = stderr.trim();
            if e.is_empty() {
                stdout.trim()
            } else {
                e
            }
        };
        return Err(ToolError::Msg(format!(
            "apply_patch failed (exit {code}): {detail}"
        )));
    }

    Ok(json!({
        "cwd": abs.to_string_lossy(),
        "check": a.check,
        "status": "ok",
        "output": stdout.trim(),
    }))
}

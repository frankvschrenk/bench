//! Git integration handlers (bench.git.*).
//
// Faithful port of the Bun tools/git.ts: status / diff / commit / push, each
// shelling out via `git -C <root> ...` on a blocking thread. The path is
// sandbox-resolved first. Reply shapes match the originals.
//
// commit pitfall kept on purpose: WITHOUT an explicit `paths` array, staging is
// `git add -A` (commit everything). Partial commits MUST pass `paths`.

use std::path::Path;
use std::process::{Command, Output};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::roots::RootRegistry;

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let roots = ctx.roots.clone();
    let op = op.to_string();
    match tokio::task::spawn_blocking(move || dispatch(&op, args, &roots)).await {
        Ok(outcome) => outcome,
        Err(join) => Some(Err(ToolError::Msg(format!("git task failed: {join}")))),
    }
}

fn dispatch(op: &str, args: Value, roots: &RootRegistry) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "status" => status(args, roots),
        "diff" => diff(args, roots),
        "commit" => commit(args, roots),
        "push" => push(args, roots),
        _ => return None,
    };
    Some(result)
}

// ─── Arg shapes ─────────────────────────────────────────────

#[derive(Deserialize)]
struct PathArg {
    path: String,
}

#[derive(Deserialize)]
struct DiffArgs {
    path: String,
    #[serde(default)]
    staged: bool,
}

#[derive(Deserialize)]
struct CommitArgs {
    path: String,
    message: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    allow_empty: bool,
}

#[derive(Deserialize)]
struct PushArgs {
    path: String,
    remote: Option<String>,
    branch: Option<String>,
    #[serde(default)]
    force_with_lease: bool,
    #[serde(default)]
    tags: bool,
}

// ─── Handlers ───────────────────────────────────────────

fn status(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: PathArg = serde_json::from_value(args)?;
    let abs = roots.resolve(&a.path)?;
    let out = git(&abs, &["status", "--porcelain=v1", "-b"])?;
    if !out.status.success() {
        return Err(ToolError::Msg(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.split('\n').filter(|l| !l.is_empty()).collect();
    // First porcelain -b line is the branch header "## branch...ahead/behind".
    let branch = lines.first().map(|l| l.replace("## ", "")).unwrap_or_default();
    let entries: Vec<Value> = lines
        .iter()
        .skip(1)
        .map(|l| {
            // "XY <path>": status code in bytes 0..2 (ASCII), path from byte 3.
            let code = l.get(0..2).unwrap_or("").trim();
            let file = l.get(3..).unwrap_or("");
            json!({ "code": code, "file": file })
        })
        .collect();
    Ok(json!({
        "dir": abs.to_string_lossy(),
        "branch": branch,
        "clean": entries.is_empty(),
        "entries": entries,
    }))
}

fn diff(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: DiffArgs = serde_json::from_value(args)?;
    let abs = roots.resolve(&a.path)?;
    let out = if a.staged {
        git(&abs, &["diff", "--cached"])?
    } else {
        git(&abs, &["diff"])?
    };
    if !out.status.success() {
        return Err(ToolError::Msg(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(json!({
        "dir": abs.to_string_lossy(),
        "diff": String::from_utf8_lossy(&out.stdout),
    }))
}

fn commit(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: CommitArgs = serde_json::from_value(args)?;
    let abs = roots.resolve(&a.path)?;

    // Stage. Explicit paths -> stage exactly those; none -> `git add -A` (the
    // documented commit-everything pitfall, kept faithfully).
    if a.paths.is_empty() {
        let _ = git(&abs, &["add", "-A"])?;
    } else {
        let mut add_args: Vec<&str> = vec!["add"];
        add_args.extend(a.paths.iter().map(String::as_str));
        let _ = git(&abs, &add_args)?;
    }

    // Commit. Exit status is intentionally ignored: a clean tree returns
    // non-zero ("nothing to commit"), which is not an error here — the log
    // lookup below reports whatever HEAD currently is.
    let mut commit_args: Vec<&str> = vec!["commit", "-m", &a.message];
    if a.allow_empty {
        commit_args.push("--allow-empty");
    }
    let _ = git(&abs, &commit_args)?;

    let log = git(&abs, &["log", "-1", "--oneline"])?;
    let log_out = String::from_utf8_lossy(&log.stdout);
    let sha = log_out.split(' ').next().unwrap_or("").to_string();
    let summary = log_out.trim().to_string();

    let mut pushed = false;
    let mut push_output = String::new();
    if a.push {
        let p = git(&abs, &["push"])?;
        pushed = p.status.success();
        push_output = String::from_utf8_lossy(&p.stderr).trim().to_string();
    }

    Ok(json!({
        "dir": abs.to_string_lossy(),
        "sha": sha,
        "summary": summary,
        "pushed": pushed,
        "push_output": push_output,
    }))
}

fn push(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: PushArgs = serde_json::from_value(args)?;
    let abs = roots.resolve(&a.path)?;
    let mut push_args: Vec<&str> = vec!["push"];
    if let Some(r) = &a.remote {
        push_args.push(r);
    }
    if let Some(b) = &a.branch {
        push_args.push(b);
    }
    if a.force_with_lease {
        push_args.push("--force-with-lease");
    }
    if a.tags {
        push_args.push("--tags");
    }
    let out = git(&abs, &push_args)?;
    Ok(json!({
        "dir": abs.to_string_lossy(),
        "status": if out.status.success() { "ok" } else { "error" },
        "exit_code": out.status.code().unwrap_or(-1),
    }))
}

// ─── Helper ────────────────────────────────────────────

// Run `git -C <abs> <args...>` and capture output. -C scopes git to the repo
// at the resolved (sandboxed) path regardless of the process cwd.
fn git(abs: &Path, args: &[&str]) -> Result<Output, ToolError> {
    Command::new("git")
        .arg("-C")
        .arg(abs)
        .args(args)
        .output()
        .map_err(ToolError::from)
}

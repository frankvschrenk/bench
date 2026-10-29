//! Command execution handlers (bench.exec.*).
//
// Covers exec / exec_start / exec_read / exec_stop / which. Unlike the fs
// handlers (blocking std::fs on a worker thread), these are async tokio::process
// so a long-running child never ties up an OS thread. Streaming sessions are
// keyed by UUID in a process-global table. Faithful port of the Bun
// tools/exec.ts; the JSON reply shapes match the originals.
//
// Deviation from the first design sketch: the streaming monitor is a plain
// child.wait() rather than a select! over a kill channel. exec_stop terminates
// by sending SIGTERM to the pid (`kill <pid>`) and lets the monitor reap the
// child via wait(). This keeps `child` owned outright by one task — no shared
// &mut, no select-arm borrow gymnastics — and is portable on the macOS target.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::roots::RootRegistry;

// 1 MiB cap on captured output (same as the Bun MAX). Truncation backs up to a
// UTF-8 char boundary so a multi-byte sequence is never split (slicing raw
// bytes would panic).
const MAX_OUTPUT: usize = 1024 * 1024;

/// A live `exec_start` session. The two buffers accumulate while the child
/// runs; `exec_read` drains them, `exec_stop` (or natural exit) ends it.
struct Session {
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    running: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
    /// Child pid, so exec_stop can signal it without holding the child handle.
    pid: Option<u32>,
}

// Process-global session table. OnceLock initialises it lazily without a macro
// dependency; the std Mutex is only ever held for the map lookup/insert, never
// across an await.
fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "exec" => exec(args, &ctx.roots).await,
        "exec_start" => exec_start(args, &ctx.roots).await,
        "exec_read" => exec_read(args),
        "exec_stop" => exec_stop(args),
        "which" => which(args).await,
        _ => return None,
    };
    Some(result)
}

// ─── Arg shapes ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ExecArgs {
    command: String,
    cwd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    stdin: Option<String>,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct StartArgs {
    command: String,
    cwd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    stdin: Option<String>,
}

#[derive(Deserialize)]
struct SessionArg {
    session_id: String,
}

#[derive(Deserialize)]
struct WhichArg {
    name: String,
}

// ─── Handlers ───────────────────────────────────────────

// Run to completion, capturing all output. The own timeout (min(secs,3600)) is
// independent of the dispatcher, which exempts bench.exec.exec from its
// deadline precisely so this bound governs. Pipe reads run as concurrent tasks
// so a child that floods one stream cannot deadlock against a full pipe buffer.
async fn exec(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let mut a: ExecArgs = serde_json::from_value(args)?;
    let abs_cwd = roots.resolve_cwd(&a.cwd)?;
    let timeout_secs = a.timeout_seconds.unwrap_or(60).min(3600);
    let start = Instant::now();

    let mut cmd = Command::new(&a.command);
    cmd.args(&a.args)
        .current_dir(&abs_cwd)
        .env("PATH", tool_path())
        .envs(&a.env) // tokio inherits the parent env; we layer these on top
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if a.stdin.is_some() { Stdio::piped() } else { Stdio::null() });

    let mut child = cmd.spawn()?;

    // Feed stdin on its own task: writing inline could block on a full pipe if
    // the child floods stdout before reading, so let it run alongside the reads.
    if let Some(input) = a.stdin.take() {
        if let Some(mut sink) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = sink.write_all(input.as_bytes()).await;
                let _ = sink.shutdown().await; // drop -> EOF
            });
        }
    }

    let so = child.stdout.take().expect("stdout piped");
    let se = child.stderr.take().expect("stderr piped");
    let so_task = tokio::spawn(drain_pipe(so));
    let se_task = tokio::spawn(drain_pipe(se));

    let status = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(res) => res.ok(),
        Err(_) => {
            // Deadline hit: terminate and reap so the pipe reads can finish.
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    let exit_code = status.and_then(exit_code_of);

    let stdout_full = so_task.await.unwrap_or_default();
    let stderr_full = se_task.await.unwrap_or_default();
    let (stdout, stdout_truncated) = truncate_at(stdout_full, MAX_OUTPUT);
    let (stderr, stderr_truncated) = truncate_at(stderr_full, MAX_OUTPUT);
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(json!({
        "command": a.command,
        "args": a.args,
        "cwd": path_str(&abs_cwd),
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "duration_ms": duration_ms,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    }))
}

// Spawn a long-running child and return a session id; output streams into the
// session buffers for exec_read to drain. The monitor task owns the child and
// records the exit code on completion.
async fn exec_start(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let mut a: StartArgs = serde_json::from_value(args)?;
    let abs_cwd = roots.resolve_cwd(&a.cwd)?;
    let session_id = Uuid::new_v4().to_string();

    let mut cmd = Command::new(&a.command);
    cmd.args(&a.args)
        .current_dir(&abs_cwd)
        .env("PATH", tool_path())
        .envs(&a.env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if a.stdin.is_some() { Stdio::piped() } else { Stdio::null() });

    let mut child = cmd.spawn()?;

    if let Some(input) = a.stdin.take() {
        if let Some(mut sink) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = sink.write_all(input.as_bytes()).await;
                let _ = sink.shutdown().await;
            });
        }
    }

    let pid = child.id();
    let session = Arc::new(Session {
        stdout: Arc::new(Mutex::new(String::new())),
        stderr: Arc::new(Mutex::new(String::new())),
        running: Arc::new(AtomicBool::new(true)),
        exit_code: Arc::new(Mutex::new(None)),
        pid,
    });
    sessions()
        .lock()
        .unwrap()
        .insert(session_id.clone(), session.clone());

    let so = child.stdout.take().expect("stdout piped");
    let se = child.stderr.take().expect("stderr piped");
    tokio::spawn(pump(so, session.stdout.clone()));
    tokio::spawn(pump(se, session.stderr.clone()));

    let running = session.running.clone();
    let exit_slot = session.exit_code.clone();
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(exit_code_of);
        running.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = exit_slot.lock() {
            *slot = code;
        }
    });

    Ok(json!({ "session_id": session_id, "status": "started" }))
}

// Drain and return the buffered output since the last read. Synchronous: it
// only takes the std Mutexes briefly (mem::take), never across an await.
fn exec_read(args: Value) -> Result<Value, ToolError> {
    let a: SessionArg = serde_json::from_value(args)?;
    let session = sessions().lock().unwrap().get(&a.session_id).cloned();
    let Some(session) = session else {
        return Err(ToolError::Msg(format!(
            "exec_read: unknown session {}",
            a.session_id
        )));
    };
    let stdout_delta = std::mem::take(&mut *session.stdout.lock().unwrap());
    let stderr_delta = std::mem::take(&mut *session.stderr.lock().unwrap());
    let running = session.running.load(Ordering::SeqCst);
    let exit_code = *session.exit_code.lock().unwrap();
    Ok(json!({
        "session_id": a.session_id,
        "stdout_delta": stdout_delta,
        "stderr_delta": stderr_delta,
        "running": running,
        "exit_code": exit_code,
    }))
}

// Terminate a streaming session and forget it. SIGTERM by pid; the monitor
// task reaps the child via wait(). Best-effort: if the session already exited,
// the kill is a harmless no-op against a dead pid.
fn exec_stop(args: Value) -> Result<Value, ToolError> {
    let a: SessionArg = serde_json::from_value(args)?;
    let session = sessions().lock().unwrap().remove(&a.session_id);
    let Some(session) = session else {
        return Err(ToolError::Msg(format!(
            "exec_stop: unknown session {}",
            a.session_id
        )));
    };
    if let Some(pid) = session.pid {
        kill_pid(pid);
    }
    Ok(json!({ "session_id": a.session_id, "status": "stopped" }))
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
}

// Windows has no SIGTERM; taskkill /T ends the child and its process tree.
#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

// A GUI app on macOS inherits launchd's bare PATH (/usr/bin:/bin:…), so bun,
// cargo and Homebrew tools were "not found" unless a caller spelled out PATH.
// Append the usual developer locations that exist; the caller's env still wins.
fn tool_path() -> String {
    let home = std::path::PathBuf::from(crate::home_dir());
    let mut parts: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extras = vec![home.join(".cargo").join("bin"), home.join(".bun").join("bin")];
    if cfg!(unix) {
        extras.extend(["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"].map(Into::into));
    }
    for extra in extras {
        if !parts.contains(&extra) && extra.is_dir() {
            parts.push(extra);
        }
    }
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// Resolve an executable against $PATH via `which` (`where` on Windows).
// Found iff exit 0; `where` may list several hits, the first one wins.
async fn which(args: Value) -> Result<Value, ToolError> {
    let a: WhichArg = serde_json::from_value(args)?;
    let finder = if cfg!(windows) { "where" } else { "which" };
    match Command::new(finder).arg(&a.name).env("PATH", tool_path()).output().await {
        Ok(out) if out.status.success() => {
            let path = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            Ok(json!({ "name": a.name, "found": true, "path": path }))
        }
        _ => Ok(json!({ "name": a.name, "found": false, "path": Value::Null })),
    }
}

// ─── Helpers ───────────────────────────────────────────

// Read a pipe to EOF into one string (lossy). Used by exec for whole-output
// capture; the concurrent spawn means a flooding stream never blocks the wait.
async fn drain_pipe<R: AsyncReadExt + Unpin>(mut reader: R) -> String {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

// Stream a pipe chunk-by-chunk into a shared buffer for exec_read. Decoding is
// lossy per chunk: a multi-byte sequence split across a read boundary yields a
// replacement char rather than waiting — acceptable for the mostly-ASCII build
// logs this serves, and it keeps the buffer live without a stateful decoder.
async fn pump<R: AsyncReadExt + Unpin>(mut reader: R, buf: Arc<Mutex<String>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&chunk[..n]);
                if let Ok(mut g) = buf.lock() {
                    g.push_str(&text);
                }
            }
        }
    }
}

// Normal exit -> its code; signal death -> the negated signal (Unix
// convention), so a SIGTERM kill reads as e.g. -15 rather than null.
#[cfg(unix)]
fn exit_code_of(status: std::process::ExitStatus) -> Option<i32> {
    status.code().or_else(|| status.signal().map(|s| -s))
}

#[cfg(not(unix))]
fn exit_code_of(status: std::process::ExitStatus) -> Option<i32> {
    status.code()
}

// Truncate to at most `max` bytes, backing up to a char boundary. Returns the
// (possibly shortened) string and whether truncation happened.
fn truncate_at(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

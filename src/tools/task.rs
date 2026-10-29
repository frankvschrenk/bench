//! Working-memory task handlers (bench.task.*).
//
// Faithful port of the Bun tools/task.ts. State lives in the hard-wired "bench"
// Postgres DB (task / task_note / task_edge), NOT in oosmem — task tracking is a
// graph of notes + edges with its own pgvector embeddings (granite-embedding,
// 384-dim) for semantic resume/search, distinct from the long-term memory.*
// stream that forwards to oosmem. The DSN comes from Settings (host-only); the
// "bench" dbname is appended here, and each op opens a short-lived pool with the
// schema ensured.

use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};

use crate::ctx::Ctx;
use crate::db;
use crate::error::ToolError;

const NO_DSN_HELP: &str = "no DSN configured — set it in Settings → Database";
const EMBED_MODEL: &str = "granite-embedding:latest";
const EMBED_URL: &str = "http://localhost:11434/v1/embeddings";
// "handoff": Übergabe an die nächste Sitzung (neuer Chat) – context.brief zeigt
// die jüngste je Aufgabe vollständig, auch wenn neuere Notizen folgen.
const NOTE_KINDS: [&str; 6] = ["hypothesis", "finding", "decision", "question", "observation", "handoff"];
const NOTE_KIND_CHECK: &str = "CHECK (kind IN ('hypothesis','finding','decision','question','observation','handoff'))";
const EDGE_KINDS: [&str; 3] = ["touched", "produced", "refers"];

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "start" => start(args, ctx).await,
        "note" => note(args, ctx).await,
        "link" => link(args, ctx).await,
        "finish" => finish(args, ctx).await,
        "resume" => resume(args, ctx).await,
        "show" => show(args, ctx).await,
        "search" => search(args, ctx).await,
        "reembed" => reembed(ctx).await,
        _ => return None,
    };
    Some(result)
}

// ─── Arg shapes ─────────────────────────────────────────────

#[derive(Deserialize)]
struct StartArgs {
    goal: String,
    repo: Option<String>,
}

#[derive(Deserialize)]
struct NoteArgs {
    task_id: i64,
    kind: String,
    text: String,
}

#[derive(Deserialize)]
struct LinkArgs {
    task_id: i64,
    kind: String,
    target: String,
    detail: Option<String>,
}

#[derive(Deserialize)]
struct FinishArgs {
    task_id: i64,
    outcome: String,
    status: Option<String>,
}

#[derive(Deserialize)]
struct ResumeArgs {
    query: Option<String>,
}

#[derive(Deserialize)]
struct ShowArgs {
    task_id: i64,
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    n: Option<i64>,
    #[serde(default)]
    include_finished: bool,
}

// ─── Handlers ───────────────────────────────────────────

async fn start(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: StartArgs = serde_json::from_value(args)?;
    let pool = open_db(ctx, "task_start").await?;
    let row = sqlx::query("INSERT INTO task (goal, repo) VALUES ($1, $2) RETURNING id, started_at, status")
        .bind(&a.goal)
        .bind(a.repo.as_deref())
        .fetch_one(&pool)
        .await?;
    let started_at: chrono::DateTime<chrono::Utc> = row.try_get("started_at")?;
    let out = json!({
        "task_id": row.try_get::<i64, _>("id")?,
        "goal": a.goal,
        "repo": a.repo,
        "started_at": iso(started_at),
        "status": row.try_get::<String, _>("status")?,
    });
    pool.close().await;
    Ok(out)
}

async fn note(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: NoteArgs = serde_json::from_value(args)?;
    if !NOTE_KINDS.contains(&a.kind.as_str()) {
        return Err(ToolError::Msg(format!("task_note: invalid kind \"{}\"", a.kind)));
    }
    let pool = open_db(ctx, "task_note").await?;
    let vec = embed(&a.text).await;
    let lit = vec.as_ref().map(|v| vec_literal(v));
    let row = sqlx::query(
        "INSERT INTO task_note (task_id, kind, text, embedding) \
         VALUES ($1, $2, $3, $4::vector) RETURNING id",
    )
    .bind(a.task_id)
    .bind(&a.kind)
    .bind(&a.text)
    .bind(lit.as_deref())
    .fetch_one(&pool)
    .await?;
    let out = json!({
        "id": row.try_get::<i64, _>("id")?,
        "task_id": a.task_id,
        "kind": a.kind,
        "embedded": vec.is_some(),
    });
    pool.close().await;
    Ok(out)
}

async fn link(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: LinkArgs = serde_json::from_value(args)?;
    if !EDGE_KINDS.contains(&a.kind.as_str()) {
        return Err(ToolError::Msg(format!("task_link: invalid kind \"{}\"", a.kind)));
    }
    let pool = open_db(ctx, "task_link").await?;
    sqlx::query(
        "INSERT INTO task_edge (task_id, kind, target, detail) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (task_id, kind, target) DO NOTHING",
    )
    .bind(a.task_id)
    .bind(&a.kind)
    .bind(&a.target)
    .bind(a.detail.as_deref())
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(json!({ "task_id": a.task_id, "kind": a.kind, "target": a.target, "status": "ok" }))
}

async fn finish(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: FinishArgs = serde_json::from_value(args)?;
    let status = a.status.unwrap_or_else(|| "done".to_string());
    if status != "done" && status != "abandoned" {
        return Err(ToolError::Msg(format!("task_finish: invalid status \"{status}\"")));
    }
    let pool = open_db(ctx, "task_finish").await?;
    sqlx::query("UPDATE task SET status = $1, outcome = $2, finished_at = now() WHERE id = $3")
        .bind(&status)
        .bind(&a.outcome)
        .bind(a.task_id)
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(json!({ "task_id": a.task_id, "status": status, "outcome": a.outcome }))
}

async fn resume(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: ResumeArgs = serde_json::from_value(args)?;
    let pool = open_db(ctx, "task_resume").await?;

    let mut task: Option<Value> = None;
    if let Some(q) = &a.query {
        if let Some(vec) = embed(q).await {
            // Nearest note of an active task. (The Bun original used SELECT
            // DISTINCT with ORDER BY a non-selected expression, which Postgres
            // rejects with 42P10 as soon as notes actually carry vectors.)
            let row = sqlx::query(
                "SELECT t.id FROM task t \
                 JOIN task_note n ON n.task_id = t.id \
                 WHERE t.status = 'active' AND n.embedding IS NOT NULL \
                 ORDER BY n.embedding <=> $1::vector LIMIT 1",
            )
            .bind(vec_literal(&vec))
            .fetch_optional(&pool)
            .await?;
            if let Some(row) = row {
                task = bundle_task(&pool, row.try_get::<i64, _>("id")?).await?;
            }
        }
    }
    if task.is_none() {
        let row = sqlx::query("SELECT id FROM task WHERE status = 'active' ORDER BY started_at DESC LIMIT 1")
            .fetch_optional(&pool)
            .await?;
        if let Some(row) = row {
            task = bundle_task(&pool, row.try_get::<i64, _>("id")?).await?;
        }
    }
    pool.close().await;
    match task {
        Some(t) => Ok(json!({ "found": true, "task": t })),
        None => Ok(json!({ "found": false })),
    }
}

async fn show(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: ShowArgs = serde_json::from_value(args)?;
    let pool = open_db(ctx, "task_show").await?;
    let task = bundle_task(&pool, a.task_id).await?;
    pool.close().await;
    match task {
        Some(t) => Ok(t),
        None => Err(ToolError::Msg(format!("task_show: task {} not found", a.task_id))),
    }
}

async fn search(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: SearchArgs = serde_json::from_value(args)?;
    let pool = open_db(ctx, "task_search").await?;
    let limit = a.n.unwrap_or(5).min(20);
    let vec = embed(&a.query).await;

    let rows = if let Some(v) = &vec {
        let sql = if a.include_finished {
            "SELECT n.id, n.task_id, n.kind, n.text, n.created_at, n.embedding <=> $1::vector AS similarity \
             FROM task_note n ORDER BY similarity LIMIT $2"
        } else {
            "SELECT n.id, n.task_id, n.kind, n.text, n.created_at, n.embedding <=> $1::vector AS similarity \
             FROM task_note n JOIN task t ON t.id = n.task_id WHERE t.status = 'active' ORDER BY similarity LIMIT $2"
        };
        sqlx::query(sql)
            .bind(vec_literal(v))
            .bind(limit)
            .fetch_all(&pool)
            .await?
    } else {
        sqlx::query("SELECT n.id, n.task_id, n.kind, n.text, n.created_at FROM task_note n ORDER BY n.created_at DESC LIMIT $1")
            .bind(limit)
            .fetch_all(&pool)
            .await?
    };

    let has_sim = vec.is_some();
    let results: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("id".into(), json!(r.try_get::<i64, _>("id").unwrap_or_default()));
            m.insert("task_id".into(), json!(r.try_get::<i64, _>("task_id").unwrap_or_default()));
            m.insert("kind".into(), json!(r.try_get::<String, _>("kind").unwrap_or_default()));
            m.insert("text".into(), json!(r.try_get::<String, _>("text").unwrap_or_default()));
            m.insert("created_at".into(), json!(created_at_iso(r)));
            if has_sim {
                m.insert("similarity".into(), json!(r.try_get::<f64, _>("similarity").unwrap_or_default()));
            }
            Value::Object(m)
        })
        .collect();
    pool.close().await;
    Ok(json!({ "query": a.query, "results": results }))
}

/// Notes written while the embedding model was missing carry no vector and
/// are invisible to semantic search/resume. Embed them now (up to 500 per call).
async fn reembed(ctx: &Ctx) -> Result<Value, ToolError> {
    let pool = open_db(ctx, "task_reembed").await?;
    let rows = sqlx::query("SELECT id, text FROM task_note WHERE embedding IS NULL ORDER BY id LIMIT 500")
        .fetch_all(&pool)
        .await?;
    let (mut done, mut failed) = (0u32, 0u32);
    for r in &rows {
        let id: i64 = r.try_get("id")?;
        let text: String = r.try_get("text")?;
        match embed(&text).await {
            Some(v) => {
                sqlx::query("UPDATE task_note SET embedding = $1::vector WHERE id = $2")
                    .bind(vec_literal(&v))
                    .bind(id)
                    .execute(&pool)
                    .await?;
                done += 1;
            }
            None => failed += 1,
        }
    }
    let left: i64 = sqlx::query("SELECT count(*) AS n FROM task_note WHERE embedding IS NULL")
        .fetch_one(&pool)
        .await?
        .try_get("n")?;
    pool.close().await;
    Ok(json!({ "embedded": done, "failed": failed, "still_missing": left, "model": EMBED_MODEL }))
}

// ─── Helpers ───────────────────────────────────────────

// Open a pool against the bench DB with the schema ensured, or a clear error
// (tool-prefixed) pointing at Settings when no DSN is configured.
pub(crate) async fn open_db(ctx: &Ctx, label: &str) -> Result<PgPool, ToolError> {
    let dsn = ctx.settings.read().await.dsn.clone();
    if dsn.is_empty() {
        return Err(ToolError::Msg(format!("{label}: {NO_DSN_HELP}")));
    }
    let pool = db::connect(&db::bench_dsn(&dsn), 3).await?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

async fn ensure_schema(pool: &PgPool) -> Result<(), ToolError> {
    if let Err(e) = sqlx::query("CREATE EXTENSION IF NOT EXISTS vector").execute(pool).await {
        // 42710 = duplicate_object (benign creation race); anything else is real.
        if e.as_database_error().and_then(|d| d.code()).as_deref() != Some("42710") {
            return Err(e.into());
        }
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS task ( \
         id bigserial PRIMARY KEY, goal text NOT NULL, repo text, \
         status text NOT NULL DEFAULT 'active' CHECK (status IN ('active','done','abandoned')), \
         outcome text, started_at timestamptz NOT NULL DEFAULT now(), finished_at timestamptz )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS task_note ( \
         id bigserial PRIMARY KEY, task_id bigint NOT NULL REFERENCES task(id) ON DELETE CASCADE, \
         kind text NOT NULL CHECK (kind IN ('hypothesis','finding','decision','question','observation','handoff')), \
         text text NOT NULL, embedding vector(384), created_at timestamptz NOT NULL DEFAULT now() )",
    )
    .execute(pool)
    .await?;
    // Ältere Datenbanken: Prüfregel der Notiz-Arten nachziehen (nur wenn "handoff" fehlt).
    let def: Option<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'task_note_kind_check'",
    )
    .fetch_optional(pool)
    .await?;
    if def.is_some_and(|d| !d.contains("handoff")) {
        sqlx::query("ALTER TABLE task_note DROP CONSTRAINT task_note_kind_check").execute(pool).await?;
        sqlx::query(&format!("ALTER TABLE task_note ADD CONSTRAINT task_note_kind_check {NOTE_KIND_CHECK}"))
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS task_edge ( \
         id bigserial PRIMARY KEY, task_id bigint NOT NULL REFERENCES task(id) ON DELETE CASCADE, \
         kind text NOT NULL CHECK (kind IN ('touched','produced','refers')), \
         target text NOT NULL, detail text, created_at timestamptz NOT NULL DEFAULT now(), \
         UNIQUE (task_id, kind, target) )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

// Embed text via Ollama (granite-embedding, 384-dim). Best-effort: any failure
// (service down, model missing, non-200) yields None and the caller proceeds
// without a vector — exactly the Bun behaviour.
async fn embed(text: &str) -> Option<Vec<f32>> {
    let resp = reqwest::Client::new()
        .post(EMBED_URL)
        .json(&json!({ "model": EMBED_MODEL, "input": text }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    let arr = data.get("data")?.as_array()?.first()?.get("embedding")?.as_array()?;
    let vec: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
    (!vec.is_empty()).then_some(vec)
}

// pgvector text literal "[a,b,c]" for a TEXT bind cast with $N::vector.
fn vec_literal(vec: &[f32]) -> String {
    let mut s = String::with_capacity(vec.len() * 8 + 2);
    s.push('[');
    for (i, f) in vec.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&f.to_string());
    }
    s.push(']');
    s
}

// Assemble a task with its notes and edges, or None when the id is unknown.
async fn bundle_task(pool: &PgPool, task_id: i64) -> Result<Option<Value>, ToolError> {
    let Some(t) = sqlx::query(
        "SELECT id, goal, repo, status, outcome, started_at, finished_at FROM task WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let started_at: chrono::DateTime<chrono::Utc> = t.try_get("started_at")?;
    let finished_at: Option<chrono::DateTime<chrono::Utc>> = t.try_get("finished_at")?;

    let notes: Vec<Value> = sqlx::query(
        "SELECT id, kind, text, created_at FROM task_note WHERE task_id = $1 ORDER BY created_at",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.try_get::<i64, _>("id").unwrap_or_default(),
            "kind": r.try_get::<String, _>("kind").unwrap_or_default(),
            "text": r.try_get::<String, _>("text").unwrap_or_default(),
            "created_at": created_at_iso(r),
        })
    })
    .collect();

    let edges: Vec<Value> = sqlx::query(
        "SELECT kind, target, detail, created_at FROM task_edge WHERE task_id = $1 ORDER BY created_at",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| {
        json!({
            "kind": r.try_get::<String, _>("kind").unwrap_or_default(),
            "target": r.try_get::<String, _>("target").unwrap_or_default(),
            "detail": r.try_get::<Option<String>, _>("detail").ok().flatten(),
            "created_at": created_at_iso(r),
        })
    })
    .collect();

    Ok(Some(json!({
        "id": t.try_get::<i64, _>("id")?,
        "goal": t.try_get::<String, _>("goal")?,
        "repo": t.try_get::<Option<String>, _>("repo")?,
        "status": t.try_get::<String, _>("status")?,
        "outcome": t.try_get::<Option<String>, _>("outcome")?,
        "started_at": iso(started_at),
        "finished_at": finished_at.map(iso),
        "notes": notes,
        "edges": edges,
    })))
}

// created_at as RFC-3339 millis Z, tolerant: a decode miss yields "".
fn created_at_iso(r: &sqlx::postgres::PgRow) -> String {
    r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        .map(iso)
        .unwrap_or_default()
}

// RFC-3339 with milliseconds and Z, matching new Date().toISOString() and the
// fs handlers' timestamp format.
fn iso(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

//! PostgreSQL handlers (bench.pg.*).
//
// pg.* operates on the application DB the user set in Settings (the host-only
// DSN plus app_database). pg.reset connects to the "postgres" maintenance DB
// to DROP/CREATE the target, since Postgres refuses to drop the DB you are
// connected to. Faithful port of the Bun tools/postgres.ts.
//
// query wraps the user SQL in `SELECT to_jsonb(t)::text FROM (<sql>) t` so any
// row-returning statement comes back as JSON without the sqlx json feature (the
// jsonb is read as text and parsed) — the oosgql/oosd convention. exec uses the
// simple-query protocol (sqlx::raw_sql) so DDL / multi-statement blocks run.

use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::ctx::Ctx;
use crate::db;
use crate::error::ToolError;

const NO_DSN_HELP: &str = "no DSN configured — set it in Settings → Database";
const NO_APP_DB_HELP: &str = "no application database configured — set it in Settings → Database";

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "query" => query(args, ctx).await,
        "exec" => exec(args, ctx).await,
        "reset" => reset(ctx).await,
        _ => return None,
    };
    Some(result)
}

#[derive(Deserialize)]
struct SqlArg {
    sql: String,
}

// Resolve the ready-to-use app DSN, or a clear error naming what's missing.
async fn resolve_app_dsn(ctx: &Ctx, label: &str) -> Result<String, ToolError> {
    let (dsn, app) = {
        let s = ctx.settings.read().await;
        (s.dsn.clone(), s.app_database.clone())
    };
    if dsn.is_empty() {
        return Err(ToolError::Msg(format!("{label}: {NO_DSN_HELP}")));
    }
    if app.is_empty() {
        return Err(ToolError::Msg(format!("{label}: {NO_APP_DB_HELP}")));
    }
    Ok(db::rewrite_database(&dsn, &app))
}

async fn query(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: SqlArg = serde_json::from_value(args)?;
    let dsn = resolve_app_dsn(ctx, "pg_query").await?;
    let pool = db::connect(&dsn, 1).await?;
    // Wrap so every column/type round-trips through jsonb; trailing ';' would
    // break the subquery, so strip it.
    let inner = a.sql.trim().trim_end_matches(';');
    let wrapped = format!("SELECT to_jsonb(t)::text AS j FROM ({inner}) t");
    let rows = sqlx::query(&wrapped).fetch_all(&pool).await?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            r.try_get::<String, _>("j")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null)
        })
        .collect();
    pool.close().await;
    Ok(json!({ "rows": out, "row_count": out.len() }))
}

async fn exec(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: SqlArg = serde_json::from_value(args)?;
    let dsn = resolve_app_dsn(ctx, "pg_exec").await?;
    let pool = db::connect(&dsn, 1).await?;
    // raw_sql = simple-query protocol: parses a whole multi-statement block
    // (incl. $$-quoted PL/pgSQL) server-side, no client-side split needed.
    let result = sqlx::raw_sql(&a.sql).execute(&pool).await?;
    pool.close().await;
    Ok(json!({ "rows_affected": result.rows_affected(), "status": "ok" }))
}

async fn reset(ctx: &Ctx) -> Result<Value, ToolError> {
    let (dsn, app) = {
        let s = ctx.settings.read().await;
        (s.dsn.clone(), s.app_database.clone())
    };
    if dsn.is_empty() {
        return Err(ToolError::Msg(format!("pg_reset: {NO_DSN_HELP}")));
    }
    if app.is_empty() {
        return Err(ToolError::Msg(format!("pg_reset: {NO_APP_DB_HELP}")));
    }
    // Connect to the maintenance DB so we can drop the target. The app name is
    // an identifier (not parameterisable for DDL) — it comes from the user's
    // own Settings, same trust boundary as the Bun version.
    let admin = db::connect(&db::rewrite_database(&dsn, "postgres"), 1).await?;
    sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS {app}"))
        .execute(&admin)
        .await?;
    sqlx::raw_sql(&format!("CREATE DATABASE {app}"))
        .execute(&admin)
        .await?;
    admin.close().await;
    Ok(json!({ "status": "ok", "database": app }))
}

//! Postgres connection helpers for the pg.* and task.* tools.
//
// The DSN in Settings is host-only (no dbname). bench wires two databases on
// top of it: its own working-memory DB "bench" (task tables) and the user's
// application DB (pg.* tools). rewrite_database swaps the dbname segment,
// faithful to the Bun util/db.ts. Each tool op opens a short-lived pool.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::error::ToolError;

const BENCH_DB: &str = "bench";

// Swap the dbname segment of a libpq-style DSN, preserving the query string
// (sslmode etc.). No dbname present -> insert one between host and "?".
// Mirrors the Bun rewriteDatabase byte for byte.
pub fn rewrite_database(dsn: &str, dbname: &str) -> String {
    let (base, query) = match dsn.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (dsn, None),
    };
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let after_scheme = &base[scheme_end..];
    let rebuilt = match after_scheme.find('/') {
        None => format!("{base}/{dbname}"),
        Some(slash) => format!(
            "{}{}{}",
            &base[..scheme_end],
            &after_scheme[..slash + 1],
            dbname
        ),
    };
    match query {
        Some(q) => format!("{rebuilt}?{q}"),
        None => rebuilt,
    }
}

// DSN pointing at the hard-wired "bench" DB (task tables). The dbname is the
// same on every machine, so it lives here, not in Settings.
pub fn bench_dsn(dsn: &str) -> String {
    rewrite_database(dsn, BENCH_DB)
}

// Open a short-lived pool. task.* uses max 3 (a few statements per op); the
// pg.* tools pass 1, matching the Bun postgres({max:1}).
pub async fn connect(dsn: &str, max: u32) -> Result<PgPool, ToolError> {
    PgPoolOptions::new()
        .max_connections(max)
        .connect(dsn)
        .await
        .map_err(ToolError::from)
}

//! bench_events — local SQLite audit log of every handled bench.* call.
//
// Faithful port of the Bun util/logging.ts. Deliberately local SQLite, not the
// Postgres "bench" DB the task.* tools use: the log must record even when no
// DSN is configured, and a logging failure must never affect the dispatcher.
// So every write is fire-and-forget (spawned, errors swallowed) and the whole
// log silently no-ops if the database could not be opened at boot.
//
// The Bun bench wrote into oos-store-ts' shared store.db; the Tauri app keeps
// settings in tauri-plugin-store JSON instead, so the event log gets its own
// file (events.db) in the app data dir.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::telemetry::ToolEvent;

const CREATE_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS bench_events (\
        id          INTEGER  PRIMARY KEY AUTOINCREMENT,\
        ts          TEXT     NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),\
        subject     TEXT     NOT NULL,\
        args_json   TEXT,\
        status      TEXT     NOT NULL DEFAULT 'ok',\
        duration_ms INTEGER  NOT NULL DEFAULT 0,\
        result_size INTEGER  NOT NULL DEFAULT 0,\
        error_msg   TEXT\
    )";
const CREATE_IDX_TS: &str = "CREATE INDEX IF NOT EXISTS bench_events_ts ON bench_events (ts DESC)";
const CREATE_IDX_SUBJECT: &str =
    "CREATE INDEX IF NOT EXISTS bench_events_subject ON bench_events (subject)";
const INSERT: &str = "\
    INSERT INTO bench_events (subject, args_json, status, duration_ms, result_size, error_msg)\
    VALUES (?, ?, ?, ?, ?, ?)";

/// A handle to the bench_events log. Cheap to clone (the pool is Arc-backed).
/// `pool` is None when the database could not be opened, which turns every
/// `log` into a no-op.
#[derive(Clone)]
pub struct EventLog {
    pool: Option<SqlitePool>,
}

impl EventLog {
    /// Open (creating if absent) the SQLite event log at `db_path` and ensure
    /// the schema exists. Never panics: on any failure it logs to stderr and
    /// returns a disabled log, exactly as the Bun initEventLog swallowed errors.
    pub async fn open(db_path: &Path) -> EventLog {
        match Self::try_open(db_path).await {
            Ok(pool) => EventLog { pool: Some(pool) },
            Err(err) => {
                eprintln!("[bench/logging] init failed: {err}");
                EventLog { pool: None }
            }
        }
    }

    async fn try_open(db_path: &Path) -> Result<SqlitePool, sqlx::Error> {
        // filename() (not a sqlite:// URL) sidesteps URL-encoding the app data
        // path, which on macOS contains spaces ("Application Support").
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            // WAL so the fire-and-forget writers never block on a reader, same
            // reasoning as the Bun store.
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::query(CREATE_TABLE).execute(&pool).await?;
        sqlx::query(CREATE_IDX_TS).execute(&pool).await?;
        sqlx::query(CREATE_IDX_SUBJECT).execute(&pool).await?;
        Ok(pool)
    }

    /// Persist one row for a handled call. Returns immediately: the insert runs
    /// on a detached task and any error is dropped, so logging can never slow
    /// down or fail a tool reply. ts is left to the table DEFAULT (DB clock),
    /// matching the Bun schema.
    pub fn log(&self, event: &ToolEvent) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        let subject = event.tool.clone();
        let args_json = event.args.to_string();
        let status = event.status.clone();
        let duration_ms = event.duration_ms as i64;
        let result_size = event.result_size as i64;
        let error_msg = event.error.clone();
        tokio::spawn(async move {
            let _ = sqlx::query(INSERT)
                .bind(subject)
                .bind(args_json)
                .bind(status)
                .bind(duration_ms)
                .bind(result_size)
                .bind(error_msg)
                .execute(&pool)
                .await;
        });
    }
}

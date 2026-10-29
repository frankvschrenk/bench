//! Repo handbook + session brief (bench.handbook.*, bench.context.brief).
//
// Why: memory events are append-only, which is right for history but wrong for
// "how do I build this repo" — after a few weeks the stream holds three
// contradicting answers and search may surface the oldest. The handbook is the
// *current* state per repo: a handful of keyed entries (map, build, quirks,
// open threads …) that are replaced, not appended.
//
// Storage stays in oosmem and stays immutable: every entry is an event in the
// handbook stream with topic "<repo>|<key>". Replacing an entry appends a new
// event that `supersedes` the old one (oosmem ref kind 1); the newest event per
// topic is the current entry, the older ones are its history. An empty text is
// a tombstone (entry removed). Repo "*" holds entries for every repo (bench
// quirks, working habits).
//
// context.brief bundles what a fresh session needs before touching a repo: the
// handbook (global + repo) and the active tasks of that repo with their latest
// notes — compact, so it can be the first call of every session.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::tools::memory;

/// oosmem stream that holds all handbook entries.
pub const HANDBOOK_STREAM: u64 = 2;
/// oosmem ref kind "supersedes".
const REF_SUPERSEDES: u8 = 1;
const GLOBAL: &str = "*";
const NOTE_CHARS: usize = 700;

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    let result = match op {
        "set" => set(args, ctx).await,
        "get" => get(args, ctx).await,
        "history" => history(args, ctx).await,
        _ => return None,
    };
    Some(result)
}

pub async fn handle_context(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    match op {
        "brief" => Some(brief(args, ctx).await),
        _ => None,
    }
}

#[derive(Deserialize)]
struct SetArgs {
    repo: String,
    key: String,
    text: String,
}

#[derive(Deserialize)]
struct RepoArgs {
    repo: String,
    /// handbook.get: nur diese Schlüssel (leer = alle)
    #[serde(default)]
    keys: Vec<String>,
    /// context.brief: volle Texte statt Kurzform (Standard: kurz)
    #[serde(default)]
    full: bool,
}

/// Kurzform eines Handbuch-Eintrags für context.brief: so viel, dass man
/// entscheiden kann, ob man ihn mit handbook.get {repo, keys} ganz holt.
const SUMMARY_CHARS: usize = 220;
/// Notizen neben der Übergabe nur angerissen
const NOTE_SHORT_CHARS: usize = 240;

#[derive(Deserialize)]
struct HistoryArgs {
    repo: String,
    key: String,
}

/// "~/repro/waki", "/Users/frank/repro/waki/" -> "/Users/frank/repro/waki".
/// "*" stays "*". A bare name ("waki") is matched against the last path
/// component later, so it is kept as is.
pub(crate) fn normalize_repo(repo: &str) -> String {
    let r = repo.trim();
    if r == GLOBAL {
        return GLOBAL.into();
    }
    let r = match r.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", crate::home_dir()),
        None => r.to_string(),
    };
    let t = r.trim_end_matches('/');
    if t.is_empty() { r } else { t.to_string() }
}

/// Same repo if equal, or if one side is a bare name ("waki") that equals the
/// last path component of the other. Symmetric on purpose: entries were once
/// stored under the bare name, and a brief by full path must still see them.
fn same_repo(stored: &str, wanted: &str) -> bool {
    if stored == wanted {
        return true;
    }
    let last = |s: &str| s.rsplit('/').next().map(str::to_string);
    (!wanted.contains('/') && last(stored).as_deref() == Some(wanted))
        || (!stored.contains('/') && stored != GLOBAL && last(wanted).as_deref() == Some(stored))
}

/// A bare name ("waki") given to set is resolved to the full path already used
/// for that repo, so all versions of an entry live under one topic.
fn canonical_repo(evs: &[Value], repo: &str) -> String {
    if repo == GLOBAL || repo.contains('/') {
        return repo.to_string();
    }
    evs.iter()
        .filter_map(parse)
        .map(|e| e.repo)
        .find(|r| r.contains('/') && same_repo(r, repo))
        .unwrap_or_else(|| repo.to_string())
}

fn check_key(key: &str) -> Result<String, ToolError> {
    let k = key.trim();
    if k.is_empty() || k.contains('|') || k.chars().count() > 60 {
        return Err(ToolError::Msg("handbook: key must be 1-60 chars without '|'".into()));
    }
    Ok(k.to_string())
}

/// One entry as stored: newest event of a topic.
#[derive(Clone)]
struct Entry {
    id: u64,
    repo: String,
    key: String,
    text: String,
    at: String,
}

/// All handbook events, newest first (oosmem lists newest first).
async fn events(ctx: &Ctx) -> Result<Vec<Value>, ToolError> {
    let reply = memory::request(
        ctx,
        memory::SUBJECT_STREAM_EVENTS,
        json!({ "stream_id": HANDBOOK_STREAM, "limit": 100000 }),
        "handbook",
    )
    .await?;
    Ok(reply.get("events").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn parse(ev: &Value) -> Option<Entry> {
    let topic = ev.get("topic")?.as_str()?;
    let (repo, key) = topic.split_once('|')?;
    Some(Entry {
        id: ev.get("id")?.as_u64()?,
        repo: repo.to_string(),
        key: key.to_string(),
        text: ev.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
        at: ev.get("created_at").and_then(Value::as_str).unwrap_or_default().to_string(),
    })
}

/// Current entries (newest per key, tombstones dropped) for the repos accepted
/// by `want`. Grouped by key only: `want` selects one repo, whether its events
/// were stored under the full path or the bare name.
fn current(evs: &[Value], want: impl Fn(&str) -> bool) -> Vec<Entry> {
    let mut seen: BTreeMap<String, Entry> = BTreeMap::new();
    for e in evs.iter().filter_map(parse) {
        if !want(&e.repo) {
            continue;
        }
        // newest first: the first one per key wins
        seen.entry(e.key.clone()).or_insert(e);
    }
    seen.into_values().filter(|e| !e.text.trim().is_empty()).collect()
}

fn entry_json(e: &Entry) -> Value {
    json!({ "key": e.key, "text": e.text, "updated_at": e.at, "event_id": e.id })
}

/// Zeilen zusammengezogen und gekürzt (erste Zeilen eines Eintrags)
fn summary(text: &str, n: usize) -> String {
    let flat = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ⏎ ");
    cut(&flat, n)
}

fn entry_short(e: &Entry) -> Value {
    json!({ "key": e.key, "summary": summary(&e.text, SUMMARY_CHARS), "chars": e.text.chars().count(), "updated_at": e.at })
}

async fn set(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: SetArgs = serde_json::from_value(args)?;
    let repo = normalize_repo(&a.repo);
    if repo.is_empty() {
        return Err(ToolError::Msg("handbook_set: repo required (path or \"*\")".into()));
    }
    let key = check_key(&a.key)?;
    let topic = format!("{repo}|{key}");
    if topic.chars().count() > 200 {
        return Err(ToolError::Msg("handbook_set: repo|key longer than 200 chars".into()));
    }
    let evs = events(ctx).await?;
    let repo = canonical_repo(&evs, &repo);
    let topic = format!("{repo}|{key}");
    // the entry it replaces (newest event of this repo+key, tombstone or not,
    // also when it was stored under the bare name)
    let prev = evs.iter().filter_map(parse).find(|e| same_repo(&e.repo, &repo) && e.key == key);
    if prev.as_ref().is_some_and(|p| p.text == a.text) {
        return Ok(json!({ "repo": repo, "key": key, "status": "unchanged", "event_id": prev.unwrap().id }));
    }
    let refs: Vec<Value> = prev
        .iter()
        .map(|p| json!({ "target_event_id": p.id, "kind": REF_SUPERSEDES }))
        .collect();
    let reply = memory::request(
        ctx,
        memory::SUBJECT_APPEND,
        json!({
            "stream_id": HANDBOOK_STREAM,
            "content": a.text,
            "topic": topic,
            "trace": "space",
            "refs": refs,
        }),
        "handbook_set",
    )
    .await?;
    let status = match (&prev, a.text.trim().is_empty()) {
        (_, true) => "removed",
        (Some(_), false) => "replaced",
        (None, false) => "created",
    };
    Ok(json!({
        "repo": repo,
        "key": key,
        "status": status,
        "event_id": reply.get("event_id").cloned().unwrap_or(Value::Null),
        "supersedes": prev.map(|p| p.id),
    }))
}

async fn get(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: RepoArgs = serde_json::from_value(args)?;
    let repo = normalize_repo(&a.repo);
    let evs = events(ctx).await?;
    let entries: Vec<Value> = current(&evs, |r| same_repo(r, &repo))
        .iter()
        .filter(|e| a.keys.is_empty() || a.keys.iter().any(|k| k.trim() == e.key))
        .map(entry_json)
        .collect();
    Ok(json!({ "repo": repo, "entries": entries }))
}

async fn history(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: HistoryArgs = serde_json::from_value(args)?;
    let repo = normalize_repo(&a.repo);
    let key = check_key(&a.key)?;
    let versions: Vec<Value> = events(ctx)
        .await?
        .iter()
        .filter_map(parse)
        .filter(|e| same_repo(&e.repo, &repo) && e.key == key)
        .map(|e| json!({ "event_id": e.id, "text": e.text, "at": e.at }))
        .collect();
    Ok(json!({ "repo": repo, "key": key, "versions": versions }))
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

async fn brief(args: Value, ctx: &Ctx) -> Result<Value, ToolError> {
    let a: RepoArgs = serde_json::from_value(args)?;
    let repo = normalize_repo(&a.repo);
    let evs = events(ctx).await?;
    // Kurzform (Standard, Frank 25.10.2026): Schlüssel + erste Zeilen; ganze
    // Einträge holt man gezielt mit handbook.get {repo, keys}. full=true wie früher.
    let show = |e: &Entry| if a.full { entry_json(e) } else { entry_short(e) };
    let global: Vec<Value> = current(&evs, |r| r == GLOBAL).iter().map(show).collect();
    let handbook: Vec<Value> = current(&evs, |r| r != GLOBAL && same_repo(r, &repo)).iter().map(show).collect();

    // active tasks of this repo with their newest notes (best effort: no DSN
    // or Postgres down must not cost the handbook)
    let tasks = match active_tasks(ctx, &repo, a.full).await {
        Ok(t) => t,
        Err(e) => json!({ "error": e.to_string() }),
    };
    let empty = global.is_empty() && handbook.is_empty() && tasks.as_array().is_some_and(|t| t.is_empty());
    Ok(json!({
        "repo": repo,
        "global": global,
        "handbook": handbook,
        "active_tasks": tasks,
        "hint": if empty {
            "Nothing known about this repo yet. Record lasting knowledge with bench.handbook.set {repo, key, text} \
             (keys like map, build, quirks, conventions, open) and progress with bench.task.start/note."
        } else if !a.full {
            "Short form: handbook entries show key + first lines (summary, chars). Fetch the entries you need in full \
             with bench.handbook.get {repo, keys:[...]} (global ones with repo \"*\"); context.brief {repo, full:true} \
             returns everything. Handbook = current state (replace with bench.handbook.set). Task notes = progress log; \
             full task with bench.task.show. A task's \"handoff\" (in full) is the last session's hand-over: start there; \
             before a session ends write one with bench.task.note kind=handoff (done, next step, open points)."
        } else {
            "Handbook = current state (replace with bench.handbook.set, history via bench.handbook.history). \
             Task notes = progress log; full task with bench.task.show. Keep the handbook true when something lasting changes. \
             A task's \"handoff\" is the last session's hand-over: start there; before a session ends (new chat, \
             context running out) write one with bench.task.note kind=handoff (done, next step, open points)."
        },
    }))
}

async fn active_tasks(ctx: &Ctx, repo: &str, full: bool) -> Result<Value, ToolError> {
    let pool = crate::tools::task::open_db(ctx, "context_brief").await?;
    let rows = sqlx::query("SELECT id, goal, repo, started_at FROM task WHERE status = 'active' ORDER BY started_at DESC")
        .fetch_all(&pool)
        .await?;
    let mut out = Vec::new();
    for r in rows {
        let task_repo: Option<String> = r.try_get("repo")?;
        let Some(task_repo) = task_repo else { continue };
        if !same_repo(&normalize_repo(&task_repo), repo) {
            continue;
        }
        let id: i64 = r.try_get("id")?;
        let note_chars = if full { NOTE_CHARS } else { NOTE_SHORT_CHARS };
        let mut notes: Vec<Value> = sqlx::query(
            "SELECT id, kind, text, created_at FROM task_note WHERE task_id = $1 ORDER BY created_at DESC LIMIT 3",
        )
        .bind(id)
        .fetch_all(&pool)
        .await?
        .iter()
        .map(|n| {
            json!({
                "id": n.try_get::<i64, _>("id").unwrap_or_default(),
                "kind": n.try_get::<String, _>("kind").unwrap_or_default(),
                "text": cut(&n.try_get::<String, _>("text").unwrap_or_default(), note_chars),
                "at": n.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|d| d.to_rfc3339()).unwrap_or_default(),
            })
        })
        .collect();
        // Jüngste Übergabe vollständig: Einstieg für eine neue Sitzung, auch wenn
        // danach weitere Notizen kamen.
        let handoff: Option<Value> = sqlx::query(
            "SELECT id, text, created_at FROM task_note WHERE task_id = $1 AND kind = 'handoff' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&pool)
        .await?
        .map(|n| {
            json!({
                "id": n.try_get::<i64, _>("id").unwrap_or_default(),
                "text": n.try_get::<String, _>("text").unwrap_or_default(),
                "at": n.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|d| d.to_rfc3339()).unwrap_or_default(),
            })
        });
        // Kurzform: die Übergabe steht schon ganz da – nicht noch einmal als Notiz
        if !full {
            if let Some(hid) = handoff.as_ref().and_then(|h| h.get("id")).and_then(Value::as_i64) {
                notes.retain(|n| n.get("id").and_then(Value::as_i64) != Some(hid));
            }
        }
        let started: chrono::DateTime<chrono::Utc> = r.try_get("started_at")?;
        let mut task = json!({
            "task_id": id,
            "goal": r.try_get::<String, _>("goal")?,
            "started_at": started.to_rfc3339(),
            "latest_notes": notes,
        });
        if let Some(h) = handoff {
            task["handoff"] = h;
        }
        out.push(task);
    }
    pool.close().await;
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: u64, topic: &str, content: &str) -> Value {
        json!({ "id": id, "topic": topic, "content": content, "created_at": format!("t{id}") })
    }

    #[test]
    fn newest_entry_wins_and_tombstones_vanish() {
        // newest first, like oosmem
        let evs = vec![
            ev(9, "/r/waki|build", "cargo leptos build"),
            ev(8, "/r/waki|quirks", ""),
            ev(7, "*|bench", "bun liegt in ~/.bun/bin"),
            ev(5, "/r/waki|quirks", "alt"),
            ev(4, "/r/waki|build", "cargo build"),
            ev(3, "/r/other|build", "make"),
            ev(2, "kein-topic", "x"),
        ];
        let cur = current(&evs, |r| same_repo(r, "/r/waki"));
        assert_eq!(cur.len(), 1);
        assert_eq!((cur[0].key.as_str(), cur[0].text.as_str(), cur[0].id), ("build", "cargo leptos build", 9));
        assert_eq!(current(&evs, |r| r == GLOBAL).len(), 1);
        // bare name matches the last path component
        assert_eq!(current(&evs, |r| same_repo(r, "waki")).len(), 1);
        assert!(current(&evs, |r| same_repo(r, "wak")).is_empty());
    }

    #[test]
    fn bare_and_full_repo_names_share_one_entry() {
        // 17.10.2026: entries written as "waki|open" next to "/r/waki|open";
        // a brief by full path showed the stale one.
        let evs = vec![
            ev(207, "waki|open", "neu"),
            ev(174, "/r/waki|open", "alt"),
            ev(100, "*|open", "global"),
        ];
        for wanted in ["/r/waki", "waki"] {
            let cur = current(&evs, |r| r != GLOBAL && same_repo(r, wanted));
            assert_eq!(cur.len(), 1, "{wanted}");
            assert_eq!((cur[0].id, cur[0].text.as_str()), (207, "neu"), "{wanted}");
        }
        assert_eq!(canonical_repo(&evs, "waki"), "/r/waki");
        assert_eq!(canonical_repo(&evs, "neu"), "neu");
        assert!(!same_repo("*", "/r/waki"));
    }

    #[test]
    fn summary_is_short_and_flat() {
        let s = summary("Erste Zeile\n\n- zweite\n", 220);
        assert_eq!(s, "Erste Zeile ⏎ - zweite");
        let long = "x".repeat(500);
        assert_eq!(summary(&long, 220).chars().count(), 221);
        let e = Entry { id: 1, repo: "/r".into(), key: "build".into(), text: long, at: "t".into() };
        assert_eq!(entry_short(&e)["chars"], 500);
    }

    #[test]
    fn repo_paths_are_normalized() {
        let home = crate::home_dir();
        assert_eq!(normalize_repo("~/repro/waki/"), format!("{home}/repro/waki"));
        assert_eq!(normalize_repo(" /a/b// "), "/a/b");
        assert_eq!(normalize_repo("*"), "*");
        assert!(check_key("a|b").is_err() && check_key(" ").is_err() && check_key("build").is_ok());
    }
}

//! The memory store's request handlers and the in-process entry point
//! (`Memory`). Port of the Go `server` package (server.go + handlers.go).
//
// Until 14.11.2026 oosmem was its own process answering oos.cmd.mem.* over
// NATS; it now lives inside the bench. The subject names stay as operation
// keys and the JSON payloads are unchanged. A lock file keeps a second process
// off the same store. Handlers are thin: decode,
// call into store/vector/synth, encode the reply. Per-request failures are
// folded into the reply's `error` field, never propagated.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;

use crate::format::{Event, EventTrace, Ref};
use crate::payload::*;
use crate::store::Store;
use crate::synth::{Episode, OllamaClient, Summarizer};
use crate::vector;

// Subject namespace (verb-first, mirrors the rest of the monorepo).
pub const SUBJECT_APPEND: &str = "oos.cmd.mem.event.append";
pub const SUBJECT_BYID: &str = "oos.cmd.mem.event.byid";
pub const SUBJECT_STREAM_EVENTS: &str = "oos.cmd.mem.stream.events";
pub const SUBJECT_SEARCH: &str = "oos.cmd.mem.search";
pub const SUBJECT_EPISODE_LOOKUP: &str = "oos.cmd.mem.episode.lookup";
pub const SUBJECT_EPISODE_SUMMARIZE: &str = "oos.cmd.mem.episode.summarize";
pub const SUBJECT_HEALTH: &str = "oos.cmd.mem.health";

/// Every operation `Memory::call` understands.
pub const OPERATIONS: &[&str] = &[
    SUBJECT_APPEND,
    SUBJECT_BYID,
    SUBJECT_STREAM_EVENTS,
    SUBJECT_SEARCH,
    SUBJECT_EPISODE_LOOKUP,
    SUBJECT_EPISODE_SUMMARIZE,
    SUBJECT_HEALTH,
];

// Search caps mirror the Go server: default k, hard ceiling, and the episode
// over-fetch multiplier (several events of one stream surface per query, so we
// fetch extra to find k distinct streams).
const SEARCH_K_DEFAULT: usize = 10;
const SEARCH_K_CAP: usize = 256;
const EPISODE_K_DEFAULT: usize = 5;
const EPISODE_K_CAP: usize = 20;
const EPISODE_OVERFETCH: usize = 6;

/// How to open the store. `dim` only applies to a fresh store; an existing
/// store's stored dim always wins. Empty `llm_url` disables episode.summarize.
pub struct MemoryConfig {
    pub data_dir: String,
    pub dim: u16,
    pub llm_url: String,
    pub llm_model: String,
    pub llm_timeout: Duration,
}

/// Shared handler state. The store is single-writer behind a Mutex; handlers
/// lock it for the brief synchronous span of one request and never hold the
/// lock across an await.
struct Handlers {
    store: Arc<Mutex<Store>>,
    dim: u16,
    summ: Option<Arc<Summarizer>>,
    llm_model: String,
}

fn json<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_else(|_| br#"{"error":"oosmem: encode reply failed"}"#.to_vec())
}

fn err_json(msg: String) -> Vec<u8> {
    json(&serde_json::json!({ "error": msg }))
}

impl Handlers {
    async fn dispatch(&self, subject: &str, data: &[u8]) -> Vec<u8> {
        match subject {
            SUBJECT_APPEND => self.append(data),
            SUBJECT_BYID => self.by_id(data),
            SUBJECT_STREAM_EVENTS => self.stream_events(data),
            SUBJECT_SEARCH => self.search(data),
            SUBJECT_EPISODE_LOOKUP => self.episode_lookup(data),
            SUBJECT_EPISODE_SUMMARIZE => self.summarize(data).await,
            SUBJECT_HEALTH => self.health(),
            other => err_json(format!("oosmem: unknown subject {other}")),
        }
    }

    // ── append ───────────────────────────────────────────

    fn append(&self, data: &[u8]) -> Vec<u8> {
        let req: AppendRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => return json(&AppendReply { error: Some(format!("append: decode: {e}")), ..Default::default() }),
        };
        if req.stream_id == 0 {
            return json(&AppendReply { error: Some("append: stream_id required".into()), ..Default::default() });
        }
        // Trace defaults to unknown when omitted; a non-empty but invalid value
        // is a client bug -- reject rather than store a wrong perspective.
        let trace = if req.trace.is_empty() {
            EventTrace::Unknown
        } else {
            match EventTrace::parse(&req.trace) {
                (t, true) => t,
                _ => {
                    return json(&AppendReply {
                        error: Some("append: invalid trace, want one of space|time|action|unknown".into()),
                        ..Default::default()
                    })
                }
            }
        };

        let vec = vector::build(&req.content, &req.topic, self.dim as usize);
        let refs = req
            .refs
            .iter()
            .map(|r| Ref { kind: r.kind, target_event_id: r.target_event_id, target_offset_cache: 0 })
            .collect();
        let ev = Event {
            id: 0,
            stream_id: req.stream_id,
            created_at: Utc::now(),
            closed_at: None,
            event_type: req.typ,
            outcome: req.outcome,
            confidence: req.confidence,
            flags: req.flags,
            trace: trace.as_u8(),
            refs,
            content: req.content,
            topic: req.topic,
            cost: req.cost,
            vector: vec,
            file_offset: 0,
        };

        let result = { self.store.lock().unwrap().append_event(ev) };
        match result {
            Ok(id) => json(&AppendReply { event_id: id, ..Default::default() }),
            Err(e) => json(&AppendReply { error: Some(format!("append: store: {e}")), ..Default::default() }),
        }
    }

    // ── byid ────────────────────────────────────────────

    fn by_id(&self, data: &[u8]) -> Vec<u8> {
        let req: ByIdRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => return json(&ByIdReply { error: Some(format!("byid: decode: {e}")), ..Default::default() }),
        };
        let store = self.store.lock().unwrap();
        let event = store.by_id(req.event_id).map(to_event_json);
        json(&ByIdReply { event, ..Default::default() })
    }

    // ── stream.events ────────────────────────────────────

    fn stream_events(&self, data: &[u8]) -> Vec<u8> {
        let req: StreamEventsRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => {
                return json(&StreamEventsReply { error: Some(format!("stream.events: decode: {e}")), ..Default::default() })
            }
        };
        let store = self.store.lock().unwrap();
        let mut ids = store.stream_event_ids(req.stream_id);
        ids.sort_unstable_by(|a, b| b.cmp(a)); // newest first
        let mut out = Vec::new();
        for id in ids {
            if req.before != 0 && id >= req.before {
                continue;
            }
            if let Some(ev) = store.by_id(id) {
                out.push(to_event_json(ev));
                if req.limit > 0 && out.len() >= req.limit {
                    break;
                }
            }
        }
        json(&StreamEventsReply { events: out, ..Default::default() })
    }

    // ── search ──────────────────────────────────────────

    fn search(&self, data: &[u8]) -> Vec<u8> {
        let req: SearchRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => return json(&SearchReply { error: Some(format!("search: decode: {e}")), ..Default::default() }),
        };
        let k = clamp_k(req.k, SEARCH_K_DEFAULT, SEARCH_K_CAP);
        let stream = (req.stream != 0).then_some(req.stream);
        let qvec = vector::build_query(&req.query, self.dim as usize);

        let store = self.store.lock().unwrap();
        // Exact scan already filters by stream and ranks; over-fetch only to
        // make room for superseded events that are dropped below.
        let fetch = if req.include_superseded { k } else { (k * 3).min(SEARCH_K_CAP) };
        let hits = store.search(&qvec, fetch, stream);
        let out: Vec<SearchHit> = hits
            .into_iter()
            .filter(|(id, _)| req.include_superseded || !store.is_superseded(*id))
            .take(k)
            .filter_map(|(id, score)| store.by_id(id).map(|ev| SearchHit { event: to_event_json(ev), score }))
            .collect();
        json(&SearchReply { hits: out, ..Default::default() })
    }

    // ── episode.lookup ───────────────────────────────────

    fn episode_lookup(&self, data: &[u8]) -> Vec<u8> {
        let req: EpisodeLookupRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => {
                return json(&EpisodeLookupReply { error: Some(format!("episode.lookup: decode: {e}")), ..Default::default() })
            }
        };
        let k = clamp_k(req.k, EPISODE_K_DEFAULT, EPISODE_K_CAP);
        let fetch = (k * EPISODE_OVERFETCH).min(SEARCH_K_CAP);
        let qvec = vector::build_query(&req.query, self.dim as usize);

        let store = self.store.lock().unwrap();
        let hits = store.search(&qvec, fetch, None);

        // Aggregate hit scores per stream, recording first-seen order so score
        // ties resolve to closer-to-top-of-results first.
        struct Agg {
            stream_id: u64,
            score: f32,
            hit_count: usize,
            first_seen: usize,
        }
        let mut aggs: Vec<Agg> = Vec::new();
        let mut index: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for (id, score) in hits {
            let Some(ev) = store.by_id(id) else { continue };
            match index.get(&ev.stream_id) {
                Some(&i) => {
                    aggs[i].score += score;
                    aggs[i].hit_count += 1;
                }
                None => {
                    index.insert(ev.stream_id, aggs.len());
                    aggs.push(Agg { stream_id: ev.stream_id, score, hit_count: 1, first_seen: aggs.len() });
                }
            }
        }
        aggs.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.first_seen.cmp(&b.first_seen)));
        aggs.truncate(k);

        let mut episodes = Vec::with_capacity(aggs.len());
        for a in &aggs {
            let mut ep = EpisodeJson { stream_id: a.stream_id, score: a.score, hit_count: a.hit_count, ..Default::default() };
            if let Some(st) = store.stream(a.stream_id) {
                fill_episode_header(&mut ep, st);
            }
            let mut ids = store.stream_event_ids(a.stream_id);
            ids.sort_unstable();
            for id in ids {
                let Some(ev) = store.by_id(id) else { continue };
                let wire = to_event_json(ev);
                match ev.trace {
                    1 => ep.space.push(wire),
                    2 => ep.time.push(wire),
                    3 => ep.action.push(wire),
                    _ => ep.untracked.push(wire),
                }
            }
            episodes.push(ep);
        }
        json(&EpisodeLookupReply { episodes, ..Default::default() })
    }

    // ── health ───────────────────────────────────────────

    fn health(&self) -> Vec<u8> {
        let store = self.store.lock().unwrap();
        json(&HealthReply {
            events: store.event_count(),
            streams: store.stream_count(),
            // No separate ANN index: search is an in-RAM exact scan that is
            // always available, so we report the capability as present.
            has_index: true,
            has_llm: self.summ.is_some(),
            ..Default::default()
        })
    }

    // ── episode.summarize ──────────────────────────────────

    async fn summarize(&self, data: &[u8]) -> Vec<u8> {
        let req: EpisodeSummarizeRequest = match serde_json::from_slice(data) {
            Ok(r) => r,
            Err(e) => {
                return json(&EpisodeSummarizeReply { error: Some(format!("episode.summarize: decode: {e}")), ..Default::default() })
            }
        };
        let Some(summ) = self.summ.clone() else {
            return json(&EpisodeSummarizeReply {
                error: Some("episode.summarize: llm not configured".into()),
                stream_id: req.stream_id,
                ..Default::default()
            });
        };
        if req.stream_id == 0 {
            return json(&EpisodeSummarizeReply { error: Some("episode.summarize: stream_id required".into()), ..Default::default() });
        }

        // Build the episode under the lock, then release it before the await.
        let episode = {
            let store = self.store.lock().unwrap();
            if store.stream(req.stream_id).is_none() {
                return json(&EpisodeSummarizeReply {
                    error: Some(format!("episode.summarize: stream {} not found", req.stream_id)),
                    stream_id: req.stream_id,
                    ..Default::default()
                });
            }
            build_episode(&store, req.stream_id)
        };

        let model_used = if req.model.is_empty() { self.llm_model.clone() } else { req.model.clone() };
        let start = Instant::now();
        let result = summ.summarize(&episode, &req.language, req.max_chars, &req.model).await;
        let duration_ms = start.elapsed().as_millis() as i64;

        match result {
            Ok(summary) => json(&EpisodeSummarizeReply {
                stream_id: req.stream_id,
                summary,
                model: model_used,
                duration_ms,
                ..Default::default()
            }),
            Err(e) => json(&EpisodeSummarizeReply {
                error: Some(format!("episode.summarize: {e}")),
                stream_id: req.stream_id,
                model: model_used,
                duration_ms,
                ..Default::default()
            }),
        }
    }
}

// Clamp a requested k: 0 -> default, above cap -> cap.
fn clamp_k(k: usize, default: usize, cap: usize) -> usize {
    if k == 0 {
        default
    } else {
        k.min(cap)
    }
}

// Build a synth Episode from the store: events of the stream, id-ascending,
// bucketed by trace (Time thereby ends up oldest-first).
fn build_episode(store: &Store, stream_id: u64) -> Episode {
    let mut ep = Episode { stream_id, ..Default::default() };
    if let Some(st) = store.stream(stream_id) {
        ep.name = st.name.clone();
    }
    let mut ids = store.stream_event_ids(stream_id);
    ids.sort_unstable();
    for id in ids {
        let Some(ev) = store.by_id(id) else { continue };
        match ev.trace {
            1 => ep.space.push(ev.clone()),
            2 => ep.time.push(ev.clone()),
            3 => ep.action.push(ev.clone()),
            _ => ep.untracked.push(ev.clone()),
        }
    }
    ep
}

/// The memory store, opened in-process. Callers hand an operation name (the
/// former oos.cmd.mem.* subject, kept as the key so the wire payloads stay
/// unchanged) and a JSON payload, and get the JSON reply back -- exactly what
/// the NATS responder used to publish.
pub struct Memory {
    handlers: Arc<Handlers>,
    data_dir: String,
    // Held for the lifetime of the store: a second process (an old oosmem
    // under launchd, a second bench) must not write the same files.
    _lock: std::fs::File,
}

impl Memory {
    /// Open (or create) the store and set up the optional summarizer.
    pub fn open(cfg: MemoryConfig) -> anyhow::Result<Memory> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(std::path::Path::new(&cfg.data_dir).join("oosmem.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "{} wird schon von einem anderen Prozess benutzt (läuft noch der alte oosmem-Dienst?)",
                cfg.data_dir
            ),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }

        let (store, report) = Store::open_or_create(&cfg.data_dir, cfg.dim)?;
        let dim = store.vector_dim();
        eprintln!(
            "oosmem: loaded {} events, {} streams, {} inverse-refs, repaired {} idx (dim={dim})",
            report.events_loaded, report.streams_built, report.inverse_refs, report.repaired_idx
        );

        let summ = if cfg.llm_url.is_empty() {
            None
        } else {
            eprintln!("oosmem: summarizer enabled (url={}, model={})", cfg.llm_url, cfg.llm_model);
            Some(Arc::new(Summarizer::new(
                OllamaClient::new(&cfg.llm_url, &cfg.llm_model, cfg.llm_timeout),
                &cfg.llm_model,
            )))
        };

        let store = Arc::new(Mutex::new(store));
        let handlers = Arc::new(Handlers { store, dim, summ, llm_model: cfg.llm_model.clone() });
        Ok(Memory { handlers, data_dir: cfg.data_dir, _lock: lock })
    }

    /// Run one operation (see the SUBJECT_* constants) and return the JSON reply.
    /// Failures are folded into the reply's `error` field, never returned.
    pub async fn call(&self, op: &str, payload: &[u8]) -> Vec<u8> {
        self.handlers.dispatch(op, payload).await
    }

    /// Rewrite the header with current counts. Events are durable per append;
    /// this only keeps meta.bin fresh (periodically and on shutdown).
    pub fn snapshot(&self) -> anyhow::Result<()> {
        self.handlers.store.lock().unwrap().snapshot()?;
        Ok(())
    }

    /// Folder the store lives in.
    pub fn data_dir(&self) -> &str {
        &self.data_dir
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        if let Err(e) = self.snapshot() {
            eprintln!("oosmem: final snapshot failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &std::path::Path) -> MemoryConfig {
        MemoryConfig {
            data_dir: dir.to_string_lossy().into_owned(),
            dim: 128,
            llm_url: String::new(),
            llm_model: String::new(),
            llm_timeout: Duration::from_secs(1),
        }
    }

    fn value(bytes: Vec<u8>) -> serde_json::Value {
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn im_prozess_schreiben_lesen_und_sperre() {
        let dir = std::env::temp_dir().join(format!("oosmem-test-{}-{}", std::process::id(), Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        {
            let mem = Memory::open(cfg(&dir)).unwrap();
            // A second open on the same folder must be refused while the first lives.
            assert!(Memory::open(cfg(&dir)).is_err());

            let r = value(mem.call(SUBJECT_APPEND, br#"{"stream_id":7,"content":"Bench baut mit cargo build","topic":"bauen","trace":"action"}"#).await);
            assert!(r.get("error").is_none(), "{r}");
            let id = r["event_id"].as_u64().unwrap();

            let h = value(mem.call(SUBJECT_HEALTH, b"{}").await);
            assert_eq!(h["events"], 1);

            let l = value(mem.call(SUBJECT_STREAM_EVENTS, br#"{"stream_id":7,"limit":5}"#).await);
            assert_eq!(l["events"][0]["id"].as_u64(), Some(id), "{l}");

            let u = value(mem.call("oos.cmd.mem.nope", b"{}").await);
            assert!(u["error"].as_str().unwrap().contains("unknown"));
        }
        // Dropped: lock released, the event survives a reopen.
        let mem = Memory::open(cfg(&dir)).unwrap();
        assert_eq!(value(mem.call(SUBJECT_HEALTH, b"{}").await)["events"], 1);
        drop(mem);
        let _ = std::fs::remove_dir_all(dir);
    }
}

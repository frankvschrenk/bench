//! The NATS server: open the store, subscribe the seven oos.cmd.mem.* subjects
//! as request-reply responders, run a periodic snapshot loop, and shut down
//! cleanly on SIGINT/SIGTERM with a final snapshot. Port of the Go `server`
//! package (server.go + handlers.go).
//
// Subjects are queue-subscribed under the group "oosmem": with a single
// instance this behaves like a plain subscription, but it guarantees that if a
// second instance is ever started by mistake, append is not executed twice
// (the duplicate-write fan-out that bit oosai). Handlers are thin: decode,
// call into store/vector/synth, encode the reply. Per-request failures are
// folded into the reply's `error` field, never propagated.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use futures::StreamExt;
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

const ALL_SUBJECTS: &[&str] = &[
    SUBJECT_APPEND,
    SUBJECT_BYID,
    SUBJECT_STREAM_EVENTS,
    SUBJECT_SEARCH,
    SUBJECT_EPISODE_LOOKUP,
    SUBJECT_EPISODE_SUMMARIZE,
    SUBJECT_HEALTH,
];

const QUEUE_GROUP: &str = "oosmem";

// Search caps mirror the Go server: default k, hard ceiling, and the episode
// over-fetch multiplier (several events of one stream surface per query, so we
// fetch extra to find k distinct streams).
const SEARCH_K_DEFAULT: usize = 10;
const SEARCH_K_CAP: usize = 256;
const EPISODE_K_DEFAULT: usize = 5;
const EPISODE_K_CAP: usize = 20;
const EPISODE_OVERFETCH: usize = 6;

/// Boot-time configuration, assembled by main from flags + dim resolution.
pub struct ServerConfig {
    pub data_dir: String,
    pub nats_url: String,
    pub dim: u16,
    pub snapshot_every: Duration,
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
        // Exact scan already filters by stream and ranks, so no over-fetch.
        let hits = store.search(&qvec, k, stream);
        let out: Vec<SearchHit> = hits
            .into_iter()
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

/// Open the store, connect to NATS, subscribe every subject, run the snapshot
/// loop, and block until SIGINT/SIGTERM. Takes a final snapshot on the way out.
pub async fn run(cfg: ServerConfig) -> anyhow::Result<()> {
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
    let handlers = Arc::new(Handlers { store: store.clone(), dim, summ, llm_model: cfg.llm_model.clone() });

    let nats_url = if cfg.nats_url.is_empty() { "nats://127.0.0.1:4222".to_string() } else { cfg.nats_url.clone() };
    let client = async_nats::ConnectOptions::new().name("oosmem").connect(&nats_url).await?;

    for &subj in ALL_SUBJECTS {
        let sub = client.queue_subscribe(subj, QUEUE_GROUP.to_string()).await?;
        spawn_responder(handlers.clone(), client.clone(), sub, subj);
    }
    eprintln!("oosmem: ready on {nats_url}, subjects {ALL_SUBJECTS:?}");

    // Snapshot loop + signal wait. The first interval tick fires immediately;
    // consume it so we do not snapshot the instant we boot.
    let mut interval = tokio::time::interval(cfg.snapshot_every);
    interval.tick().await;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            _ = interval.tick() => {
                if let Err(e) = store.lock().unwrap().snapshot() {
                    eprintln!("oosmem: snapshot failed: {e}");
                }
            }
        }
    }

    eprintln!("oosmem: shutting down");
    let _ = client.flush().await;
    store.lock().unwrap().snapshot()?;
    Ok(())
}

// Spawn a task that answers every request on one subscriber by dispatching to
// the handler for `subject` and publishing the reply to msg.reply.
fn spawn_responder(
    handlers: Arc<Handlers>,
    client: async_nats::Client,
    mut sub: async_nats::Subscriber,
    subject: &'static str,
) {
    tokio::spawn(async move {
        while let Some(msg) = sub.next().await {
            let reply_to = match msg.reply {
                Some(r) => r,
                None => continue, // not a request; nothing to answer
            };
            let body = handlers.dispatch(subject, &msg.payload).await;
            if let Err(e) = client.publish(reply_to, body.into()).await {
                eprintln!("oosmem: respond failed on {subject}: {e}");
            }
        }
    });
}

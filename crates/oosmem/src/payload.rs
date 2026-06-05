//! NATS request/reply wire types. JSON, snake_case, mirroring the Go
//! `payload.go` so existing callers (bench memory tools, sessions) see the
//! same shapes. This is the boundary where internal types (Event, Stream,
//! chrono times) translate to and from JSON.
//
// Vectors are never put on the wire: replies carry content/topic/metadata and
// callers re-vectorise client-side if needed, which keeps replies small and
// avoids leaking the token-hash space. Times serialise as RFC3339 (chrono's
// serde default). `error` is absent on success; an empty/absent error means OK.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::format::{Event, EventTrace};
use crate::store::Stream;

// ── Shared shapes ────────────────────────────────────────────

/// Wire shape of a Ref. `kind` is the raw RefKind byte (0..4).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RefJson {
    pub target_event_id: u64,
    pub kind: u8,
}

/// Wire shape of an Event. No vector; trace as a string for readability.
#[derive(Debug, Clone, Serialize)]
pub struct EventJson {
    pub id: u64,
    pub stream_id: u64,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(rename = "type")]
    pub typ: u8,
    pub outcome: u8,
    pub confidence: u8,
    pub flags: u8,
    pub trace: String,
    pub content: String,
    pub topic: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cost: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<RefJson>,
}

/// Convert a stored Event to its wire shape, dropping the vector and offset
/// cache. Trace maps via the byte -> string table.
pub fn to_event_json(e: &Event) -> EventJson {
    EventJson {
        id: e.id,
        stream_id: e.stream_id,
        created_at: e.created_at,
        closed_at: e.closed_at,
        typ: e.event_type,
        outcome: e.outcome,
        confidence: e.confidence,
        flags: e.flags,
        trace: EventTrace::from_u8(e.trace).as_str().to_string(),
        content: e.content.clone(),
        topic: e.topic.clone(),
        cost: e.cost.clone(),
        refs: e
            .refs
            .iter()
            .map(|r| RefJson { target_event_id: r.target_event_id, kind: r.kind })
            .collect(),
    }
}

// ── event.append ─────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct AppendRequest {
    pub stream_id: u64,
    pub content: String,
    pub topic: String,
    pub cost: String,
    #[serde(rename = "type")]
    pub typ: u8,
    pub outcome: u8,
    pub confidence: u8,
    pub flags: u8,
    pub trace: String,
    pub refs: Vec<RefJson>,
}

#[derive(Debug, Serialize, Default)]
pub struct AppendReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub event_id: u64,
}

// ── event.byid ───────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ByIdRequest {
    pub event_id: u64,
}

#[derive(Debug, Serialize, Default)]
pub struct ByIdReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// None (null) when the id is unknown -- not an error condition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<EventJson>,
}

// ── stream.events ────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct StreamEventsRequest {
    pub stream_id: u64,
    pub limit: usize,
    /// event_id exclusive upper bound; 0 means no bound (newest included).
    pub before: u64,
}

#[derive(Debug, Serialize, Default)]
pub struct StreamEventsReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub events: Vec<EventJson>,
}

// ── search ──────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct SearchRequest {
    pub query: String,
    pub k: usize,
    /// optional stream filter; 0 means all streams.
    pub stream: u64,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub event: EventJson,
    pub score: f32,
}

#[derive(Debug, Serialize, Default)]
pub struct SearchReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub hits: Vec<SearchHit>,
}

// ── episode.lookup ───────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct EpisodeLookupRequest {
    pub query: String,
    pub k: usize,
}

/// One episode (today: a stream) with its events bucketed by trace. Time is
/// oldest-first; all buckets are id-ascending here. Score is the summed hit
/// score of the episode's events that surfaced in the query search.
#[derive(Debug, Serialize, Default)]
pub struct EpisodeJson {
    pub stream_id: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub opened_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    pub score: f32,
    pub hit_count: usize,
    pub space: Vec<EventJson>,
    pub time: Vec<EventJson>,
    pub action: Vec<EventJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub untracked: Vec<EventJson>,
}

#[derive(Debug, Serialize, Default)]
pub struct EpisodeLookupReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub episodes: Vec<EpisodeJson>,
}

// ── episode.summarize ────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct EpisodeSummarizeRequest {
    pub stream_id: u64,
    pub language: String,
    pub max_chars: usize,
    pub model: String,
}

#[derive(Debug, Serialize, Default)]
pub struct EpisodeSummarizeReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub stream_id: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub duration_ms: i64,
}

// ── health ──────────────────────────────────────────────

#[derive(Debug, Serialize, Default)]
pub struct HealthReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub events: usize,
    pub streams: usize,
    pub has_index: bool,
    pub has_llm: bool,
}

/// Episode metadata helper used by the lookup handler to fill an EpisodeJson
/// header from a Stream.
pub fn fill_episode_header(ep: &mut EpisodeJson, s: &Stream) {
    ep.name = s.name.clone();
    ep.opened_at = s.opened_at;
    ep.closed_at = s.closed_at;
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

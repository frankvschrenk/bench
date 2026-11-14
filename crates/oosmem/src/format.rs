//! On-disk binary layout for the oosmem store: the data types, magic
//! numbers, enum mappings, and the little-endian codec. Ported byte-for-byte
//! from the Go `format` package so this build reads an existing store
//! (meta.bin / events.idx / events.bin) without migration.
//
// Faithfulness note: the structural byte fields (type/outcome/confidence/
// flags/trace/ref-kind) are kept as raw u8 on the Event/Ref structs, exactly
// as Go stored them (where these were uint8 type aliases). Unknown byte
// values therefore round-trip losslessly. The named enums below are helpers
// the upper layers use at the JSON boundary; they never gate decoding.

use chrono::{DateTime, Utc};

// ─── Constants (binding reference; mirror the Go types.go) ─────────────────

/// First four bytes of meta.bin: "OMEM".
pub const FILE_MAGIC: u32 = 0x4F4D_454D;

/// Layout version. Readers refuse stores newer than this. Version 2 added the
/// per-event Trace byte at offset 40 (header grew 48 -> 49).
pub const FORMAT_VERSION: u16 = 2;

/// Vector dimension for a freshly created store (stored in the meta header).
pub const DEFAULT_VECTOR_DIM: u16 = 1024;

pub const META_HEADER_SIZE: usize = 64;
pub const IDX_RECORD_SIZE: usize = 28;
pub const EVENT_HEADER_SIZE: usize = 49;
pub const REF_SIZE: usize = 17;

pub const MAX_FIELD_LEN: usize = u16::MAX as usize; // content/topic/cost/refs ceiling

// ─── Errors ──────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("oosmem/format: buffer too short: need {need}, got {got}")]
    ShortBuffer { need: usize, got: usize },
    #[error("oosmem/format: meta magic mismatch: got {got:#010x}, want {want:#010x}")]
    BadMagic { got: u32, want: u32 },
    #[error("oosmem/format: meta version {got} newer than supported {supported}")]
    FutureVersion { got: u16, supported: u16 },
    #[error("oosmem/format: field {field} = {len} exceeds u16 limit")]
    FieldTooLarge { field: &'static str, len: usize },
    #[error("oosmem/format: vector length {got} does not match configured dim {want}")]
    VectorDimension { got: usize, want: u16 },
    #[error("oosmem/format: record_len {record_len} disagrees with actual size {actual}")]
    RecordLen { record_len: usize, actual: usize },
}

type Result<T> = std::result::Result<T, FormatError>;

// ─── Semantic enums (helpers; decoding never depends on them) ─────────────

/// Which perspective of an episode an event belongs to. Space = stage/intent,
/// Time = neutral chronicle, Action = what was done + immediate result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTrace {
    Unknown,
    Space,
    Time,
    Action,
}

impl EventTrace {
    pub fn from_u8(b: u8) -> EventTrace {
        match b {
            1 => EventTrace::Space,
            2 => EventTrace::Time,
            3 => EventTrace::Action,
            _ => EventTrace::Unknown,
        }
    }
    pub fn as_u8(self) -> u8 {
        match self {
            EventTrace::Unknown => 0,
            EventTrace::Space => 1,
            EventTrace::Time => 2,
            EventTrace::Action => 3,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EventTrace::Space => "space",
            EventTrace::Time => "time",
            EventTrace::Action => "action",
            EventTrace::Unknown => "unknown",
        }
    }
    /// Inverse of `as_str`. The bool is false for unrecognised input (and for
    /// the empty string), so callers can tell an explicit "unknown" from a
    /// malformed value — same contract as the Go ParseEventTrace.
    pub fn parse(s: &str) -> (EventTrace, bool) {
        match s {
            "space" => (EventTrace::Space, true),
            "time" => (EventTrace::Time, true),
            "action" => (EventTrace::Action, true),
            "unknown" => (EventTrace::Unknown, true),
            _ => (EventTrace::Unknown, false),
        }
    }
}

/// Directed reference kind between two events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Unknown,
    Supersedes,
    Causes,
    CausedBy,
    Informs,
}

impl RefKind {
    pub fn from_u8(b: u8) -> RefKind {
        match b {
            1 => RefKind::Supersedes,
            2 => RefKind::Causes,
            3 => RefKind::CausedBy,
            4 => RefKind::Informs,
            _ => RefKind::Unknown,
        }
    }
    pub fn as_u8(self) -> u8 {
        match self {
            RefKind::Unknown => 0,
            RefKind::Supersedes => 1,
            RefKind::Causes => 2,
            RefKind::CausedBy => 3,
            RefKind::Informs => 4,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            RefKind::Supersedes => "supersedes",
            RefKind::Causes => "causes",
            RefKind::CausedBy => "caused_by",
            RefKind::Informs => "informs",
            RefKind::Unknown => "unknown",
        }
    }
}

/// FlagClosedBySupersede: event implicitly closed because another supersedes it.
pub const FLAG_CLOSED_BY_SUPERSEDE: u8 = 1 << 0;

// ─── Records ────────────────────────────────────────────────

/// A directed edge from one event to a target, stored inline in the record.
/// `target_offset_cache` is a performance hint and may be zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub kind: u8,
    pub target_event_id: u64,
    pub target_offset_cache: u64,
}

/// Fixed 64-byte header of meta.bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaHeader {
    pub magic: u32,
    pub version: u16,
    pub vector_dim: u16,
    pub last_event_id: u64,
    pub event_count: u64,
    pub stream_count: u64,
    pub created_at: DateTime<Utc>,
    pub last_snapshot_at: Option<DateTime<Utc>>,
}

/// One 28-byte entry of events.idx, mirrored in RAM at start-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdxRecord {
    pub event_id: u64,
    pub file_offset: u64,
    pub record_len: u32,
    pub created_at: DateTime<Utc>,
}

/// In-memory representation of one events.bin record. The structural byte
/// fields stay raw u8 for lossless round-trip; interpret via the enums above.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: u64,
    pub stream_id: u64,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub event_type: u8,
    pub outcome: u8,
    pub confidence: u8,
    pub flags: u8,
    pub trace: u8,
    pub refs: Vec<Ref>,
    pub content: String,
    pub topic: String,
    pub cost: String,
    pub vector: Vec<f32>,
    /// Byte offset in events.bin; set by the store on load/append, not encoded.
    pub file_offset: u64,
}

// ─── Time helpers (0 nanos is the NULL sentinel, as in Go) ────────────────

fn time_to_nanos(t: DateTime<Utc>) -> i64 {
    t.timestamp_nanos_opt().unwrap_or(0)
}

fn opt_time_to_nanos(t: Option<DateTime<Utc>>) -> i64 {
    t.map(time_to_nanos).unwrap_or(0)
}

fn nanos_to_time(ns: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(ns)
}

fn nanos_to_opt_time(ns: i64) -> Option<DateTime<Utc>> {
    if ns == 0 {
        None
    } else {
        Some(nanos_to_time(ns))
    }
}

// ─── Little-endian read helpers ───────────────────────────────────

fn rd_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn rd_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn rd_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[0..8]);
    u64::from_le_bytes(a)
}

// ─── MetaHeader codec ─────────────────────────────────────────

pub fn encode_meta_header(h: &MetaHeader) -> [u8; META_HEADER_SIZE] {
    let mut buf = [0u8; META_HEADER_SIZE];
    buf[0..4].copy_from_slice(&h.magic.to_le_bytes());
    buf[4..6].copy_from_slice(&h.version.to_le_bytes());
    buf[6..8].copy_from_slice(&h.vector_dim.to_le_bytes());
    buf[8..16].copy_from_slice(&h.last_event_id.to_le_bytes());
    buf[16..24].copy_from_slice(&h.event_count.to_le_bytes());
    buf[24..32].copy_from_slice(&h.stream_count.to_le_bytes());
    buf[32..40].copy_from_slice(&time_to_nanos(h.created_at).to_le_bytes());
    buf[40..48].copy_from_slice(&opt_time_to_nanos(h.last_snapshot_at).to_le_bytes());
    // bytes 48..64 reserved, left zero.
    buf
}

pub fn decode_meta_header(buf: &[u8]) -> Result<MetaHeader> {
    if buf.len() < META_HEADER_SIZE {
        return Err(FormatError::ShortBuffer { need: META_HEADER_SIZE, got: buf.len() });
    }
    let magic = rd_u32(&buf[0..4]);
    if magic != FILE_MAGIC {
        return Err(FormatError::BadMagic { got: magic, want: FILE_MAGIC });
    }
    let version = rd_u16(&buf[4..6]);
    if version > FORMAT_VERSION {
        return Err(FormatError::FutureVersion { got: version, supported: FORMAT_VERSION });
    }
    Ok(MetaHeader {
        magic,
        version,
        vector_dim: rd_u16(&buf[6..8]),
        last_event_id: rd_u64(&buf[8..16]),
        event_count: rd_u64(&buf[16..24]),
        stream_count: rd_u64(&buf[24..32]),
        created_at: nanos_to_time(rd_u64(&buf[32..40]) as i64),
        last_snapshot_at: nanos_to_opt_time(rd_u64(&buf[40..48]) as i64),
    })
}

// ─── IdxRecord codec ──────────────────────────────────────────

pub fn encode_idx_record(r: &IdxRecord) -> [u8; IDX_RECORD_SIZE] {
    let mut buf = [0u8; IDX_RECORD_SIZE];
    buf[0..8].copy_from_slice(&r.event_id.to_le_bytes());
    buf[8..16].copy_from_slice(&r.file_offset.to_le_bytes());
    buf[16..20].copy_from_slice(&r.record_len.to_le_bytes());
    buf[20..28].copy_from_slice(&time_to_nanos(r.created_at).to_le_bytes());
    buf
}

pub fn decode_idx_record(buf: &[u8]) -> Result<IdxRecord> {
    if buf.len() < IDX_RECORD_SIZE {
        return Err(FormatError::ShortBuffer { need: IDX_RECORD_SIZE, got: buf.len() });
    }
    Ok(IdxRecord {
        event_id: rd_u64(&buf[0..8]),
        file_offset: rd_u64(&buf[8..16]),
        record_len: rd_u32(&buf[16..20]),
        created_at: nanos_to_time(rd_u64(&buf[20..28]) as i64),
    })
}

// ─── Event codec ─────────────────────────────────────────────

/// Serialized size of `ev` at `vector_dim`, including the leading record_len.
pub fn event_record_size(ev: &Event, vector_dim: u16) -> usize {
    EVENT_HEADER_SIZE
        + ev.refs.len() * REF_SIZE
        + ev.content.len()
        + ev.topic.len()
        + ev.cost.len()
        + vector_dim as usize * 4
}

pub fn encode_event(ev: &Event, vector_dim: u16) -> Result<Vec<u8>> {
    if ev.refs.len() > MAX_FIELD_LEN {
        return Err(FormatError::FieldTooLarge { field: "refs", len: ev.refs.len() });
    }
    if ev.content.len() > MAX_FIELD_LEN {
        return Err(FormatError::FieldTooLarge { field: "content", len: ev.content.len() });
    }
    if ev.topic.len() > MAX_FIELD_LEN {
        return Err(FormatError::FieldTooLarge { field: "topic", len: ev.topic.len() });
    }
    if ev.cost.len() > MAX_FIELD_LEN {
        return Err(FormatError::FieldTooLarge { field: "cost", len: ev.cost.len() });
    }
    if ev.vector.len() != vector_dim as usize {
        return Err(FormatError::VectorDimension { got: ev.vector.len(), want: vector_dim });
    }

    let record_len = event_record_size(ev, vector_dim);
    let mut buf = vec![0u8; record_len];

    buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
    buf[4..12].copy_from_slice(&ev.id.to_le_bytes());
    buf[12..20].copy_from_slice(&ev.stream_id.to_le_bytes());
    buf[20..28].copy_from_slice(&time_to_nanos(ev.created_at).to_le_bytes());
    buf[28..36].copy_from_slice(&opt_time_to_nanos(ev.closed_at).to_le_bytes());
    buf[36] = ev.event_type;
    buf[37] = ev.outcome;
    buf[38] = ev.confidence;
    buf[39] = ev.flags;
    buf[40] = ev.trace;
    buf[41..43].copy_from_slice(&(ev.refs.len() as u16).to_le_bytes());
    buf[43..45].copy_from_slice(&(ev.content.len() as u16).to_le_bytes());
    buf[45..47].copy_from_slice(&(ev.topic.len() as u16).to_le_bytes());
    buf[47..49].copy_from_slice(&(ev.cost.len() as u16).to_le_bytes());

    let mut off = EVENT_HEADER_SIZE;
    for r in &ev.refs {
        buf[off] = r.kind;
        buf[off + 1..off + 9].copy_from_slice(&r.target_event_id.to_le_bytes());
        buf[off + 9..off + 17].copy_from_slice(&r.target_offset_cache.to_le_bytes());
        off += REF_SIZE;
    }
    buf[off..off + ev.content.len()].copy_from_slice(ev.content.as_bytes());
    off += ev.content.len();
    buf[off..off + ev.topic.len()].copy_from_slice(ev.topic.as_bytes());
    off += ev.topic.len();
    buf[off..off + ev.cost.len()].copy_from_slice(ev.cost.as_bytes());
    off += ev.cost.len();
    for &f in &ev.vector {
        buf[off..off + 4].copy_from_slice(&f.to_bits().to_le_bytes());
        off += 4;
    }

    if off != record_len {
        return Err(FormatError::RecordLen { record_len, actual: off });
    }
    Ok(buf)
}

/// Decode one event record at the start of `buf`. Returns the event and the
/// number of bytes consumed (the stored record_len). `file_offset` is 0; the
/// store sets it from the IdxRecord used to locate the record.
pub fn decode_event(buf: &[u8], vector_dim: u16) -> Result<(Event, usize)> {
    if buf.len() < EVENT_HEADER_SIZE {
        return Err(FormatError::ShortBuffer { need: EVENT_HEADER_SIZE, got: buf.len() });
    }
    let record_len = rd_u32(&buf[0..4]) as usize;
    if buf.len() < record_len {
        return Err(FormatError::ShortBuffer { need: record_len, got: buf.len() });
    }

    let id = rd_u64(&buf[4..12]);
    let stream_id = rd_u64(&buf[12..20]);
    let created_at = nanos_to_time(rd_u64(&buf[20..28]) as i64);
    let closed_at = nanos_to_opt_time(rd_u64(&buf[28..36]) as i64);
    let event_type = buf[36];
    let outcome = buf[37];
    let confidence = buf[38];
    let flags = buf[39];
    let trace = buf[40];
    let refs_count = rd_u16(&buf[41..43]) as usize;
    let content_len = rd_u16(&buf[43..45]) as usize;
    let topic_len = rd_u16(&buf[45..47]) as usize;
    let cost_len = rd_u16(&buf[47..49]) as usize;

    let expected = EVENT_HEADER_SIZE
        + refs_count * REF_SIZE
        + content_len
        + topic_len
        + cost_len
        + vector_dim as usize * 4;
    if expected != record_len {
        return Err(FormatError::RecordLen { record_len, actual: expected });
    }

    let mut off = EVENT_HEADER_SIZE;
    let mut refs = Vec::with_capacity(refs_count);
    for _ in 0..refs_count {
        refs.push(Ref {
            kind: buf[off],
            target_event_id: rd_u64(&buf[off + 1..off + 9]),
            target_offset_cache: rd_u64(&buf[off + 9..off + 17]),
        });
        off += REF_SIZE;
    }

    let content = decode_utf8(&buf[off..off + content_len]);
    off += content_len;
    let topic = decode_utf8(&buf[off..off + topic_len]);
    off += topic_len;
    let cost = decode_utf8(&buf[off..off + cost_len]);
    off += cost_len;

    let mut vector = Vec::with_capacity(vector_dim as usize);
    for _ in 0..vector_dim as usize {
        vector.push(f32::from_bits(rd_u32(&buf[off..off + 4])));
        off += 4;
    }

    Ok((
        Event {
            id,
            stream_id,
            created_at,
            closed_at,
            event_type,
            outcome,
            confidence,
            flags,
            trace,
            refs,
            content,
            topic,
            cost,
            vector,
            file_offset: 0,
        },
        record_len,
    ))
}

// Go wrote Go strings (already valid UTF-8) verbatim; lossy decode is a
// belt-and-suspenders against a corrupt byte run and never triggers on a
// well-formed store.
fn decode_utf8(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(nanos: i64) -> DateTime<Utc> {
        nanos_to_time(nanos)
    }

    #[test]
    fn meta_round_trip() {
        let h = MetaHeader {
            magic: FILE_MAGIC,
            version: FORMAT_VERSION,
            vector_dim: 1024,
            last_event_id: 81,
            event_count: 81,
            stream_count: 1,
            created_at: ts(1_700_000_000_000_000_000),
            last_snapshot_at: Some(ts(1_700_000_900_000_000_000)),
        };
        let buf = encode_meta_header(&h);
        assert_eq!(buf.len(), META_HEADER_SIZE);
        assert_eq!(decode_meta_header(&buf).unwrap(), h);
    }

    #[test]
    fn meta_rejects_bad_magic_and_future_version() {
        let mut buf = encode_meta_header(&MetaHeader {
            magic: FILE_MAGIC,
            version: FORMAT_VERSION,
            vector_dim: 8,
            last_event_id: 0,
            event_count: 0,
            stream_count: 0,
            created_at: ts(1),
            last_snapshot_at: None,
        });
        buf[0] ^= 0xFF;
        assert!(matches!(decode_meta_header(&buf), Err(FormatError::BadMagic { .. })));

        let mut future = encode_meta_header(&MetaHeader {
            magic: FILE_MAGIC,
            version: FORMAT_VERSION + 1,
            vector_dim: 8,
            last_event_id: 0,
            event_count: 0,
            stream_count: 0,
            created_at: ts(1),
            last_snapshot_at: None,
        });
        // magic stays valid; only the version is out of range.
        future[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
        assert!(matches!(decode_meta_header(&future), Err(FormatError::FutureVersion { .. })));
    }

    #[test]
    fn idx_round_trip() {
        let r = IdxRecord {
            event_id: 42,
            file_offset: 4096,
            record_len: 1234,
            created_at: ts(1_700_000_000_123_000_000),
        };
        assert_eq!(decode_idx_record(&encode_idx_record(&r)).unwrap(), r);
    }

    #[test]
    fn event_round_trip_with_refs_and_vector() {
        let dim = 8u16;
        let ev = Event {
            id: 81,
            stream_id: 1,
            created_at: ts(1_700_000_000_000_000_000),
            closed_at: None,
            event_type: 2,
            outcome: 1,
            confidence: 1,
            flags: 0,
            trace: EventTrace::Action.as_u8(),
            refs: vec![
                Ref { kind: RefKind::Supersedes.as_u8(), target_event_id: 7, target_offset_cache: 0 },
                Ref { kind: RefKind::Informs.as_u8(), target_event_id: 9, target_offset_cache: 4096 },
            ],
            content: "bench-nats payload double-encoding fix".to_string(),
            topic: "bench-nats-rust-payload".to_string(),
            cost: String::new(),
            vector: vec![0.1, 0.2, 0.0, -0.3, 0.4, 0.5, 0.0, 0.6],
            file_offset: 0,
        };
        let buf = encode_event(&ev, dim).unwrap();
        assert_eq!(buf.len(), event_record_size(&ev, dim));
        assert_eq!(rd_u32(&buf[0..4]) as usize, buf.len());
        let (got, n) = decode_event(&buf, dim).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(got, ev);
    }

    #[test]
    fn event_rejects_wrong_vector_dim() {
        let ev = Event {
            id: 1, stream_id: 1, created_at: ts(1), closed_at: None,
            event_type: 0, outcome: 0, confidence: 0, flags: 0, trace: 0,
            refs: vec![], content: String::new(), topic: String::new(),
            cost: String::new(), vector: vec![0.0; 4], file_offset: 0,
        };
        assert!(matches!(encode_event(&ev, 8), Err(FormatError::VectorDimension { .. })));
    }

    #[test]
    fn trace_parse_round_trip() {
        for t in [EventTrace::Space, EventTrace::Time, EventTrace::Action, EventTrace::Unknown] {
            assert_eq!(EventTrace::from_u8(t.as_u8()), t);
        }
        assert_eq!(EventTrace::parse("action"), (EventTrace::Action, true));
        assert_eq!(EventTrace::parse(""), (EventTrace::Unknown, false));
        assert_eq!(EventTrace::parse("bogus"), (EventTrace::Unknown, false));
    }
}

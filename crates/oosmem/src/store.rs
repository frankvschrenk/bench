//! The on-disk store and the in-RAM event graph on top of it. Port of the Go
//! `store` package (files.go + memory.go), with the ANN index replaced by an
//! exact in-RAM cosine scan -- at this corpus size (tens to low thousands of
//! events) brute force is faster than maintaining an HNSW and is always exact.
//
// Three files form the persistent state: meta.bin, events.bin, events.idx. The
// old hnsw.bin is intentionally not read or written; it was a rebuildable
// cache. events.bin/events.idx are append-only; meta.bin is rewritten
// atomically (temp + rename) at snapshot time. fsync sits between the
// events.bin and events.idx appends so a crash leaves at worst a coverable
// tail, which `load` repairs by re-decoding it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::format::{
    self, decode_event, decode_idx_record, encode_event, encode_idx_record, encode_meta_header,
    event_record_size, Event, IdxRecord, MetaHeader, FILE_MAGIC, FORMAT_VERSION,
    IDX_RECORD_SIZE, META_HEADER_SIZE,
};
use crate::vector;

const META_FILE: &str = "meta.bin";
const EVENTS_FILE: &str = "events.bin";
const IDX_FILE: &str = "events.idx";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("oosmem/store: io: {0}")]
    Io(#[from] std::io::Error),
    #[error("oosmem/store: format: {0}")]
    Format(#[from] format::FormatError),
    #[error("oosmem/store: events.idx size {size} is not a multiple of {rec}")]
    IdxMisaligned { size: u64, rec: usize },
    #[error("oosmem/store: events.idx references {covered} past events.bin end {events}")]
    IdxAhead { covered: u64, events: u64 },
    #[error("oosmem/store: meta.bin shorter than {0} bytes")]
    MetaShort(usize),
}

type Result<T> = std::result::Result<T, StoreError>;

/// A long-running work thread, derived from event records (not persisted as its
/// own record). Name is not learned from events here (events carry no stream
/// name); the server sets it when a write supplies one.
#[derive(Debug, Clone)]
pub struct Stream {
    pub id: u64,
    pub name: String,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub event_ids: Vec<u64>,
}

/// "Event `source_event_id` points at me via `kind`." Built at load from the
/// forward refs of every event; not persisted.
#[derive(Debug, Clone, Copy)]
pub struct InverseRef {
    pub source_event_id: u64,
    pub kind: u8,
}

/// What `load` found and repaired, for the startup log.
#[derive(Debug, Default, Clone)]
pub struct LoadReport {
    pub events_loaded: usize,
    pub streams_built: usize,
    pub inverse_refs: usize,
    pub repaired_idx: usize,
}

/// The store: open file handles, the parsed header, and the derived in-RAM
/// graph. Single-writer by contract; the server wraps it in a Mutex.
pub struct Store {
    dir: PathBuf,
    events: File,
    idx: File,
    header: MetaHeader,
    events_bytes: u64,
    idx_bytes: u64,

    by_id: HashMap<u64, Event>,
    by_stream: HashMap<u64, Vec<u64>>,
    inverse_refs: HashMap<u64, Vec<InverseRef>>,
    streams: HashMap<u64, Stream>,
    next_event_id: u64,
}

impl Store {
    /// Attach to an existing store at `dir`, or create a fresh one with
    /// `vector_dim` if none exists. An existing store's stored dim wins. Runs
    /// crash-tail repair and loads the full graph into RAM.
    pub fn open_or_create(dir: impl AsRef<Path>, vector_dim: u16) -> Result<(Store, LoadReport)> {
        let dir = dir.as_ref().to_path_buf();
        let meta_path = dir.join(META_FILE);
        let header = if meta_path.exists() {
            read_meta(&meta_path)?
        } else {
            std::fs::create_dir_all(&dir)?;
            let h = MetaHeader {
                magic: FILE_MAGIC,
                version: FORMAT_VERSION,
                vector_dim,
                last_event_id: 0,
                event_count: 0,
                stream_count: 0,
                created_at: Utc::now(),
                last_snapshot_at: None,
            };
            write_meta_atomic(&dir, &h)?;
            h
        };

        let events = OpenOptions::new().read(true).write(true).create(true).open(dir.join(EVENTS_FILE))?;
        let idx = OpenOptions::new().read(true).write(true).create(true).open(dir.join(IDX_FILE))?;
        let events_bytes = events.metadata()?.len();
        let idx_bytes = idx.metadata()?.len();
        if idx_bytes % IDX_RECORD_SIZE as u64 != 0 {
            return Err(StoreError::IdxMisaligned { size: idx_bytes, rec: IDX_RECORD_SIZE });
        }

        let mut store = Store {
            dir,
            events,
            idx,
            header,
            events_bytes,
            idx_bytes,
            by_id: HashMap::new(),
            by_stream: HashMap::new(),
            inverse_refs: HashMap::new(),
            streams: HashMap::new(),
            next_event_id: 1,
        };
        let report = store.load()?;
        Ok((store, report))
    }

    /// The configured vector dimension (the stored value wins for an existing
    /// store). Callers build query/event vectors at this dimension.
    pub fn vector_dim(&self) -> u16 {
        self.header.vector_dim
    }

    // ── Load + repair ───────────────────────────────────────

    fn load(&mut self) -> Result<LoadReport> {
        let mut report = LoadReport::default();
        report.repaired_idx = self.repair_idx()?;

        let idx_recs = self.read_all_idx_records()?;
        let dim = self.vector_dim();
        let mut max_id = 0u64;
        for rec in &idx_recs {
            let buf = self.read_events_range(rec.file_offset, rec.file_offset + rec.record_len as u64)?;
            let (mut ev, _) = decode_event(&buf, dim)?;
            ev.file_offset = rec.file_offset;
            if rec.event_id > max_id {
                max_id = rec.event_id;
            }
            self.absorb(ev);
        }
        // nextEventID comes from the idx (authoritative), not meta.LastEventID,
        // which is only current as of the last snapshot.
        self.next_event_id = max_id + 1;

        report.events_loaded = self.by_id.len();
        report.streams_built = self.streams.len();
        report.inverse_refs = self.inverse_refs.values().map(Vec::len).sum();
        Ok(report)
    }

    // Insert one event into the four maps. by_stream / stream.event_ids stay in
    // append (== id) order under the single-writer invariant.
    fn absorb(&mut self, ev: Event) {
        let id = ev.id;
        let stream_id = ev.stream_id;
        let created_at = ev.created_at;

        self.by_stream.entry(stream_id).or_default().push(id);
        let stream = self.streams.entry(stream_id).or_insert_with(|| Stream {
            id: stream_id,
            name: String::new(),
            opened_at: created_at,
            closed_at: None,
            event_ids: Vec::new(),
        });
        if created_at < stream.opened_at {
            stream.opened_at = created_at;
        }
        stream.event_ids.push(id);

        for r in &ev.refs {
            self.inverse_refs.entry(r.target_event_id).or_default().push(InverseRef {
                source_event_id: id,
                kind: r.kind,
            });
        }
        self.by_id.insert(id, ev);
    }

    // Crash-window repair: if events.bin holds bytes past the last idx record,
    // re-decode that tail and backfill idx entries. A partial trailing record
    // is truncated off events.bin so the next append writes at a clean
    // boundary. On a cleanly-closed store this is a no-op.
    fn repair_idx(&mut self) -> Result<usize> {
        let idx_count = self.idx_bytes / IDX_RECORD_SIZE as u64;
        let mut covered_end = 0u64;
        if idx_count > 0 {
            let last = self.read_idx_record(idx_count - 1)?;
            covered_end = last.file_offset + last.record_len as u64;
        }
        if covered_end > self.events_bytes {
            return Err(StoreError::IdxAhead { covered: covered_end, events: self.events_bytes });
        }
        if covered_end == self.events_bytes {
            return Ok(0);
        }

        let tail = self.read_events_range(covered_end, self.events_bytes)?;
        let dim = self.vector_dim();
        let mut off = 0usize;
        let mut added = 0usize;
        while off < tail.len() {
            match decode_event(&tail[off..], dim) {
                Ok((ev, n)) => {
                    let abs_offset = covered_end + off as u64;
                    self.append_idx_record(&IdxRecord {
                        event_id: ev.id,
                        file_offset: abs_offset,
                        record_len: n as u32,
                        created_at: ev.created_at,
                    })?;
                    added += 1;
                    off += n;
                }
                Err(_) => {
                    // Partial / corrupt tail record: drop it so the next append
                    // overwrites at the last clean boundary.
                    let clean = covered_end + off as u64;
                    self.events.set_len(clean)?;
                    self.events.sync_all()?;
                    self.events_bytes = clean;
                    return Ok(added);
                }
            }
        }
        Ok(added)
    }

    // ── Read API ──────────────────────────────────────────

    pub fn by_id(&self, id: u64) -> Option<&Event> {
        self.by_id.get(&id)
    }

    pub fn event_count(&self) -> usize {
        self.by_id.len()
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Event IDs of a stream, sorted ascending (== chronological, IDs monotone).
    pub fn stream_event_ids(&self, stream_id: u64) -> Vec<u64> {
        self.by_stream.get(&stream_id).cloned().unwrap_or_default()
    }

    pub fn stream(&self, stream_id: u64) -> Option<&Stream> {
        self.streams.get(&stream_id)
    }

    pub fn inverse_refs(&self, target_id: u64) -> Vec<InverseRef> {
        self.inverse_refs.get(&target_id).cloned().unwrap_or_default()
    }

    /// Exact k-nearest cosine search over stored vectors. Skips all-zero
    /// vectors (events with no tokenisable text) and, if `stream_id` is given,
    /// restricts to that stream. Returns (event_id, score) sorted by score
    /// descending, length <= k.
    pub fn search(&self, query: &[f32], k: usize, stream_id: Option<u64>) -> Vec<(u64, f32)> {
        let mut scored: Vec<(u64, f32)> = self
            .by_id
            .values()
            .filter(|ev| stream_id.map_or(true, |s| ev.stream_id == s))
            .filter(|ev| ev.vector.iter().any(|&x| x != 0.0))
            .map(|ev| (ev.id, vector::cosine(query, &ev.vector)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored
    }

    // ── Write API ────────────────────────────────────────

    /// Assign the next monotone ID, stamp created_at if unset, fill ref offset
    /// hints, write the record + idx entry (fsync between), and update the
    /// in-RAM graph. Returns the assigned event ID.
    pub fn append_event(&mut self, mut ev: Event) -> Result<u64> {
        let id = self.next_event_id;
        ev.id = id;
        if ev.created_at.timestamp_nanos_opt().unwrap_or(0) == 0 {
            ev.created_at = Utc::now();
        }
        for r in ev.refs.iter_mut() {
            if let Some(target) = self.by_id.get(&r.target_event_id) {
                r.target_offset_cache = target.file_offset;
            }
        }

        let dim = self.vector_dim();
        let buf = encode_event(&ev, dim)?;
        let offset = self.events_bytes;
        self.events.write_all_at(&buf, offset)?;
        self.events.sync_all()?;
        self.events_bytes += buf.len() as u64;

        let idx_buf = encode_idx_record(&IdxRecord {
            event_id: id,
            file_offset: offset,
            record_len: buf.len() as u32,
            created_at: ev.created_at,
        });
        self.idx.write_all_at(&idx_buf, self.idx_bytes)?;
        self.idx.sync_all()?;
        self.idx_bytes += IDX_RECORD_SIZE as u64;

        if id > self.header.last_event_id {
            self.header.last_event_id = id;
        }
        self.header.event_count += 1;

        ev.file_offset = offset;
        self.absorb(ev);
        self.next_event_id = id + 1;
        Ok(id)
    }

    /// Rewrite meta.bin with up-to-date counts + snapshot time. The only
    /// durability point for the header; events/idx are already fsync'd per
    /// append. (No hnsw.bin: the index is in-RAM and rebuilt on load.)
    pub fn snapshot(&mut self) -> Result<()> {
        self.header.event_count = self.by_id.len() as u64;
        self.header.stream_count = self.streams.len() as u64;
        self.header.last_snapshot_at = Some(Utc::now());
        write_meta_atomic(&self.dir, &self.header)?;
        Ok(())
    }

    // ── Low-level file helpers ─────────────────────────────────

    fn read_events_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        if end < start {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; (end - start) as usize];
        self.events.read_exact_at(&mut buf, start)?;
        Ok(buf)
    }

    fn read_idx_record(&self, i: u64) -> Result<IdxRecord> {
        let mut buf = [0u8; IDX_RECORD_SIZE];
        self.idx.read_exact_at(&mut buf, i * IDX_RECORD_SIZE as u64)?;
        Ok(decode_idx_record(&buf)?)
    }

    fn read_all_idx_records(&self) -> Result<Vec<IdxRecord>> {
        if self.idx_bytes == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; self.idx_bytes as usize];
        self.idx.read_exact_at(&mut buf, 0)?;
        let count = self.idx_bytes as usize / IDX_RECORD_SIZE;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            out.push(decode_idx_record(&buf[i * IDX_RECORD_SIZE..])?);
        }
        Ok(out)
    }

    fn append_idx_record(&mut self, rec: &IdxRecord) -> Result<()> {
        let buf = encode_idx_record(rec);
        self.idx.write_all_at(&buf, self.idx_bytes)?;
        self.idx.sync_all()?;
        self.idx_bytes += IDX_RECORD_SIZE as u64;
        if rec.event_id > self.header.last_event_id {
            self.header.last_event_id = rec.event_id;
        }
        self.header.event_count += 1;
        Ok(())
    }
}

// Unused-by-store helper kept for the encode path symmetry / future tooling.
#[allow(dead_code)]
fn record_size(ev: &Event, dim: u16) -> usize {
    event_record_size(ev, dim)
}

fn read_meta(path: &Path) -> Result<MetaHeader> {
    let buf = std::fs::read(path)?;
    if buf.len() < META_HEADER_SIZE {
        return Err(StoreError::MetaShort(META_HEADER_SIZE));
    }
    Ok(format::decode_meta_header(&buf)?)
}

// Atomic meta.bin write: temp file in the same dir, fsync, rename.
fn write_meta_atomic(dir: &Path, h: &MetaHeader) -> Result<()> {
    let final_path = dir.join(META_FILE);
    let tmp_path = dir.join(format!("{META_FILE}.tmp-{}", std::process::id()));
    {
        let mut tmp = File::create(&tmp_path)?;
        tmp.write_all(&encode_meta_header(h))?;
        tmp.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    // The decisive cross-implementation check: open the REAL live store and,
    // for every event, rebuild its vector from content+topic and confirm it
    // matches the vector Go wrote (cosine ~ 1). This validates the format codec
    // (content/topic/vector decoded correctly) AND the vector pipeline (xxhash
    // + tokenizer + weights + normalise all bit-compatible with Go) against
    // production data. Read-only. Ignored by default; run with:
    //   OOSMEM_VERIFY_DIR="$HOME/Library/Application Support/oosmem" \
    //     cargo test -p oosmem -- --ignored --nocapture
    #[test]
    #[ignore = "set OOSMEM_VERIFY_DIR to a real store directory"]
    fn verify_against_real_store() {
        let Ok(dir) = std::env::var("OOSMEM_VERIFY_DIR") else {
            eprintln!("skip: OOSMEM_VERIFY_DIR unset");
            return;
        };
        let (store, report) = Store::open_or_create(&dir, 1024).expect("open real store");
        eprintln!(
            "loaded {} events, {} streams, {} inverse-refs, repaired {} idx",
            report.events_loaded, report.streams_built, report.inverse_refs, report.repaired_idx
        );
        let dim = store.vector_dim() as usize;
        eprintln!("vector_dim = {dim}");

        let mut ids: Vec<u64> = store.by_id.keys().copied().collect();
        ids.sort_unstable();

        let mut checked = 0usize;
        let mut zero = 0usize;
        let mut worst = (0u64, 1.0f32); // (id, lowest cosine seen)
        for id in ids {
            let ev = store.by_id.get(&id).unwrap();
            let stored_zero = ev.vector.iter().all(|&x| x == 0.0);
            let rebuilt = vector::build(&ev.content, &ev.topic, dim);
            let rebuilt_zero = rebuilt.iter().all(|&x| x == 0.0);
            if stored_zero {
                zero += 1;
                assert!(rebuilt_zero, "event {id}: stored vector zero but rebuilt is non-zero");
                continue;
            }
            let c = vector::cosine(&ev.vector, &rebuilt);
            if c < worst.1 {
                worst = (id, c);
            }
            assert!(
                c > 0.9999,
                "event {id}: cosine(stored, rebuilt) = {c} -- pipeline diverges from Go (topic={:?})",
                ev.topic
            );
            checked += 1;
        }
        eprintln!(
            "OK: {checked} non-zero vectors match rebuilt (cosine > 0.9999), {zero} zero-vector events; worst cosine {} at event {}",
            worst.1, worst.0
        );
        assert!(checked > 0, "no non-zero vectors verified -- store empty or all zero?");
    }
}

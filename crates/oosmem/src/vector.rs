//! Token-hash vector pipeline: text -> normalised tokens -> additive
//! float32 slot array -> L2-normalised unit vector. Ported byte-for-byte from
//! the Go `vector` package so vectors built here land in the same cosine
//! space as the ones already stored in events.bin.
//
// The single hard compatibility requirement is the hash: Go used
// cespare/xxhash/v2 (XxHash64, seed 0, over the token's UTF-8 bytes). twox-hash
// XxHash64::oneshot(0, ..) is the same algorithm, so a token maps to the same
// slot in both. The tokenizer and the f64-accumulate / f32-normalise split are
// reproduced exactly for the same reason.

use twox_hash::XxHash64;

/// Dimension used when a caller passes none. Matches the Go DefaultDim; the
/// live store actually runs at 128 (read from meta.bin), so this is only a
/// fallback for fresh stores.
pub const DEFAULT_DIM: usize = 1024;

/// Smallest token length kept (bytes, matching Go's len()).
pub const MIN_TOKEN_LEN: usize = 2;

const CONTENT_WEIGHT: f32 = 1.0;
const TOPIC_WEIGHT: f32 = 2.0;

// Deliberately small DE+EN stop list, identical to the Go set: ~60
// high-frequency closed-class words that carry no recall signal.
const STOPWORDS: &[&str] = &[
    // English
    "the", "a", "an", "and", "or", "but", "if", "of", "to", "in", "on", "at", "is", "are", "was",
    "were", "be", "been", "for", "with", "as", "by", "that", "this", "it", "its", "not", "no",
    "so", "do", "does", "did",
    // German
    "der", "die", "das", "den", "dem", "des", "ein", "eine", "einen", "einem", "einer", "eines",
    "und", "oder", "aber", "wenn", "dann", "ist", "sind", "war", "waren", "sein", "für", "mit",
    "als", "von", "vom", "zum", "zur", "im", "am", "auf", "aus", "es", "sie", "er", "wir", "ich",
    "nicht", "kein", "keine", "auch", "noch",
];

fn is_stopword(t: &str) -> bool {
    STOPWORDS.contains(&t)
}

fn allowed_symbol(b: u8) -> bool {
    matches!(b, b'_' | b'-' | b'.' | b'/')
}

// Lowercase (Unicode-aware), then fold every disallowed ASCII byte to a space
// while passing through alphanumerics, the four code symbols, and every byte
// >= 0x80 verbatim. Iterating bytes (not chars) mirrors the Go normalise: a
// multi-byte UTF-8 letter survives intact because each of its bytes is >= 0x80,
// and only ASCII bytes are ever replaced, so the result stays valid UTF-8.
fn normalise(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = Vec::with_capacity(lower.len());
    for &c in lower.as_bytes() {
        match c {
            b'a'..=b'z' | b'0'..=b'9' => out.push(c),
            _ if c >= 0x80 => out.push(c),
            _ if allowed_symbol(c) => out.push(c),
            _ => out.push(b' '),
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Cleaned, stop-listed token list for `s`. Never contains empty strings;
/// tokens are lowercased; stopwords and tokens shorter than MIN_TOKEN_LEN
/// bytes are dropped.
pub fn tokenize(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    normalise(s)
        .split_whitespace()
        .filter(|t| t.len() >= MIN_TOKEN_LEN && !is_stopword(t))
        .map(str::to_string)
        .collect()
}

// Topic tokens plus their structural sub-components: "cold-start-pipeline"
// expands to itself plus "cold", "start", "pipeline". Splits on `-./_` only,
// sub-components shorter than MIN_TOKEN_LEN or in the stop list are dropped.
// Content tokens are NOT expanded, so identifiers like `bench.fs.read` stay
// whole where they appear in code.
fn expand_topic(tokens: Vec<String>) -> Vec<String> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(tokens.len() * 2);
    for t in tokens {
        let has_sep = t.contains(['-', '.', '/', '_']);
        out.push(t.clone());
        if !has_sep {
            continue;
        }
        for sub in t.split(['-', '.', '/', '_']) {
            if sub.len() < MIN_TOKEN_LEN || is_stopword(sub) || sub == t {
                continue;
            }
            out.push(sub.to_string());
        }
    }
    out
}

// vec[xxhash64(token) mod dim] += weight, for every token.
fn accumulate(vec: &mut [f32], tokens: &[String], weight: f32) {
    let dim = vec.len() as u64;
    for tok in tokens {
        let slot = (XxHash64::oneshot(0, tok.as_bytes()) % dim) as usize;
        vec[slot] += weight;
    }
}

// Divide vec in place by its L2 norm. An all-zero vector is left unchanged.
// sumSq accumulates in f64 and the inverse norm is cast to f32 before the
// multiply, exactly as the Go normalise2, so re-built vectors match bit-for-bit.
fn normalise2(vec: &mut [f32]) {
    let mut sum_sq = 0f64;
    for &v in vec.iter() {
        sum_sq += v as f64 * v as f64;
    }
    if sum_sq == 0.0 {
        return;
    }
    let inv_norm = (1.0f64 / sum_sq.sqrt()) as f32;
    for v in vec.iter_mut() {
        *v *= inv_norm;
    }
}

/// Build a vector from content + topic. Topic tokens get double weight and are
/// expanded along `-./_`. Result is L2-normalised; empty input yields an
/// all-zero vector.
pub fn build(content: &str, topic: &str, dim: usize) -> Vec<f32> {
    assert!(dim > 0, "oosmem/vector: dim must be positive");
    let mut vec = vec![0.0f32; dim];
    accumulate(&mut vec, &tokenize(content), CONTENT_WEIGHT);
    accumulate(&mut vec, &expand_topic(tokenize(topic)), TOPIC_WEIGHT);
    normalise2(&mut vec);
    vec
}

/// Query-time counterpart: one string, no topic, every token at content weight.
pub fn build_query(query: &str, dim: usize) -> Vec<f32> {
    assert!(dim > 0, "oosmem/vector: dim must be positive");
    let mut vec = vec![0.0f32; dim];
    accumulate(&mut vec, &tokenize(query), CONTENT_WEIGHT);
    normalise2(&mut vec);
    vec
}

/// Cosine similarity of two equal-length vectors. For L2-normalised inputs this
/// is their dot product. Returns 0 for an all-zero vector, NaN on length
/// mismatch (a caller bug). Accumulates in f64 like the Go Cosine.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::NAN;
    }
    let mut dot = 0f64;
    for i in 0..a.len() {
        dot += a[i] as f64 * b[i] as f64;
    }
    dot as f32
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical XxHash64 test vector: hash of the empty input with seed 0.
    // If this fails, twox-hash is not bit-compatible with Go's xxhash and the
    // whole recall space would silently diverge.
    #[test]
    fn xxhash64_matches_canonical_seed0() {
        assert_eq!(XxHash64::oneshot(0, b""), 0xEF46_DB37_51D8_E999);
    }

    #[test]
    fn tokenize_keeps_code_identifiers_and_drops_noise() {
        // dots/slashes/underscores/hyphens hold identifiers together.
        assert_eq!(
            tokenize("Call bench.fs.read on apps/oos/src then DO it"),
            vec!["call", "bench.fs.read", "apps/oos/src", "then"]
        );
        // stopwords (DE+EN) and sub-MIN_TOKEN_LEN tokens drop out.
        assert_eq!(tokenize("der die das a I of und für Build"), vec!["build"]);
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn expand_topic_explodes_slug_but_keeps_whole() {
        let got = expand_topic(tokenize("cold-start-pipeline"));
        for want in ["cold-start-pipeline", "cold", "start", "pipeline"] {
            assert!(got.iter().any(|t| t == want), "missing {want} in {got:?}");
        }
    }

    #[test]
    fn build_is_unit_norm_and_deterministic() {
        let a = build("bench-nats payload double encoding", "bench-nats-rust-payload", 128);
        let b = build("bench-nats payload double encoding", "bench-nats-rust-payload", 128);
        assert_eq!(a, b, "build must be deterministic");
        // self-cosine of a non-empty vector is 1 (within f32 epsilon).
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5, "self-cosine {}", cosine(&a, &a));
    }

    #[test]
    fn empty_input_is_zero_vector() {
        let v = build("", "", 64);
        assert_eq!(v.len(), 64);
        assert!(v.iter().all(|&x| x == 0.0));
        assert_eq!(cosine(&v, &v), 0.0);
    }

    #[test]
    fn topic_weight_dominates_content() {
        // A token appearing only in the topic should outweigh a content-only
        // token in the same fresh slot (2.0 vs 1.0 before normalisation).
        let v = build("alpha", "bravo", 256);
        let alpha = (XxHash64::oneshot(0, b"alpha") % 256) as usize;
        let bravo = (XxHash64::oneshot(0, b"bravo") % 256) as usize;
        if alpha != bravo {
            assert!(v[bravo] > v[alpha], "topic weight should dominate");
        }
    }
}

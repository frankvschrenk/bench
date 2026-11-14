//! oosmem — the NATS-served episodic memory store.
//
// Rust port of the Go service (onisin_old/apps/oosmem). The on-disk binary
// format is reproduced byte-for-byte so this build reads the existing store in
// place; the HNSW cache file is not ported (it was a rebuildable cache), and
// vector search is an exact in-RAM cosine scan rather than ANN -- the corpus
// is small enough that exact is both simpler and faster.
//
// Usage:
//   oosmem -data <dir> [-nats nats://127.0.0.1:4222] [-dim 128]
//          [-snapshot 30m] [-llm-url http://127.0.0.1:11434]
//          [-llm-model gemma4:e4b-mlx] [-llm-timeout 60s]
//
// Only -data is required. Vector-dim resolution: an existing store's stored
// dim always wins; for a fresh store an explicit -dim wins, else oosmem probes
// oos.cmd.embed.meta once, else falls back to 128.

// dead_code is allowed on the foundational modules: they carry the full
// on-disk + vector vocabulary (enum mappings, format constants, the
// inverse-ref graph) faithfully ported from Go. Some of it no handler consumes
// yet, but it documents the format and seeds the graph-aware recall we may add
// later; pruning it now would just have to be re-added.
#[allow(dead_code)]
mod format;
mod payload;
mod server;
#[allow(dead_code)]
mod store;
mod synth;
#[allow(dead_code)]
mod vector;

use std::time::Duration;

use server::ServerConfig;

#[tokio::main]
async fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("oosmem: {e}");
            std::process::exit(2);
        }
    };
    if args.data_dir.is_empty() {
        eprintln!("oosmem: -data is required");
        std::process::exit(2);
    }

    // Resolve the vector dim for a *fresh* store. For an existing store this is
    // ignored (the stored dim wins inside open_or_create), but we still resolve
    // it so a first run alongside oosai adopts oosai's dimension.
    let mut dim = args.dim;
    if !args.dim_explicit {
        match probe_embed_dim(&args.nats_url).await {
            Some(d) if d > 0 => {
                eprintln!("oosmem: vector_dim {d} adopted from oosai");
                dim = d;
            }
            _ => eprintln!("oosmem: embed.meta probe unavailable; using default dim {dim}"),
        }
    }

    let cfg = ServerConfig {
        data_dir: args.data_dir,
        nats_url: args.nats_url,
        dim,
        snapshot_every: args.snapshot_every,
        llm_url: args.llm_url,
        llm_model: args.llm_model,
        llm_timeout: args.llm_timeout,
    };
    if let Err(e) = server::run(cfg).await {
        eprintln!("oosmem exited with error: {e}");
        std::process::exit(1);
    }
    eprintln!("oosmem stopped cleanly");
}

/// Parsed command-line flags. Go-style single-dash flags are accepted (the
/// LaunchAgent passes `-data` / `-llm-url`); `--flag` is accepted too.
struct Args {
    data_dir: String,
    nats_url: String,
    dim: u16,
    dim_explicit: bool,
    snapshot_every: Duration,
    llm_url: String,
    llm_model: String,
    llm_timeout: Duration,
}

impl Args {
    fn parse(tokens: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut a = Args {
            data_dir: String::new(),
            nats_url: String::new(),
            dim: 128,
            dim_explicit: false,
            snapshot_every: Duration::from_secs(30 * 60),
            llm_url: String::new(),
            llm_model: "gemma4:e4b-mlx".to_string(),
            llm_timeout: Duration::from_secs(60),
        };
        let mut it = tokens.peekable();
        while let Some(tok) = it.next() {
            let flag = tok.trim_start_matches('-');
            let mut value = || it.next().ok_or_else(|| format!("flag -{flag} needs a value"));
            match flag {
                "data" => a.data_dir = value()?,
                "nats" => a.nats_url = value()?,
                "dim" => {
                    a.dim = value()?.parse().map_err(|_| "dim must be a u16".to_string())?;
                    a.dim_explicit = true;
                }
                "snapshot" => a.snapshot_every = parse_duration(&value()?)?,
                "llm-url" => a.llm_url = value()?,
                "llm-model" => a.llm_model = value()?,
                "llm-timeout" => a.llm_timeout = parse_duration(&value()?)?,
                "log" => {
                    let _ = value()?; // accepted for CLI compatibility; logging goes to stderr
                }
                other => return Err(format!("unknown flag -{other}")),
            }
        }
        Ok(a)
    }
}

// Parse a Go-style duration with a single unit suffix: ms, s, m, or h. A bare
// number is read as seconds. Good enough for the few cadence flags oosmem has.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult_ms): (&str, u64) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60 * 1000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 60 * 60 * 1000)
    } else {
        (s, 1000)
    };
    let value: u64 = num.trim().parse().map_err(|_| format!("invalid duration {s:?}"))?;
    Ok(Duration::from_millis(value * mult_ms))
}

// Best-effort probe of oos.cmd.embed.meta for the embedding vector dimension.
// Any failure (no responder, transport error, bad reply) returns None so the
// caller falls back to the default. Scoped connection; the server opens its
// own long-lived one later.
async fn probe_embed_dim(nats_url: &str) -> Option<u16> {
    let url = if nats_url.is_empty() { "nats://127.0.0.1:4222" } else { nats_url };
    let client = async_nats::connect(url).await.ok()?;
    let fut = client.request("oos.cmd.embed.meta", b"{}".to_vec().into());
    let msg = tokio::time::timeout(Duration::from_secs(2), fut).await.ok()?.ok()?;
    #[derive(serde::Deserialize)]
    struct EmbedMeta {
        #[serde(default)]
        vector_dim: u16,
    }
    serde_json::from_slice::<EmbedMeta>(&msg.payload).ok().map(|m| m.vector_dim)
}

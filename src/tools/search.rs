//! Content & filename search (bench.search.search).
//
// Faithful port of the Bun tools/search.ts. Deliberately NOT .gitignore-aware
// (the original wasn't): instead it prunes the well-known heavy dirs and, by
// default, hidden entries while walking. With no `glob`, every non-pruned file
// is a candidate; with a `glob`, the path relative to the search root must
// match (literal_separator so `*` never crosses `/`, like the Bun glob). A glob
// without `/` matches the file name at any depth, like ripgrep -g ("*.rs" used
// to find only files directly in the root, i.e. silently nothing). With
// a `pattern`, each matching line becomes a hit; without one, just the path is
// reported. Synchronous std::fs on a blocking thread, like the fs handlers.

use std::path::PathBuf;

use globset::GlobBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::roots::RootRegistry;

const HEAVY: [&str; 6] = [".git", "node_modules", "dist", ".next", "build", "target"];

fn default_max_files() -> usize {
    500
}
fn default_max_hits() -> usize {
    100
}

#[derive(Deserialize)]
struct SearchArgs {
    path: String,
    glob: Option<String>,
    pattern: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    context: usize,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    include_heavy: bool,
    #[serde(default = "default_max_files")]
    max_files: usize,
    #[serde(default = "default_max_hits")]
    max_hits_per_file: usize,
}

pub async fn handle(op: &str, args: Value, ctx: &Ctx) -> Option<Result<Value, ToolError>> {
    if op != "search" {
        return None;
    }
    let roots = ctx.roots.clone();
    match tokio::task::spawn_blocking(move || search(args, &roots)).await {
        Ok(outcome) => Some(outcome),
        Err(join) => Some(Err(ToolError::Msg(format!("search task failed: {join}")))),
    }
}

fn search(args: Value, roots: &RootRegistry) -> Result<Value, ToolError> {
    let a: SearchArgs = serde_json::from_value(args)?;
    let abs = roots.resolve(&a.path)?;
    if !std::fs::metadata(&abs)?.is_dir() {
        return Err(ToolError::Msg(format!(
            "search: path must be a directory, got: {}",
            abs.to_string_lossy()
        )));
    }

    let regex = match &a.pattern {
        Some(p) => Some(
            RegexBuilder::new(p)
                .case_insensitive(a.case_insensitive)
                .build()
                .map_err(|e| ToolError::Msg(format!("search: invalid pattern: {e}")))?,
        ),
        None => None,
    };

    let name_only = a.glob.as_deref().is_some_and(|g| !g.contains('/'));
    let matcher = match &a.glob {
        Some(g) => Some(
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| ToolError::Msg(format!("search: invalid glob: {e}")))?
                .compile_matcher(),
        ),
        None => None,
    };

    let mut results: Vec<Value> = Vec::new();
    let mut file_count: usize = 0;

    // Manual stack DFS, pruning hidden + heavy entries as we descend (no ignore
    // crate). Entries are sorted per directory for a deterministic walk order.
    let mut stack: Vec<PathBuf> = vec![abs.clone()];
    'walk: while let Some(dir) = stack.pop() {
        if file_count >= a.max_files {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for ent in entries {
            if file_count >= a.max_files {
                break 'walk;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            if !a.hidden && name.starts_with('.') {
                continue;
            }
            if !a.include_heavy && HEAVY.contains(&name.as_str()) {
                continue;
            }
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            let full = ent.path();
            if ft.is_dir() {
                stack.push(full);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            // glob is matched against the path relative to the search root.
            if let Some(m) = &matcher {
                let rel = full.strip_prefix(&abs).unwrap_or(&full);
                let hit = if name_only { m.is_match(&name) } else { m.is_match(rel) };
                if !hit {
                    continue;
                }
            }

            match &regex {
                None => {
                    results.push(json!({ "path": full.to_string_lossy() }));
                    file_count += 1;
                }
                Some(re) => {
                    let Ok(text) = std::fs::read_to_string(&full) else {
                        continue;
                    };
                    let lines: Vec<&str> = text.split('\n').collect();
                    let mut hits: Vec<Value> = Vec::new();
                    for (i, line) in lines.iter().enumerate() {
                        if hits.len() >= a.max_hits_per_file {
                            break;
                        }
                        if !re.is_match(line) {
                            continue;
                        }
                        // context key is omitted entirely when context == 0,
                        // matching the Bun `context: undefined` (dropped by
                        // JSON.stringify), not serialised as null.
                        let mut hit = Map::new();
                        hit.insert("line".into(), json!(i + 1));
                        hit.insert("text".into(), json!(line));
                        if a.context > 0 {
                            let start = i.saturating_sub(a.context);
                            let end = (i + a.context).min(lines.len() - 1);
                            hit.insert("context".into(), json!(&lines[start..=end]));
                        }
                        hits.push(Value::Object(hit));
                    }
                    if !hits.is_empty() {
                        results.push(json!({ "path": full.to_string_lossy(), "hits": hits }));
                        file_count += 1;
                    }
                }
            }
        }
    }

    // glob/pattern are omitted when absent (the Bun result carried undefined).
    let mut out = Map::new();
    out.insert("root".into(), json!(abs.to_string_lossy()));
    if let Some(g) = &a.glob {
        out.insert("glob".into(), json!(g));
    }
    if let Some(p) = &a.pattern {
        out.insert("pattern".into(), json!(p));
    }
    out.insert("files".into(), json!(file_count));
    out.insert("results".into(), json!(results));
    Ok(Value::Object(out))
}

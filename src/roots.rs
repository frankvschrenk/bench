//! Allowed-root registry — the filesystem sandbox.
//
// Every fs/git/search/patch path argument is resolved through here before it
// touches disk. A path that normalises outside all configured roots is
// rejected. Faithful port of the Bun util/roots.ts: roots and targets are
// resolved lexically (no realpath), and containment is checked
// component-wise so /a/onisin never matches /a/onisin_old.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

pub struct RootRegistry {
    /// Absolute, lexically-normalised, existing directories.
    roots: Vec<PathBuf>,
}

impl RootRegistry {
    /// All allowed roots as absolute path strings.
    pub fn all(&self) -> Vec<String> {
        self.roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// Resolve a caller-supplied path, returning the absolute path when it sits
    /// inside at least one root, else an error.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        self.check(path, "path")
    }

    /// Same as [`resolve`], but the error message names the cwd argument.
    pub fn resolve_cwd(&self, cwd: &str) -> Result<PathBuf, ToolError> {
        self.check(cwd, "cwd")
    }

    fn check(&self, p: &str, label: &str) -> Result<PathBuf, ToolError> {
        let clean = p.replace('\0', "");
        let abs = normalize_abs(&clean);
        for root in &self.roots {
            if abs.starts_with(root) {
                return Ok(abs);
            }
        }
        Err(ToolError::Msg(format!("{label} escapes allowed roots: {p}")))
    }
}

/// Build a registry from path strings, keeping only those that resolve to an
/// existing directory (matching the Bun version, which stat-checked each root).
pub fn build_root_registry(paths: &[String]) -> RootRegistry {
    let mut roots = Vec::new();
    for p in paths {
        let abs = normalize_abs(p);
        if let Ok(meta) = std::fs::metadata(&abs) {
            if meta.is_dir() {
                roots.push(abs);
            }
        }
    }
    RootRegistry { roots }
}

// Make a path absolute (relative paths join the process cwd) and collapse
// `.`/`..` lexically, mirroring node's path.resolve. We do NOT canonicalise
// (realpath) because targets may not exist yet (write/mkdir), and the Bun
// version didn't either.
fn normalize_abs(p: &str) -> PathBuf {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    let mut out: Vec<Component> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a real segment, but never climb past the root prefix.
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

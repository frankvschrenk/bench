//! Static manifest of every bench.* NATS subject.
//!
//! `list_operations` returns this verbatim so Claude can discover the
//! whole bench surface (fs, search, exec, git, patch, pg, memory, task)
//! without a network round-trip. Faithful port of operations.ts.

use serde::ser::{Serialize, SerializeMap, SerializeStruct, Serializer};

/// An ordered input schema. We serialize a slice of (name, type) pairs
/// as a JSON object rather than storing a map, because the manifest is
/// read top-to-bottom by a human and JSON.stringify in the TS original
/// preserved declaration order; a BTreeMap-backed serde_json::Map would
/// reorder the keys alphabetically.
struct InputSchema(&'static [(&'static str, &'static str)]);

impl Serialize for InputSchema {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut map = ser.serialize_map(Some(self.0.len()))?;
        for (name, ty) in self.0 {
            map.serialize_entry(name, ty)?;
        }
        map.end()
    }
}

/// One bench subject with its input/output signature.
struct Operation {
    subject: &'static str,
    description: &'static str,
    input: InputSchema,
    output: &'static str,
}

impl Serialize for Operation {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // Field order subject/description/input/output mirrors the TS
        // object literal so the rendered manifest is byte-for-byte alike.
        let mut st = ser.serialize_struct("Operation", 4)?;
        st.serialize_field("subject", self.subject)?;
        st.serialize_field("description", self.description)?;
        st.serialize_field("input", &self.input)?;
        st.serialize_field("output", self.output)?;
        st.end()
    }
}

/// Render the manifest as the pretty-printed JSON array the tool returns
/// (2-space indent, matching JSON.stringify(OPERATIONS, null, 2)).
pub fn manifest_json() -> String {
    // Infallible: the manifest is static and contains only strings.
    serde_json::to_string_pretty(&OPERATIONS).expect("manifest serializes")
}

static OPERATIONS: &[Operation] = &[
    // -- fs --------------------------------------------------------
    Operation { subject: "bench.fs.allowed_roots", description: "List all allowed filesystem roots.", input: InputSchema(&[]), output: "{ roots: string[] }" },
    Operation { subject: "bench.fs.stat", description: "Stat a single path.", input: InputSchema(&[("path", "string")]), output: "{ path, kind, size, mtime, mode }" },
    Operation { subject: "bench.fs.list", description: "List directory entries.", input: InputSchema(&[("path", "string"), ("hidden", "boolean (optional)")]), output: "{ path, count, entries[] }" },
    Operation { subject: "bench.fs.tree", description: "Recursive directory tree.", input: InputSchema(&[("path", "string"), ("depth", "number (optional)"), ("hidden", "boolean (optional)"), ("include_heavy", "boolean (optional)")]), output: "{ path, root }" },
    Operation { subject: "bench.fs.read", description: "Read a file (text). Supports start/end/lines/tail.", input: InputSchema(&[("path", "string"), ("start", "number (optional)"), ("end", "number (optional)"), ("lines", "number (optional)"), ("tail", "number (optional)")]), output: "{ path, kind, content, total_lines, truncated }" },
    Operation { subject: "bench.fs.read_many", description: "Read multiple files in one call.", input: InputSchema(&[("paths", "string[]")]), output: "{ results: [{ path, ok, content }] }" },
    Operation { subject: "bench.fs.write", description: "Write / overwrite a file.", input: InputSchema(&[("path", "string"), ("content", "string")]), output: "{ path, bytes, status }" },
    Operation { subject: "bench.fs.append", description: "Append to a file.", input: InputSchema(&[("path", "string"), ("content", "string")]), output: "{ path, status }" },
    Operation { subject: "bench.fs.edit", description: "Find-and-replace in a file (unique match required by default).", input: InputSchema(&[("path", "string"), ("find", "string"), ("replace", "string"), ("expect_count", "number (optional, default 1, -1=any)"), ("dry_run", "boolean (optional)")]), output: "{ path, replacements, status }" },
    Operation { subject: "bench.fs.mkdir", description: "Create directory (with parents).", input: InputSchema(&[("path", "string")]), output: "{ path, status }" },
    Operation { subject: "bench.fs.move", description: "Move / rename.", input: InputSchema(&[("src", "string"), ("dst", "string")]), output: "{ src, dst, status }" },
    Operation { subject: "bench.fs.copy", description: "Copy a file.", input: InputSchema(&[("src", "string"), ("dst", "string")]), output: "{ src, dst, status }" },
    Operation { subject: "bench.fs.remove", description: "Delete a file or directory.", input: InputSchema(&[("path", "string")]), output: "{ path, status }" },
    Operation { subject: "bench.fs.project_info", description: "Detect project structure (git, package.json, etc.).", input: InputSchema(&[("path", "string")]), output: "{ path, git_root, git_branch, files[] }" },

    // -- search ----------------------------------------------------
    Operation { subject: "bench.search.search", description: "Search files by glob and/or regex content. .gitignore-aware.", input: InputSchema(&[("path", "string"), ("glob", "string (optional)"), ("pattern", "string (optional)"), ("case_insensitive", "boolean (optional)"), ("context", "number (optional, context lines)"), ("hidden", "boolean (optional)"), ("include_heavy", "boolean (optional)"), ("max_files", "number (optional, default 500)"), ("max_hits_per_file", "number (optional, default 100)")]), output: "{ root, files, results[] }" },

    // -- exec ------------------------------------------------------
    Operation { subject: "bench.exec.exec", description: "Run a command (blocking). Returns stdout/stderr/exit_code.", input: InputSchema(&[("command", "string"), ("cwd", "string"), ("args", "string[] (optional)"), ("env", "Record<string,string> (optional)"), ("stdin", "string (optional)"), ("timeout_seconds", "number (optional, default 60)")]), output: "{ exit_code, stdout, stderr, duration_ms }" },
    Operation { subject: "bench.exec.exec_start", description: "Start a long-running command. Returns session_id.", input: InputSchema(&[("command", "string"), ("cwd", "string"), ("args", "string[] (optional)"), ("env", "Record<string,string> (optional)"), ("stdin", "string (optional)")]), output: "{ session_id, status }" },
    Operation { subject: "bench.exec.exec_read", description: "Read buffered output from a streaming session.", input: InputSchema(&[("session_id", "string")]), output: "{ stdout_delta, stderr_delta, running, exit_code }" },
    Operation { subject: "bench.exec.exec_stop", description: "Terminate a streaming session (SIGTERM).", input: InputSchema(&[("session_id", "string")]), output: "{ session_id, status }" },
    Operation { subject: "bench.exec.which", description: "Resolve an executable name against $PATH.", input: InputSchema(&[("name", "string")]), output: "{ name, found, path }" },

    // -- git -------------------------------------------------------
    Operation { subject: "bench.git.status", description: "git status --porcelain.", input: InputSchema(&[("path", "string")]), output: "{ dir, branch, clean, entries[] }" },
    Operation { subject: "bench.git.diff", description: "git diff (or --cached).", input: InputSchema(&[("path", "string"), ("staged", "boolean (optional)")]), output: "{ dir, diff }" },
    Operation { subject: "bench.git.commit", description: "Stage + commit (optionally push).", input: InputSchema(&[("path", "string"), ("message", "string"), ("paths", "string[] (optional, specific files to stage)"), ("push", "boolean (optional)"), ("allow_empty", "boolean (optional)")]), output: "{ sha, summary, pushed }" },
    Operation { subject: "bench.git.push", description: "Push to remote.", input: InputSchema(&[("path", "string"), ("remote", "string (optional)"), ("branch", "string (optional)"), ("force_with_lease", "boolean (optional)"), ("tags", "boolean (optional)")]), output: "{ status, exit_code }" },

    // -- patch -----------------------------------------------------
    Operation { subject: "bench.patch.apply_patch", description: "Apply a unified diff.", input: InputSchema(&[("cwd", "string"), ("patch", "string"), ("check", "boolean (optional)"), ("strip", "number (optional, default 1)")]), output: "{ status, output }" },

    // -- postgres --------------------------------------------------
    Operation { subject: "bench.pg.query", description: "SELECT / row-returning SQL.", input: InputSchema(&[("sql", "string")]), output: "{ rows[], row_count }" },
    Operation { subject: "bench.pg.exec", description: "Non-row-returning SQL (INSERT/UPDATE/DDL).", input: InputSchema(&[("sql", "string")]), output: "{ rows_affected, status }" },
    Operation { subject: "bench.pg.reset", description: "Drop and recreate the demo database.", input: InputSchema(&[]), output: "{ status, database }" },

    // -- memory ----------------------------------------------------
    // Persistence lives in oosmem (oos.cmd.mem.*); the bench.memory.*
    // handlers in bench are thin adapters over its event model
    // (stream_id + trace). Events are immutable: no delete subject.
    Operation { subject: "bench.memory.write", description: "Append an event to a memory stream. Trace classifies the event as space (where), time (when), action (what was done), or unknown.", input: InputSchema(&[("stream_id", "number (positive integer)"), ("content", "string"), ("topic", "string (optional, <=200 chars)"), ("trace", "space|time|action|unknown (optional, default unknown)")]), output: "{ event_id }" },
    Operation { subject: "bench.memory.search", description: "Semantic search across memory events. Optional stream filter restricts hits to one stream.", input: InputSchema(&[("query", "string"), ("k", "number (optional, default 5)"), ("stream", "number (optional, stream filter)")]), output: "{ hits: [{ event, score }] }" },
    Operation { subject: "bench.memory.list", description: "List events in one stream, newest first. Optional before cursor returns the page older than the given event id.", input: InputSchema(&[("stream_id", "number (positive integer)"), ("limit", "number (optional, default 20)"), ("before", "number (optional, event id cursor)")]), output: "{ events[] }" },

    // -- task ------------------------------------------------------
    Operation { subject: "bench.task.start", description: "Start a new task.", input: InputSchema(&[("goal", "string"), ("repo", "string (optional)")]), output: "{ task_id, goal, started_at, status }" },
    Operation { subject: "bench.task.note", description: "Add a note to a task.", input: InputSchema(&[("task_id", "number"), ("kind", "hypothesis|finding|decision|question|observation"), ("text", "string")]), output: "{ id, task_id, kind }" },
    Operation { subject: "bench.task.link", description: "Link a task to an artefact.", input: InputSchema(&[("task_id", "number"), ("kind", "touched|produced|refers"), ("target", "string"), ("detail", "string (optional)")]), output: "{ task_id, kind, target, status }" },
    Operation { subject: "bench.task.finish", description: "Mark a task done or abandoned.", input: InputSchema(&[("task_id", "number"), ("outcome", "string"), ("status", "done|abandoned (optional, default done)")]), output: "{ task_id, status, outcome }" },
    Operation { subject: "bench.task.resume", description: "Resume the most recent active task (optional semantic query).", input: InputSchema(&[("query", "string (optional)")]), output: "{ found, task? }" },
    Operation { subject: "bench.task.show", description: "Show a task with all notes and links.", input: InputSchema(&[("task_id", "number")]), output: "{ task with notes[] and edges[] }" },
    Operation { subject: "bench.task.search", description: "Semantic search across task notes.", input: InputSchema(&[("query", "string"), ("n", "number (optional, default 5)"), ("include_finished", "boolean (optional)")]), output: "{ results[] }" },
];

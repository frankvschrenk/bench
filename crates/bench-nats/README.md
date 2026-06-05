# bench-nats

Stdio MCP server that bridges Claude to a running `bench` desktop app
over NATS. Rust port of the original Bun/TypeScript implementation
(`apps/bench-nats` in the previous repo); behaviour is kept wire-
identical so the binary can be swapped without the MCP client noticing.

Claude (in claude.ai or Claude Code) speaks the Model Context Protocol
over stdio. bench-nats translates every MCP tool call into a NATS
request-reply on a `bench.*` subject, blocks until bench replies, and
hands the result back. No bench app reachable on NATS -> no tools.

## What it exposes

Five MCP tools, kept deliberately tiny so Claude does not have to learn
a new schema every time bench grows a subject:

- **`list_operations`** — returns the full manifest of `bench.*` subjects
  with input/output signatures. Static; no network round-trip. Call once
  at the start of a task to discover what bench can do.
- **`set_target`** — pick which bench instance to talk to by name (e.g.
  `"macos"`, `"linux"`). Persisted in `~/.config/bench-nats/session.json`.
  Empty string -> any available bench answers.
- **`send_message`** — fire a NATS request to the configured target.
  Subject is the bench suffix (e.g. `bench.fs.read`); bench-nats prefixes
  with the target if one is set. Blocks up to 30 s for the reply.
- **`watch_subject`** — subscribe to a subject and forward each message
  to Claude as an MCP log notification. Useful for long-running async
  flows (start a pipeline, watch its `done` subject, Claude is woken when
  the result lands).
- **`unwatch_subject`** — cancel an active subscription.

The manifest in `src/operations.rs` is the only thing Claude needs to
read to discover the full bench surface (fs, search, exec, git, patch,
pg, memory, task).

## Subject routing

- No target set -> subject is sent as-is (`bench.fs.read`). Any bench
  subscribed to `bench.>` will answer.
- Target set to `"macos"` -> subject becomes `macos.bench.fs.read`. Only
  the bench whose `instanceName` is `"macos"` will answer (its dispatcher
  subscribes to both `bench.>` and `macos.bench.>`).

This is how a single Claude session can drive a Mac bench and a Linux
bench at the same time.

## Running

Development (from the workspace root):

```
cargo run -p bench-nats   # reads from stdin, writes to stdout
```

Production build (single optimized binary, what claude.ai/Claude Code
actually invokes):

```
cargo build -p bench-nats --release   # -> target/release/bench-nats
```

The release profile (`opt-level = "z"`, `lto`, `strip`) is inherited
from the workspace root.

### Wiring into Claude

Add to the MCP server config:

```json
{
  "bench-nats": {
    "command": "/Users/frank/repro/onisin/target/release/bench-nats",
    "env": {
      "BENCH_NATS_URL":       "nats://localhost:4222",
      "BENCH_DEFAULT_TARGET": "macos"
    }
  }
}
```

Then, in Claude:

1. `list_operations` to see what is available.
2. Optional: `set_target "linux"` to switch to the other bench instance.
3. `send_message subject="bench.fs.read" payload={ path: "..." }`.

## Environment

- `BENCH_NATS_URL` — NATS server (default `nats://localhost:4222`).
- `BENCH_DEFAULT_TARGET` — initial target if `session.json` does not yet
  exist. After the first `set_target` call this is irrelevant.

## Layout

```
bench-nats/
  src/
    main.rs        # wiring: stdio transport + tokio runtime
    server.rs      # the five MCP tool handlers + ServerHandler
    nats.rs        # NATS client, session persistence, subject routing
    operations.rs  # static manifest of every bench.* subject
  Cargo.toml
  LICENSE          # Apache-2.0 (separate from the repo BSL-1.1)
```

## Implementation notes

- Built on the official MCP SDK [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)
  (`server`, `transport-io`, `macros`). The five tools are `#[tool]`
  methods on one `#[tool_router]` handler.
- **stdout carries the JSON-RPC protocol — nothing else may be printed
  there.** All diagnostics go to stderr.
- The session file lives at `~/.config/bench-nats/session.json` (not the
  macOS Application Support dir) on purpose, so a freshly built binary
  reads the same active target the previous one wrote.

## Related

- `apps/bench` — the desktop app that actually services the `bench.*`
  subjects. bench-nats is useless without one bench reachable on NATS.
- `oosmem` — the memory service. `bench.memory.*` is a thin adapter;
  `oos.cmd.mem.*` is its native surface.

## License

Apache License 2.0 — see `LICENSE` in this directory. bench-nats is
deliberately licensed permissively, separately from the BSL-1.1 of the
rest of the Onisin repository, because it is a generic MCP bridge with
no domain-specific business logic.

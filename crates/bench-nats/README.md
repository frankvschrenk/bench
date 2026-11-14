# bench-nats

stdio-MCP-Server für **Claude Desktop**, der die Werkzeuge einer laufenden
Bench durchreicht. Claude startet ihn als Kindprozess; jeder Werkzeugaufruf
geht als `POST <bench>/call/<operation>` mit Bearer-Token an die gewählte
Bench-Instanz.

Der Name stammt aus der Zeit, als der Weg NATS war (bis 14.11.2026). Werkzeuge
und Argumente sind gleich geblieben (`list_operations`, `set_target`,
`send_message {subject, payload, target?}`); sie liegen im Crate `bench-mcp`,
das die Bench auch selbst unter `/mcp` (Streamable HTTP) anbietet.
`watch_subject`/`unwatch_subject` sind entfallen.

## Ziele

- Ohne Einstellung: die Bench auf diesem Rechner, `http://127.0.0.1:4320`
  (`BENCH_URL`), Token aus deren Datenordner (`api-token`; `BENCH_TOKEN` oder
  `BENCH_DATA_DIR` überschreiben).
- Weitere Instanzen in `~/.config/bench-nats/targets.json`:

  ```json
  { "linux": { "url": "http://192.168.8.135:4320", "token": "…" } }
  ```

  Ohne `token` gilt das der lokalen Bench. `set_target` merkt sich die Wahl in
  `~/.config/bench-nats/session.json`.

## Claude Desktop

```json
"mcpServers": {
  "bench-nats": { "command": "/Applications/Onisin OS - Bench.app/Contents/MacOS/bench-nats" }
}
```

## Wichtig

stdout trägt die JSON-RPC-Frames. Diagnosen nur nach stderr.

## Lizenz

Apache License 2.0 – siehe `LICENSE`.

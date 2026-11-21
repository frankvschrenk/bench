# bench-mcp

stdio-MCP-Server für **Claude Desktop**, der die Werkzeuge einer laufenden
Bench durchreicht. Claude startet ihn als Kindprozess; jeder Werkzeugaufruf
geht als `POST <bench>/call/<operation>` mit Bearer-Token an die gewählte
Bench-Instanz.

Bis 14.11.2026 hieß die Brücke `bench-nats` und sprach NATS. Werkzeuge und
Argumente sind gleich geblieben (`list_operations`, `set_target`,
`send_message {subject, payload, target?}`); sie liegen im Crate `bench-tools`,
das die Bench auch selbst unter `/mcp` (Streamable HTTP) anbietet.
`watch_subject`/`unwatch_subject` sind entfallen. Dateien im alten Ordner
`~/.config/bench-nats` werden noch gelesen.

## Ziele

- Ohne Einstellung: die Bench auf diesem Rechner, `http://127.0.0.1:4320`
  (`BENCH_URL`), Token aus deren Datenordner (`api-token`; `BENCH_TOKEN` oder
  `BENCH_DATA_DIR` überschreiben).
- Weitere Instanzen in `~/.config/bench-mcp/targets.json`:

  ```json
  { "linux": { "url": "http://192.168.8.135:4320", "token": "…" } }
  ```

  Ohne `token` gilt das der lokalen Bench. `set_target` merkt sich die Wahl in
  `~/.config/bench-mcp/session.json`.

## Claude Desktop

```json
"mcpServers": {
  "bench-mcp": { "command": "/Applications/Bench.app/Contents/MacOS/bench-mcp" }
}
```

## Wichtig

stdout trägt die JSON-RPC-Frames. Diagnosen nur nach stderr.

## Lizenz

Apache License 2.0 – siehe `LICENSE`.

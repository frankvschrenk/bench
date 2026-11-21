# bench

Desktop-App, die Claude Werkzeuge gibt: Dateien, Suche, exec, git, Postgres,
Handbuch/Tasks und das Projektgedächtnis (oosmem). Claude spricht sie über
**MCP** an. Nebenbei bringt sie einen Dev-IdP (OAuth2/PKCE) und einen
OTLP-Log-Empfänger mit. Die Oberfläche zeigt Tool-Aufrufe, Logs, die
IdP-Benutzer und die Einstellungen.

Portierung der Tauri-Bench aus `onisin/apps/bench` nach **egui 0.37** mit den
Carbon-Widgets aus [`carbon_egui`](https://github.com/frankvschrenk/carbon_egui).
Seit 14.11.2026 ohne NATS: Die Operationen (`bench.fs.read` …) und ihre
JSON-Antworten sind unverändert, nur der Weg dorthin ist HTTP.

## Schnittstelle

Die Bench lauscht auf `127.0.0.1:4320` (Einstellungen → Schnittstelle; Port und
„im Netz erreichbar“ wirken nach einem Neustart):

| Pfad | Zweck | Token |
|---|---|---|
| `/mcp` | MCP über Streamable HTTP: `list_operations`, `set_target`, `send_message` | ja |
| `/call/<operation>` | POST mit JSON-Payload, Antwort = JSON der Operation (für bench-mcp) | ja |
| `/health` | `{ok, instance, version, memory}` | nein |

Das Token (`Authorization: Bearer …`) liegt als `api-token` im Datenordner und
entsteht beim ersten Start. Auf der Seite „Schnittstelle“ stehen die fertigen
Befehle für Claude Code und Claude Desktop.

- **Claude Code:** `claude mcp add --transport http bench http://127.0.0.1:4320/mcp --header "Authorization: Bearer <token>"`
- **Claude Desktop** startet lokale Server nur per stdio. Dafür gibt es
  `bench-mcp` (liegt neben der Bench, in der .app unter `Contents/MacOS`):
  `"bench-mcp": { "command": "<pfad>/bench-mcp" }`. Es liest Token und
  Adresse der lokalen Bench selbst; weitere Instanzen trägt man in
  `~/.config/bench-mcp/targets.json` ein:
  `{ "linux": { "url": "http://192.168.8.135:4320", "token": "…" } }`
  (die Bench dort mit „im Netz erreichbar“). `set_target linux` wählt sie aus.

## Workspace

| Crate | Aufgabe | Lizenz |
|---|---|---|
| `.` (bench) | die App | – |
| `crates/oosmem` | Gedächtnis (Handbuch, Notizen, Suche), als Bibliothek in der Bench | BSL-1.1 |
| `crates/bench-tools` | die MCP-Werkzeuge, geteilt von Bench (`/mcp`) und bench-mcp | Apache-2.0 |
| `crates/bench-mcp` | stdio-Brücke für Claude Desktop → HTTP | Apache-2.0 |

oosmem und bench-mcp kamen am 14.11.2026 mit Historie aus `onisin/services`.
oosmem war bis dahin ein eigener Dienst über NATS; jetzt öffnet die Bench den
Speicher selbst (Ordner: Einstellungen → Allgemein → Gedächtnis, Standard ist
der bisherige Ordner, z. B. `~/Library/Application Support/oosmem`). Eine
Sperrdatei verhindert, dass zwei Prozesse denselben Speicher beschreiben.
Zusammenfassungen (`episode.summarize`) laufen über Ollama; das Standardmodell
`gemma4:e4b-mlx` gibt es nur auf dem Mac, sonst `gemma4:e4b`.

## Bauen

`carbon_egui` muss neben diesem Repo liegen (Pfad-Abhängigkeit `../carbon_egui`):

```
~/repro/bench
~/repro/carbon_egui
```

```sh
cargo run              # Debug
cargo build --release --workspace  # target/release/{bench,bench-mcp}(.exe)
cargo test --workspace
./scripts/bundle-macos.sh   # macOS: target/release/bundle/Bench.app (mit bench-mcp)
```

Die .app ist ad-hoc signiert. Installieren: nach `/Applications` kopieren
(`ditto "target/release/bundle/Bench.app" "/Applications/Bench.app"`).
Sie nutzt denselben Datenordner wie die Tauri-Bench – nicht beide gleichzeitig
starten (gleiche Ports 4318/5556, gleiches Gedächtnis).

Rust ≥ 1.95. Unter Linux braucht eframe die Pakete `libxkbcommon-dev
libwayland-dev libgl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev
libxi-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev`.

## Daten und Einstellungen

Alles liegt im Datenordner `com.onisin.bench` – derselbe Ordner wie bei der
Tauri-Bench, eine vorhandene Installation wird also übernommen:

| System  | Ordner |
|---------|--------|
| macOS   | `~/Library/Application Support/com.onisin.bench` |
| Linux   | `~/.local/share/com.onisin.bench` |
| Windows | `%APPDATA%\com.onisin.bench` |

Darin `settings.json` (Format von tauri-plugin-store: `{"settings": {...}}`),
`events.db` (SQLite-Protokoll aller Aufrufe), `api-token` und `iam/`
(Signierschlüssel).

Umgebungsvariablen:

- `BENCH_DATA_DIR=<ordner>` – anderer Datenordner, z. B. für eine Testinstanz
  neben der echten Bench (eigene Settings mit anderem `apiPort`, `otlpPort`
  und `memoryDir`).
- `BENCH_PAGE=<n>` – Startseite (0 Tool-Aufrufe, 1 Logs, 2 Benutzer,
  3 Allgemein, 4 Datenbank, 5 Schnittstelle, 6 Freigegebene Ordner).
- `BENCH_SHOT=<datei.png>` – Bildschirmfoto nach `BENCH_SHOT_DELAY` Sekunden
  (Std. 1) speichern und beenden.

## Aufbau

- `src/main.rs`, `src/boot.rs` – Start: tokio-Runtime, Settings, Gedächtnis,
  Event-Log, IdP, OTLP, Schnittstelle; danach das egui-Fenster.
- `src/api.rs` – HTTP: `/mcp`, `/call`, `/health`, Token.
- `src/observe.rs` – Brücke zum Fenster (`UiSink`, Kanal + `request_repaint`),
  Übernahme gespeicherter Einstellungen, OTLP-Empfänger.
- `src/dispatcher.rs`, `src/tools/` – die `bench.*`-Handler (aus der
  Tauri-Bench übernommen, `exec`/`fs` zusätzlich für Windows).
- `src/iam/` – Dev-IdP.
- `src/app/` – Oberfläche mit carbon-egui: Header + SideNav, `DataTable` für
  Tool-Aufrufe und Logs, Formulare für die Einstellungen, Benutzerverwaltung.

## CI

`.github/workflows/build.yml` baut und testet unter Linux, Windows und macOS
(als .app) und lädt die Programme als Artefakte hoch; ein Tag `v*` erzeugt ein
GitHub-Release mit `bench-linux-x86_64.tar.gz`, `bench-windows-x86_64.zip` und
`bench-macos-arm64.zip`. Da `carbon_egui`
privat ist, braucht das Repo das Secret `CARBON_EGUI_TOKEN` (fine-grained
Token, Contents: read auf `frankvschrenk/carbon_egui`).

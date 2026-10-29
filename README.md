# bench

Desktop-App, die auf NATS als Request-Reply-Responder für `bench.*` antwortet
(Dateien, Suche, exec, git, Postgres, Handbuch/Tasks) und nebenbei einen
Dev-IdP (OAuth2/PKCE) und einen OTLP-Log-Empfänger mitbringt. Die Oberfläche
zeigt Tool-Aufrufe, Logs, die IdP-Benutzer und die Einstellungen.

Portierung der Tauri-Bench aus `onisin/apps/bench` nach **egui 0.37** mit den
Carbon-Widgets aus [`carbon_egui`](https://github.com/frankvschrenk/carbon_egui).
Der Draht-Vertrag (Subjects, JSON-Antworten, `oos.bench.debug`, IdP-Endpunkte,
KV `oos-iam`) ist unverändert.

## Bauen

`carbon_egui` muss neben diesem Repo liegen (Pfad-Abhängigkeit `../carbon_egui`):

```
~/repro/bench
~/repro/carbon_egui
```

```sh
cargo run              # Debug
cargo build --release  # target/release/bench (bench.exe unter Windows)
cargo test
./scripts/bundle-macos.sh   # macOS: target/release/bundle/Onisin OS - Bench.app
```

Die .app ist ad-hoc signiert. Installieren: nach `/Applications` kopieren
(`ditto "target/release/bundle/Onisin OS - Bench.app" "/Applications/Onisin OS - Bench.app"`).
Sie nutzt denselben Datenordner wie die Tauri-Bench – nicht beide gleichzeitig
starten (gleiche NATS-Queue, gleiche Ports 4318/5556).

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
`events.db` (SQLite-Protokoll aller Aufrufe) und `iam/` (Signierschlüssel).

Umgebungsvariablen:

- `BENCH_DATA_DIR=<ordner>` – anderer Datenordner, z. B. für eine Testinstanz
  neben der echten Bench (eigene Settings mit anderem NATS-Server, sonst teilt
  sie sich die Queue `bench` mit der echten Instanz).
- `BENCH_PAGE=<n>` – Startseite (0 Tool-Aufrufe, 1 Logs, 2 Benutzer,
  3 Allgemein, 4 Datenbank, 5 NATS-Server, 6 Freigegebene Ordner).
- `BENCH_SHOT=<datei.png>` – Bildschirmfoto nach `BENCH_SHOT_DELAY` Sekunden
  (Std. 1) speichern und beenden.

## Aufbau

- `src/main.rs`, `src/boot.rs` – Start: tokio-Runtime, Settings, Event-Log,
  IdP, OTLP, NATS-Supervisor; danach das egui-Fenster.
- `src/observe.rs` – Brücke zum Fenster (`UiSink`, Kanal + `request_repaint`),
  Reconnect-Supervisor, OTLP-Empfänger.
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

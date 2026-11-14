//! Settings pages: General, Database, tool interface, allowed roots.
//
// Every page edits a draft and hands a complete BenchSettings back when the
// user saves; the app persists it and applies it. Roots save on every
// add/remove, as in the Tauri bench.

use carbon_egui::{Button, ButtonKind, ButtonSize, Checkbox, Column, DataTable, Form, Icon, TableAction, TableSize, TextArea, TextInput};
use egui::Ui;

use super::style;
use crate::db::rewrite_database;
use crate::settings::{AllowedRoot, BenchSettings};

const FORM_WIDTH: f32 = 640.0;

fn save_button(ui: &mut Ui, enabled: bool) -> bool {
    ui.add_space(32.0);
    ui.add(Button::new("Speichern").size(ButtonSize::Medium).disabled(!enabled)).clicked()
}

// ─── General ────────────────────────────────────────────────────

#[derive(Default)]
pub struct GeneralPage {
    instance_name: String,
    otlp_port: String,
    iam_issuer: String,
    memory_dir: String,
    llm_url: String,
    llm_model: String,
}

impl GeneralPage {
    pub fn reset(&mut self, s: &BenchSettings) {
        self.instance_name = s.instance_name.clone();
        self.otlp_port = s.otlp_port.to_string();
        self.iam_issuer = s.iam_issuer.clone();
        self.memory_dir = s.memory_dir.clone();
        self.llm_url = s.llm_url.clone();
        self.llm_model = s.llm_model.clone();
    }

    pub fn show(&mut self, ui: &mut Ui, s: &BenchSettings) -> Option<BenchSettings> {
        style::page_title(ui, "Allgemein", "Name und lokale Dienste dieser Bench-Instanz.");
        let port = self.otlp_port.trim().parse::<u16>().ok().filter(|p| *p >= 1024);
        let issuer = self.iam_issuer.trim().trim_end_matches('/').to_string();
        let issuer_ok = issuer.is_empty() || crate::iam::issuer_host_port(&issuer).is_some();
        let mut save = false;
        Form::new().width(FORM_WIDTH).show(ui, |form| {
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.instance_name)
                        .label("Instanzname")
                        .placeholder("macos")
                        .helper_text("Name dieser Bench (z. B. für set_target und targets.json von bench-nats)."),
                )
            });
            form.section("Lokale Dienste");
            form.full(|ui| {
                let mut input = TextInput::new(&mut self.otlp_port)
                    .label("OTLP-Port")
                    .placeholder("4318")
                    .width(200.0)
                    .helper_text("HTTP-Port des lokalen OTel-Log-Empfängers (oosl → Bench). Wirkt nach einem Neustart.");
                if port.is_none() {
                    input = input.invalid("Port zwischen 1024 und 65535");
                }
                ui.add(input)
            });
            form.full(|ui| {
                let mut input = TextInput::new(&mut self.iam_issuer)
                    .label("Issuer-URL des Dev-IdP")
                    .placeholder("http://localhost:5556")
                    .helper_text("Leer = nur dieser Rechner. Mit Rechnername oder IP (z. B. http://192.168.1.71:5556) lauscht der IdP im Netz – Server müssen dieselbe URL als Issuer bekommen. Wirkt nach einem Neustart.");
                if !issuer_ok {
                    input = input.invalid("http(s)://host[:port]");
                }
                ui.add(input)
            });
            form.section("Gedächtnis (oosmem)");
            form.full(|ui| {
                let default = crate::settings::memory_dir(&BenchSettings::default());
                ui.add(
                    TextInput::new(&mut self.memory_dir)
                        .label("Ordner")
                        .placeholder(default.display().to_string())
                        .helper_text("Handbuch, Notizen und Suche. Leer = Standardordner (derselbe wie beim früheren oosmem-Dienst). Wirkt nach einem Neustart."),
                )
            });
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.llm_url)
                        .label("Ollama-URL für Zusammenfassungen")
                        .placeholder("http://127.0.0.1:11434")
                        .helper_text("Leer = keine Zusammenfassungen (episode.summarize). Wirkt nach einem Neustart."),
                )
            });
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.llm_model)
                        .label("Modell")
                        .placeholder("gemma4:e4b")
                        .helper_text("MLX-Modelle (…-mlx) laufen nur auf dem Mac."),
                )
            });
            form.full(|ui| save = save_button(ui, port.is_some() && issuer_ok));
        });
        let port = port?;
        save.then(|| BenchSettings {
            instance_name: self.instance_name.trim().to_string(),
            otlp_port: port,
            iam_issuer: issuer,
            memory_dir: self.memory_dir.trim().to_string(),
            llm_url: self.llm_url.trim().to_string(),
            llm_model: self.llm_model.trim().to_string(),
            ..s.clone()
        })
    }
}

// ─── Database ───────────────────────────────────────────────────

#[derive(Default)]
pub struct DatabasePage {
    dsn: String,
    app_database: String,
}

impl DatabasePage {
    pub fn reset(&mut self, s: &BenchSettings) {
        self.dsn = s.dsn.clone();
        self.app_database = s.app_database.clone();
    }

    pub fn show(&mut self, ui: &mut Ui, s: &BenchSettings) -> Option<BenchSettings> {
        style::page_title(
            ui,
            "Datenbank",
            "PostgreSQL-Verbindung für die Tools pg.*, memory.* und task.* sowie den Dev-IdP.",
        );
        let dsn = self.dsn.trim();
        let app_db = self.app_database.trim();
        let mut bench_url = if dsn.is_empty() { "— (DSN fehlt)".to_string() } else { rewrite_database(dsn, "bench") };
        let mut app_url = if dsn.is_empty() || app_db.is_empty() {
            "— (DSN und Anwendungsdatenbank fehlen)".to_string()
        } else {
            rewrite_database(dsn, app_db)
        };
        let mut save = false;
        Form::new().width(FORM_WIDTH).show(ui, |form| {
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.dsn)
                        .label("DSN")
                        .placeholder("postgres://postgres:demo@localhost:5432?sslmode=disable")
                        .helper_text("libpq-Verbindung ohne Datenbanknamen. Die Bench hängt „/bench“ bzw. die Anwendungsdatenbank an."),
                )
            });
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.app_database)
                        .label("Anwendungsdatenbank")
                        .placeholder("onisin")
                        .helper_text("Datenbank, auf der pg.* arbeitet, meist „onisin“."),
                )
            });
            form.section("Daraus ergibt sich");
            form.full(|ui| ui.add(TextInput::new(&mut bench_url).label("memory.* und task.*").read_only(true).id_salt("bench-url")));
            form.full(|ui| ui.add(TextInput::new(&mut app_url).label("pg.* und Dev-IdP").read_only(true).id_salt("app-url")));
            form.full(|ui| save = save_button(ui, true));
        });
        save.then(|| BenchSettings {
            dsn: self.dsn.trim().to_string(),
            app_database: self.app_database.trim().to_string(),
            ..s.clone()
        })
    }
}

// ─── Tool interface ─────────────────────────────────────────────

pub struct InterfacePage {
    port: String,
    network: bool,
    token: String,
}

impl InterfacePage {
    pub fn new(token: String) -> Self {
        Self { port: String::new(), network: false, token }
    }

    pub fn reset(&mut self, s: &BenchSettings) {
        self.port = s.api_port.to_string();
        self.network = s.api_network;
    }

    pub fn show(&mut self, ui: &mut Ui, s: &BenchSettings) -> Option<BenchSettings> {
        style::page_title(
            ui,
            "Schnittstelle",
            "Claude erreicht die Bench über MCP (HTTP). Port und Netzmodus gelten nach einem Neustart.",
        );
        let port = self.port.trim().parse::<u16>().ok().filter(|p| *p >= 1024);
        let mut token = self.token.clone();
        let mut snippets = claude_snippets(s.api_port, &self.token);
        let mut save = false;
        Form::new().width(FORM_WIDTH).show(ui, |form| {
            form.full(|ui| {
                let mut input = TextInput::new(&mut self.port)
                    .label("Port")
                    .placeholder("4320")
                    .width(200.0)
                    .helper_text("/mcp (MCP über Streamable HTTP), /call/<operation> (für bench-nats), /health.");
                if port.is_none() {
                    input = input.invalid("Port zwischen 1024 und 65535");
                }
                ui.add(input)
            });
            form.full(|ui| {
                ui.add(Checkbox::new(&mut self.network, "Im Netz erreichbar (lauscht auf 0.0.0.0 statt 127.0.0.1)"))
            });
            form.section("Zugang");
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut token)
                        .label("Token")
                        .read_only(true)
                        .password(true)
                        .id_salt("api-token")
                        .helper_text("Bearer-Token für /mcp und /call. Liegt als api-token im Datenordner; bench-nats liest es dort auf diesem Rechner selbst."),
                )
            });
            form.section("Claude einrichten");
            form.full(|ui| {
                ui.add(
                    TextArea::new(&mut snippets)
                        .label("Befehle und Konfiguration")
                        .read_only(true)
                        .monospace(true)
                        .rows(16)
                        .id_salt("claude-snippets"),
                )
            });
            form.full(|ui| save = save_button(ui, port.is_some()));
        });
        let port = port?;
        save.then(|| BenchSettings { api_port: port, api_network: self.network, ..s.clone() })
    }
}

/// Copy-ready setup for Claude Code, Claude Desktop and other machines.
fn claude_snippets(port: u16, token: &str) -> String {
    let exe = if cfg!(windows) { "bench-nats.exe" } else { "bench-nats" };
    let bridge = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| exe.to_string());
    let bridge_json = serde_json::to_string(&bridge).unwrap_or_default();
    format!(
        "# Claude Code (MCP direkt über HTTP)\n\
claude mcp add --transport http bench http://127.0.0.1:{port}/mcp \\\n  --header \"Authorization: Bearer {token}\"\n\n\
# Claude Desktop: claude_desktop_config.json → mcpServers\n\
\"bench-nats\": {{ \"command\": {bridge_json} }}\n\n\
# Andere Rechner für bench-nats: ~/.config/bench-nats/targets.json\n\
{{ \"linux\": {{ \"url\": \"http://<rechner>:{port}\", \"token\": \"<token jener Bench>\" }} }}\n"
    )
}

// ─── Allowed roots ──────────────────────────────────────────────

#[derive(Default)]
pub struct RootsPage {
    path: String,
}

impl RootsPage {
    pub fn show(&mut self, ui: &mut Ui, s: &BenchSettings) -> Option<BenchSettings> {
        style::page_title(
            ui,
            "Freigegebene Ordner",
            "Ordner, in denen die Bench lesen und schreiben darf. Pfade außerhalb werden abgelehnt.",
        );
        let mut next = None;

        let columns = [Column::new("Pfad")];
        let remove = [TableAction::icon_only(Icon::TrashCan, "Entfernen")];
        let r = DataTable::new("roots", &columns, s.roots.len())
            .size(TableSize::Medium)
            .width(FORM_WIDTH + 160.0)
            .row_buttons(&remove)
            .row_actions_always(true)
            .empty_state("Keine Ordner freigegeben", "Ohne Freigabe lehnt die Bench alle Dateizugriffe ab.")
            .show(ui, |row, _| s.roots[row].path.clone());
        if let Some((row, _)) = r.row_button {
            let mut roots = s.roots.clone();
            roots.remove(row);
            next = Some(BenchSettings { roots, ..s.clone() });
        }

        ui.add_space(32.0);
        let path = self.path.trim().to_string();
        let known = s.roots.iter().any(|r| r.path == path);
        let mut add = false;
        Form::new().width(FORM_WIDTH).show(ui, |form| {
            form.section("Ordner hinzufügen");
            form.full(|ui| {
                let mut input = TextInput::new(&mut self.path).label("Pfad").placeholder(placeholder_root());
                if known {
                    input = input.invalid("Dieser Ordner ist schon freigegeben");
                }
                ui.add(input)
            });
            form.full(|ui| {
                add = ui
                    .add(
                        Button::new("Hinzufügen")
                            .icon(Icon::Add)
                            .kind(ButtonKind::Tertiary)
                            .size(ButtonSize::Medium)
                            .disabled(path.is_empty() || known),
                    )
                    .clicked();
            });
        });
        if add && !path.is_empty() && !known {
            let mut roots = s.roots.clone();
            roots.push(AllowedRoot { path });
            self.path.clear();
            next = Some(BenchSettings { roots, ..s.clone() });
        }
        next
    }
}

fn placeholder_root() -> String {
    let sep = std::path::MAIN_SEPARATOR;
    format!("{}{sep}repro{sep}onisin", crate::home_dir())
}

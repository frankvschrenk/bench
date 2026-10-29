//! Settings pages: General, Database, NATS servers, allowed roots.
//
// Every page edits a draft and hands a complete BenchSettings back when the
// user saves; the app persists it and triggers the NATS reconnect. Servers and
// roots save on every add/remove, as in the Tauri bench.

use std::collections::HashMap;

use carbon_egui::{Button, ButtonKind, ButtonSize, Column, DataTable, Form, Icon, TableAction, TableSize, Tag, TagColor, TagSize, TextInput};
use egui::Ui;

use super::style;
use crate::db::rewrite_database;
use crate::settings::{AllowedRoot, BenchSettings, NatsServer};

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
}

impl GeneralPage {
    pub fn reset(&mut self, s: &BenchSettings) {
        self.instance_name = s.instance_name.clone();
        self.otlp_port = s.otlp_port.to_string();
        self.iam_issuer = s.iam_issuer.clone();
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
                        .helper_text("bench-nats erreicht diese Bench gezielt über „{name}.bench.*“."),
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
            form.full(|ui| save = save_button(ui, port.is_some() && issuer_ok));
        });
        let port = port?;
        save.then(|| BenchSettings {
            instance_name: self.instance_name.trim().to_string(),
            otlp_port: port,
            iam_issuer: issuer,
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

// ─── NATS servers ───────────────────────────────────────────────

pub struct ServersPage {
    name: String,
    url: String,
}

impl Default for ServersPage {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: "nats://localhost:4222".into(),
        }
    }
}

impl ServersPage {
    pub fn show(&mut self, ui: &mut Ui, s: &BenchSettings, statuses: &HashMap<String, bool>) -> Option<BenchSettings> {
        style::page_title(
            ui,
            "NATS-Server",
            "Server, mit denen sich die Bench verbindet. Speichern verbindet sofort neu.",
        );
        let mut next = None;

        let columns = [Column::new("Name").width(180.0), Column::new("URL").width(300.0), Column::new("Status")];
        let remove = [TableAction::icon_only(Icon::TrashCan, "Entfernen")];
        let r = DataTable::new("servers", &columns, s.servers.len())
            .size(TableSize::Medium)
            .width(FORM_WIDTH + 160.0)
            .row_buttons(&remove)
            .row_actions_always(true)
            .cell_ui(|ui, row, col| {
                if col != 2 {
                    return false;
                }
                let up = statuses.get(&s.servers[row].name).copied().unwrap_or(false);
                Tag::new(if up { "verbunden" } else { "getrennt" })
                    .color(if up { TagColor::Green } else { TagColor::Red })
                    .size(TagSize::Small)
                    .show(ui);
                true
            })
            .empty_state("Keine Server", "Unten einen NATS-Server hinzufügen.")
            .show(ui, |row, col| {
                let srv = &s.servers[row];
                match col {
                    0 => srv.name.clone(),
                    1 => srv.url.clone(),
                    _ => String::new(),
                }
            });
        if let Some((row, _)) = r.row_button {
            let mut servers = s.servers.clone();
            servers.remove(row);
            next = Some(BenchSettings { servers, ..s.clone() });
        }

        ui.add_space(32.0);
        let (name, url) = (self.name.trim().to_string(), self.url.trim().to_string());
        let mut add = false;
        Form::new().columns(2).width(FORM_WIDTH).show(ui, |form| {
            form.section("Server hinzufügen");
            form.row(|row| {
                row.cell(|ui| ui.add(TextInput::new(&mut self.name).label("Name").placeholder("macos-local")));
                row.cell(|ui| ui.add(TextInput::new(&mut self.url).label("URL").placeholder("nats://localhost:4222")));
            });
            form.full(|ui| {
                add = ui
                    .add(
                        Button::new("Hinzufügen")
                            .icon(Icon::Add)
                            .kind(ButtonKind::Tertiary)
                            .size(ButtonSize::Medium)
                            .disabled(name.is_empty() || url.is_empty()),
                    )
                    .clicked();
            });
        });
        if add && !name.is_empty() && !url.is_empty() {
            // Same name replaces the old entry, as in the Tauri bench.
            let mut servers: Vec<NatsServer> = s.servers.iter().filter(|x| x.name != name).cloned().collect();
            servers.push(NatsServer { name, url });
            self.name.clear();
            self.url = "nats://localhost:4222".into();
            next = Some(BenchSettings { servers, ..s.clone() });
        }
        next
    }
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

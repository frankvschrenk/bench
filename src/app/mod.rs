//! The bench window: Carbon UI shell (header + side nav) around the feeds,
//! the IdP admin and the settings pages.

mod feed;
mod settings_view;
mod style;
mod task;
mod users;

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use carbon_egui::theme::color;
use carbon_egui::{Header, Icon, SideNav, Tag, TagColor, TagSize, Toast, Toasts};
use egui::{vec2, Frame, Margin, RichText};
use tokio::runtime::Runtime;

use crate::boot::Backend;
use crate::observe::{AppState, UiEvent};
use crate::settings::BenchSettings;
use feed::{FeedAction, LogsView, ToolsView};
use settings_view::{DatabasePage, GeneralPage, RootsPage, ServersPage};
use users::UsersView;

const PAGE_TOOLS: usize = 0;
const PAGE_LOGS: usize = 1;
const PAGE_USERS: usize = 2;
const PAGE_GENERAL: usize = 3;
const PAGE_DATABASE: usize = 4;
const PAGE_SERVERS: usize = 5;
const PAGE_ROOTS: usize = 6;

const PAGES: [(Option<Icon>, &str); 7] = [
    (Some(Icon::Switcher), "Tool-Aufrufe"),
    (Some(Icon::Notification), "Logs"),
    (Some(Icon::User), "Benutzer"),
    (Some(Icon::Settings), "Allgemein"),
    (Some(Icon::Document), "Datenbank"),
    (Some(Icon::Send), "NATS-Server"),
    (Some(Icon::Folder), "Freigegebene Ordner"),
];

pub struct BenchApp {
    // Dropped last: the window thread owns the runtime the backend runs on.
    runtime: Runtime,
    state: Arc<AppState>,
    events: Receiver<UiEvent>,
    /// The window's copy of the settings (the backend holds the live one).
    settings: BenchSettings,
    statuses: HashMap<String, bool>,

    page: usize,
    menu_open: bool,
    paused: bool,
    tools: ToolsView,
    logs: LogsView,
    users: UsersView,
    general: GeneralPage,
    database: DatabasePage,
    servers: ServersPage,
    roots: RootsPage,
    toasts: Toasts,
    started: std::time::Instant,
    shot_taken: bool,
}

impl BenchApp {
    pub fn new(runtime: Runtime, backend: Backend) -> Self {
        let Backend { state, iam, events, settings } = backend;
        let mut app = Self {
            runtime,
            state,
            events,
            settings,
            statuses: HashMap::new(),
            page: PAGE_TOOLS,
            menu_open: true,
            paused: false,
            tools: ToolsView::default(),
            logs: LogsView::default(),
            users: UsersView::new(iam),
            general: GeneralPage::default(),
            database: DatabasePage::default(),
            servers: ServersPage::default(),
            roots: RootsPage::default(),
            toasts: Toasts::default().offset(vec2(16.0, 64.0)),
            started: std::time::Instant::now(),
            shot_taken: false,
        };
        if let Some(page) = std::env::var("BENCH_PAGE").ok().and_then(|p| p.parse().ok()) {
            app.page = page;
        }
        app.reset_drafts();
        app
    }

    fn reset_drafts(&mut self) {
        self.general.reset(&self.settings);
        self.database.reset(&self.settings);
    }

    // Drain everything the backend sent since the last frame. Paused feeds
    // drop new entries (as the old window did); status always applies.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::Tool(e) if !self.paused => self.tools.push(e),
                UiEvent::Log(r) if !self.paused => self.logs.push(r),
                UiEvent::Status(s) => {
                    self.statuses.insert(s.name, s.connected);
                }
                _ => {}
            }
        }
    }

    fn save(&mut self, next: BenchSettings) {
        let otlp_changed = next.otlp_port != self.settings.otlp_port;
        match self.runtime.block_on(self.state.save_settings(next.clone())) {
            Ok(()) => {
                self.settings = next;
                self.reset_drafts();
                let toast = Toast::success("Gespeichert").subtitle("NATS wird neu verbunden.");
                self.toasts.add(if otlp_changed { toast.subtitle("Der neue OTLP-Port gilt nach einem Neustart.") } else { toast });
            }
            Err(e) => self.toasts.add(Toast::error("Speichern fehlgeschlagen").subtitle(e)),
        }
    }

    fn feed_action(&mut self, action: Option<FeedAction>) {
        match action {
            Some(FeedAction::TogglePause) => self.paused = !self.paused,
            Some(FeedAction::Clear) => {
                self.tools.clear();
                self.logs.clear();
            }
            None => {}
        }
    }

    fn side_nav(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        let before = self.page;
        SideNav::new(&PAGES, &mut self.page).label("Bereiche").show(ui);
        if self.page != before {
            // Leaving a settings page discards unsaved edits, like the old panels.
            self.reset_drafts();
        }

        // Instance and connection state at the bottom, as in the old sidebar.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            Frame::NONE.inner_margin(Margin::same(16)).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                ui.vertical(|ui| {
                    if !self.settings.instance_name.is_empty() {
                        Tag::new(format!("Instanz {}", self.settings.instance_name))
                            .color(TagColor::Purple)
                            .size(TagSize::Small)
                            .show(ui);
                    }
                    if self.settings.servers.is_empty() {
                        ui.label(RichText::new("Kein NATS-Server eingetragen").size(12.0).color(color::TEXT_HELPER));
                    }
                    for srv in &self.settings.servers {
                        let up = self.statuses.get(&srv.name).copied().unwrap_or(false);
                        Tag::new(&srv.name)
                            .color(if up { TagColor::Green } else { TagColor::Red })
                            .size(TagSize::Small)
                            .show(ui)
                            .response
                            .on_hover_text(format!("{} – {}", srv.url, if up { "verbunden" } else { "getrennt" }));
                    }
                });
            });
        });
    }
}

impl eframe::App for BenchApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::Panel::top("header").exact_size(48.0).frame(Frame::NONE).show(ui, |ui| {
            let unread_errors = u32::from(self.logs.has_errors() && self.page != PAGE_LOGS);
            let r = Header::new()
                .menu(&mut self.menu_open)
                .product("Onisin OS", "Bench")
                .actions(&[(Icon::Notification, "Logs mit Fehlern")])
                .badge(0, unread_errors)
                .show(ui);
            if r.action_clicked == Some(0) {
                self.page = PAGE_LOGS;
            }
        });
        if self.menu_open {
            egui::Panel::left("sidenav")
                .exact_size(256.0)
                .resizable(false)
                .frame(Frame::NONE.fill(color::LAYER_01))
                .show(ui, |ui| self.side_nav(ui));
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(color::BACKGROUND).inner_margin(32))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                match self.page {
                    PAGE_TOOLS => {
                        let action = self.tools.show(ui, self.paused);
                        self.feed_action(action);
                    }
                    PAGE_LOGS => {
                        let action = self.logs.show(ui, self.paused, self.settings.otlp_port);
                        self.feed_action(action);
                    }
                    PAGE_USERS => self.users.show(ui, &self.runtime, &mut self.toasts),
                    page => {
                        let settings = self.settings.clone();
                        let statuses = &self.statuses;
                        let next = egui::ScrollArea::vertical()
                            .auto_shrink(false)
                            .show(ui, |ui| match page {
                                PAGE_GENERAL => self.general.show(ui, &settings),
                                PAGE_DATABASE => self.database.show(ui, &settings),
                                PAGE_SERVERS => self.servers.show(ui, &settings, statuses),
                                PAGE_ROOTS => self.roots.show(ui, &settings),
                                _ => None,
                            })
                            .inner;
                        if let Some(next) = next {
                            self.save(next);
                        }
                    }
                }
            });

        self.toasts.show(ui.ctx());
        self.screenshot_once(ui.ctx());
    }
}

impl BenchApp {
    // BENCH_SHOT=<datei.png>: nach BENCH_SHOT_DELAY Sekunden (Std. 1) die
    // Oberfläche als PNG speichern und beenden (Screenshots fürs README,
    // Prüfen ohne Bildschirm).
    fn screenshot_once(&mut self, ctx: &egui::Context) {
        if self.shot_taken {
            return;
        }
        let Some(path) = std::env::var_os("BENCH_SHOT") else {
            self.shot_taken = true;
            return;
        };
        let delay: f32 = std::env::var("BENCH_SHOT_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(1.0);
        if self.started.elapsed().as_secs_f32() < delay {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        self.shot_taken = true;
        ctx.request_screenshot(move |image: std::sync::Arc<egui::ColorImage>| {
            let [w, h] = image.size;
            let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Err(e) = image::save_buffer(&path, &bytes, w as u32, h as u32, image::ColorType::Rgba8) {
                eprintln!("[bench] screenshot: {e}");
            }
            std::process::exit(0);
        });
    }
}

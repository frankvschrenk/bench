//! Tools and Logs: the live feeds of tool calls (oos.bench.debug) and OTLP
//! log records, newest first, capped at MAX_EVENTS like the old window.

use std::collections::VecDeque;

use carbon_egui::{Column, DataTable, Icon, TableAction, TableSize, Tag, TagColor, TagSize};
use egui::{Color32, RichText, Ui};

use super::style;
use crate::telemetry::{LogRecord, ToolEvent};

pub const MAX_EVENTS: usize = 500;

/// What the feed toolbar asked for.
pub enum FeedAction {
    TogglePause,
    Clear,
}

fn toolbar(paused: bool) -> [TableAction; 2] {
    [
        TableAction::new(if paused { "Fortsetzen" } else { "Anhalten" }),
        TableAction::new("Leeren").icon(Icon::TrashCan),
    ]
}

fn feed_action(index: Option<usize>) -> Option<FeedAction> {
    match index {
        Some(0) => Some(FeedAction::TogglePause),
        Some(1) => Some(FeedAction::Clear),
        _ => None,
    }
}

fn contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

// Expanded rows are remembered by record key, not by position: new events
// arrive on top and would otherwise shift the open detail to another row.
fn positions(keys: &[String], visible_keys: &[&str]) -> Vec<usize> {
    keys.iter()
        .filter_map(|k| visible_keys.iter().position(|v| v == k))
        .collect()
}

fn keys_of(positions: &[usize], visible_keys: &[&str]) -> Vec<String> {
    positions
        .iter()
        .filter_map(|&p| visible_keys.get(p).map(|k| k.to_string()))
        .collect()
}

// ─── Tools ──────────────────────────────────────────────────────

#[derive(Default)]
pub struct ToolsView {
    pub events: VecDeque<ToolEvent>,
    search: String,
    open: Vec<String>,
}

impl ToolsView {
    pub fn push(&mut self, event: ToolEvent) {
        self.events.push_front(event);
        self.events.truncate(MAX_EVENTS);
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.open.clear();
    }

    pub fn show(&mut self, ui: &mut Ui, paused: bool) -> Option<FeedAction> {
        let Self { events, search, open } = self;
        let needle = search.trim().to_lowercase();
        let rows: Vec<&ToolEvent> = events
            .iter()
            .filter(|e| {
                needle.is_empty()
                    || contains(&e.tool, &needle)
                    || e.error.as_deref().is_some_and(|err| contains(err, &needle))
            })
            .collect();
        let keys: Vec<&str> = rows.iter().map(|e| e.id.as_str()).collect();
        let mut expanded = positions(open, &keys);

        let columns = [
            Column::new("Zeit").width(130.0),
            Column::new("Tool").width(240.0),
            Column::new("Status").width(90.0),
            Column::new("Dauer").width(80.0).align_right(),
            Column::new("Größe").width(80.0).align_right(),
            Column::new("Fehler"),
        ];
        let actions = toolbar(paused);
        let description = if paused {
            format!("{} Aufrufe · angehalten, neue Aufrufe werden verworfen", events.len())
        } else {
            format!("{} Aufrufe · neueste oben", events.len())
        };
        let r = DataTable::new("tools", &columns, rows.len())
            .title("Tool-Aufrufe")
            .description(description)
            .size(TableSize::Small)
            .search(search)
            .search_placeholder("Tool oder Fehler suchen")
            .toolbar_actions(&actions)
            .expandable(&mut expanded, |ui, row| {
                let args = serde_json::to_string_pretty(&rows[row].args).unwrap_or_default();
                style::json_block(ui, &args);
            })
            .cell_ui(|ui, row, col| match col {
                1 => {
                    let e = rows[row];
                    ui.label(RichText::new(&e.tool).monospace().size(12.0).color(tool_color(&e.tool)));
                    true
                }
                2 => {
                    let ok = rows[row].status == "ok";
                    Tag::new(if ok { "ok" } else { "Fehler" })
                        .color(if ok { TagColor::Green } else { TagColor::Red })
                        .size(TagSize::Small)
                        .show(ui);
                    true
                }
                _ => false,
            })
            .empty_state("Warte auf Tool-Aufrufe …", "Jeder bench.*-Aufruf erscheint hier.")
            .height(ui.available_height())
            .show(ui, |row, col| {
                let e = rows[row];
                match col {
                    0 => style::clock(&e.ts),
                    1 => e.tool.clone(),
                    2 => e.status.clone(),
                    3 => duration(e.duration_ms),
                    4 => size(e.result_size),
                    _ => e.error.clone().unwrap_or_default(),
                }
            });
        if r.expansion_changed {
            *open = keys_of(&expanded, &keys);
        }
        feed_action(r.toolbar_action)
    }
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} kB", bytes as f64 / 1024.0)
    }
}

// Colour per tool group, from the Carbon palette (60er-Stufen, lesbar auf Weiß).
fn tool_color(tool: &str) -> Color32 {
    let op = tool.strip_prefix("bench.").unwrap_or(tool);
    let (group, name) = op.split_once('.').unwrap_or((op, ""));
    match group {
        "fs" if name.starts_with("read") || name == "stat" => Color32::from_rgb(0x0f, 0x62, 0xfe), // blue-60
        "fs" if matches!(name, "list" | "tree") => Color32::from_rgb(0x00, 0x72, 0xc3),          // cyan-60
        "fs" => Color32::from_rgb(0xba, 0x4e, 0x00),                                           // orange-60
        "search" => Color32::from_rgb(0x8a, 0x3f, 0xfc),                                       // purple-60
        "exec" => Color32::from_rgb(0xda, 0x1e, 0x28),                                         // red-60
        "git" | "patch" => Color32::from_rgb(0x19, 0x80, 0x38),                                // green-60
        "memory" | "handbook" | "context" => Color32::from_rgb(0x00, 0x7d, 0x79),              // teal-60
        "task" => Color32::from_rgb(0xd0, 0x26, 0x70),                                         // magenta-60
        _ => Color32::from_rgb(0x52, 0x52, 0x52),                                              // gray-70
    }
}

// ─── Logs ───────────────────────────────────────────────────────

#[derive(Default)]
pub struct LogsView {
    pub records: VecDeque<(u64, LogRecord)>,
    next_key: u64,
    search: String,
    open: Vec<String>,
}

impl LogsView {
    pub fn push(&mut self, record: LogRecord) {
        // Log records have no id of their own; a running number keeps the
        // expanded rows stable while new records arrive.
        self.next_key += 1;
        self.records.push_front((self.next_key, record));
        self.records.truncate(MAX_EVENTS);
    }

    pub fn clear(&mut self) {
        self.records.clear();
        self.open.clear();
    }

    pub fn has_errors(&self) -> bool {
        self.records.iter().any(|(_, r)| r.level == "error")
    }

    pub fn show(&mut self, ui: &mut Ui, paused: bool, otlp_port: u16) -> Option<FeedAction> {
        let Self { records, search, open, .. } = self;
        let needle = search.trim().to_lowercase();
        let rows: Vec<&(u64, LogRecord)> = records
            .iter()
            .filter(|(_, r)| {
                needle.is_empty()
                    || contains(&r.message, &needle)
                    || contains(&r.service, &needle)
                    || contains(&r.source, &needle)
            })
            .collect();
        let key_strings: Vec<String> = rows.iter().map(|(k, _)| k.to_string()).collect();
        let keys: Vec<&str> = key_strings.iter().map(String::as_str).collect();
        let mut expanded = positions(open, &keys);

        let columns = [
            Column::new("Zeit").width(130.0),
            Column::new("Level").width(90.0),
            Column::new("Dienst").width(120.0),
            Column::new("Quelle").width(180.0),
            Column::new("Meldung"),
        ];
        let actions = toolbar(paused);
        let description = if paused {
            format!("{} Einträge · angehalten, neue Einträge werden verworfen", records.len())
        } else {
            format!("{} Einträge · OTLP/HTTP auf 127.0.0.1:{otlp_port}", records.len())
        };
        let r = DataTable::new("logs", &columns, rows.len())
            .title("Logs")
            .description(description)
            .size(TableSize::Small)
            .search(search)
            .search_placeholder("Meldung, Dienst oder Quelle suchen")
            .toolbar_actions(&actions)
            .expandable(&mut expanded, |ui, row| {
                let record = &rows[row].1;
                ui.label(RichText::new(&record.message).monospace().size(12.0));
                if let Some(fields) = &record.fields {
                    ui.add_space(8.0);
                    let json = serde_json::to_string_pretty(fields).unwrap_or_default();
                    style::json_block(ui, &json);
                }
            })
            .cell_ui(|ui, row, col| {
                if col != 1 {
                    return false;
                }
                let level = rows[row].1.level.as_str();
                let color = match level {
                    "error" => TagColor::Red,
                    "warn" => TagColor::Magenta,
                    "info" => TagColor::Blue,
                    _ => TagColor::Gray,
                };
                let short: String = level.chars().take(3).collect::<String>().to_uppercase();
                Tag::new(short).color(color).size(TagSize::Small).show(ui);
                true
            })
            .empty_state(
                format!("Warte auf Logs über OTLP :{otlp_port} …"),
                "oosl schickt strukturierte Logs als OTLP/HTTP-JSON hierher.",
            )
            .height(ui.available_height())
            .show(ui, |row, col| {
                let record = &rows[row].1;
                match col {
                    0 => style::clock(&record.ts),
                    1 => record.level.clone(),
                    2 => record.service.clone(),
                    3 => record.source.clone(),
                    _ => record.message.clone(),
                }
            });
        if r.expansion_changed {
            *open = keys_of(&expanded, &keys);
        }
        feed_action(r.toolbar_action)
    }
}

//! Small drawing helpers shared by the views (Carbon tokens only).

use carbon_egui::theme::{color, PLEX_SEMIBOLD_FAMILY};
use egui::{FontFamily, FontId, RichText, Ui};

/// Page heading with a one-line description below, Carbon spacing after.
pub fn page_title(ui: &mut Ui, title: &str, description: &str) {
    ui.label(
        RichText::new(title)
            .font(FontId::new(20.0, FontFamily::Name(PLEX_SEMIBOLD_FAMILY.into())))
            .color(color::TEXT_PRIMARY),
    );
    ui.add_space(8.0);
    ui.label(RichText::new(description).size(14.0).color(color::TEXT_SECONDARY));
    ui.add_space(24.0);
}

/// Small grey text, e.g. for "Gespeichert um …" or status lines.
pub fn helper(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(color::TEXT_HELPER)
}

/// "HH:MM:SS.mmm" in local time from an RFC-3339 timestamp.
pub fn clock(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

/// JSON block (highlighted, monospace) on a grey layer, e.g. for tool args.
pub fn json_block(ui: &mut Ui, code: &str) {
    egui::Frame::NONE
        .fill(color::LAYER_01)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let width = ui.available_width();
                    let job = carbon_egui::highlight::job("json", code, FontId::monospace(12.0), Some(18.0), width);
                    ui.label(job);
                });
        });
}

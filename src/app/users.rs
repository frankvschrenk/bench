//! Users: admin for the dev IdP that runs inside the bench backend.
//
// Left the user list (Neu / Aktualisieren), right the selected user: email
// (= login name) and username, the group list, set password, delete. A status
// line shows whether the IdP is up and its issuer.
//
// Groups carry the authorization contract: oos derives a role by splitting
// each group name on "-" and ranking admin > manager > user, so the group
// input hints at the "oos-admin" shape.

use std::sync::Arc;

use carbon_egui::theme::color;
use carbon_egui::{Button, ButtonKind, ButtonSize, Form, Icon, Modal, ModalSize, StructuredList, Tag, TagSize, TextInput, Toast, Toasts};
use egui::{Key, RichText, Ui};
use tokio::runtime::Runtime;

use super::style;
use super::task::{self, Pending};
use crate::iam::{self, store::IamUser, IamInfo, IamState};

const GROUP_HINT: &str = "Die Rolle ergibt sich aus dem Gruppennamen, geteilt an „-“: admin > manager > user, z. B. oos-admin.";

enum Reply {
    Info(IamInfo),
    Users(Result<Vec<IamUser>, String>),
    Created(Result<IamUser, String>),
    Done(&'static str, Result<(), String>),
}

pub struct UsersView {
    iam: Arc<IamState>,
    pending: Vec<Pending<Reply>>,
    loaded: bool,
    loading: bool,
    info: Option<IamInfo>,
    users: Vec<IamUser>,
    selected: Option<usize>,
    selected_id: Option<i32>,

    // Editor for the selected user
    email: String,
    username: String,
    groups: Vec<String>,
    group_input: String,
    password: String,

    // New-user dialog
    new_open: bool,
    n_email: String,
    n_username: String,
    n_password: String,
    n_groups: Vec<String>,
    n_group_input: String,

    delete_open: bool,
}

impl UsersView {
    pub fn new(iam: Arc<IamState>) -> Self {
        Self {
            iam,
            pending: Vec::new(),
            loaded: false,
            loading: false,
            info: None,
            users: Vec::new(),
            selected: None,
            selected_id: None,
            email: String::new(),
            username: String::new(),
            groups: Vec::new(),
            group_input: String::new(),
            password: String::new(),
            new_open: false,
            n_email: String::new(),
            n_username: String::new(),
            n_password: String::new(),
            n_groups: vec!["oos-user".into()],
            n_group_input: String::new(),
            delete_open: false,
        }
    }

    fn run(&mut self, rt: &Runtime, ctx: &egui::Context, f: impl FnOnce(Arc<IamState>) -> task::BoxFuture<Reply>) {
        let fut = f(self.iam.clone());
        self.pending.push(task::spawn(rt, ctx, fut));
    }

    fn reload(&mut self, rt: &Runtime, ctx: &egui::Context) {
        self.loading = true;
        self.run(rt, ctx, |s| Box::pin(async move { Reply::Info(iam::iam_info(&s).await) }));
        self.run(rt, ctx, |s| Box::pin(async move { Reply::Users(iam::iam_list(&s).await) }));
    }

    fn select(&mut self, index: Option<usize>) {
        self.selected = index;
        let user = index.and_then(|i| self.users.get(i)).cloned();
        self.selected_id = user.as_ref().map(|u| u.id);
        if let Some(u) = user {
            self.email = u.email;
            self.username = u.username;
            self.groups = u.groups;
        }
        self.group_input.clear();
        self.password.clear();
    }

    fn handle_replies(&mut self, rt: &Runtime, ctx: &egui::Context, toasts: &mut Toasts) {
        for reply in task::drain(&mut self.pending) {
            match reply {
                Reply::Info(info) => self.info = Some(info),
                Reply::Users(Ok(users)) => {
                    self.loading = false;
                    self.users = users;
                    // Keep the selection on the same user across reloads.
                    let index = self.selected_id.and_then(|id| self.users.iter().position(|u| u.id == id));
                    self.select(index);
                }
                Reply::Users(Err(e)) => {
                    self.loading = false;
                    self.users.clear();
                    self.select(None);
                    // A stopped IdP already shows its reason in the status line.
                    if self.info.as_ref().is_some_and(|i| i.running) {
                        toasts.add(Toast::error("Benutzer nicht geladen").subtitle(e));
                    }
                }
                Reply::Created(Ok(user)) => {
                    toasts.add(Toast::success("Benutzer angelegt").subtitle(user.email.clone()));
                    self.selected_id = Some(user.id);
                    self.new_open = false;
                    self.n_email.clear();
                    self.n_username.clear();
                    self.n_password.clear();
                    self.n_groups = vec!["oos-user".into()];
                    self.reload(rt, ctx);
                }
                Reply::Created(Err(e)) => toasts.add(Toast::error("Anlegen fehlgeschlagen").subtitle(e)),
                Reply::Done(what, Ok(())) => {
                    toasts.add(Toast::success(what));
                    self.reload(rt, ctx);
                }
                Reply::Done(what, Err(e)) => toasts.add(Toast::error(format!("{what}: fehlgeschlagen")).subtitle(e)),
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, rt: &Runtime, toasts: &mut Toasts) {
        let ctx = ui.ctx().clone();
        if !self.loaded {
            self.loaded = true;
            self.reload(rt, &ctx);
        }
        self.handle_replies(rt, &ctx, toasts);

        egui::Panel::left("users-list")
            .exact_size(320.0)
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 0, right: 24, top: 0, bottom: 0 }))
            .show(ui, |ui| self.list(ui, rt, &ctx));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.detail(ui, rt, &ctx));
            });

        self.dialogs(rt, &ctx);
    }

    fn list(&mut self, ui: &mut Ui, rt: &Runtime, ctx: &egui::Context) {
        style::page_title(ui, "Benutzer", "Dev-IdP der Bench (OAuth2/PKCE, Benutzer in Postgres).");
        ui.horizontal(|ui| {
            if ui.add(Button::new("Neu").icon(Icon::Add).size(ButtonSize::Small).compact(true)).clicked() {
                self.new_open = true;
            }
            ui.add_space(8.0);
            let label = if self.loading { "Lädt …" } else { "Aktualisieren" };
            if ui
                .add(Button::new(label).kind(ButtonKind::Ghost).size(ButtonSize::Small).compact(true).disabled(self.loading))
                .clicked()
            {
                self.reload(rt, ctx);
            }
        });
        ui.add_space(16.0);
        if let Some(info) = &self.info {
            let text = if info.running { format!("IdP {}", info.issuer) } else { format!("IdP {}", info.status) };
            let color = if info.running { color::TEXT_HELPER } else { color::TEXT_ERROR };
            ui.label(RichText::new(text).size(12.0).color(color)).on_hover_text(&info.status);
            ui.add_space(16.0);
        }
        if self.users.is_empty() {
            ui.label(style::helper(if self.loading { "Lädt …" } else { "Noch keine Benutzer." }));
            return;
        }
        let rows: Vec<[String; 2]> = self.users.iter().map(|u| [u.username.clone(), u.email.clone()]).collect();
        let mut selected = self.selected;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            StructuredList::new("users", &["Name", "E-Mail"], &rows)
                .label("Benutzer")
                .selectable(&mut selected)
                .condensed(true)
                .show(ui);
        });
        if selected != self.selected {
            self.select(selected);
        }
    }

    fn detail(&mut self, ui: &mut Ui, rt: &Runtime, ctx: &egui::Context) {
        let Some(user) = self.selected.and_then(|i| self.users.get(i)).cloned() else {
            ui.add_space(8.0);
            ui.label(style::helper("Links einen Benutzer wählen."));
            return;
        };
        let id = user.id;
        let identity_changed = self.email.trim() != user.email || self.username.trim() != user.username;
        let groups_changed = self.groups != user.groups;

        Form::new().width(520.0).show(ui, |form| {
            form.full(|ui| style::page_title(ui, &user.username, &format!("Benutzer-Nr. {id}")));
            form.full(|ui| {
                ui.add(
                    TextInput::new(&mut self.email)
                        .label("E-Mail")
                        .helper_text("Anmeldename – Apps wie ephemerline nutzen ihn als Benutzerschlüssel"),
                )
            });
            form.full(|ui| ui.add(TextInput::new(&mut self.username).label("Benutzername").helper_text("Anzeigename im Token")));
            form.full(|ui| {
                let ok = ui
                    .add(
                        Button::new("Speichern")
                            .size(ButtonSize::Medium)
                            .disabled(!identity_changed || self.email.trim().is_empty()),
                    )
                    .clicked();
                if ok {
                    let (email, username) = (self.email.trim().to_string(), self.username.trim().to_string());
                    self.run(rt, ctx, move |s| {
                        Box::pin(async move { Reply::Done("Gespeichert", iam::iam_update(&s, id, email, username).await.map(|_| ())) })
                    });
                }
            });

            form.section("Gruppen");
            form.full(|ui| group_editor(ui, "groups", &mut self.groups, &mut self.group_input, false));
            form.full(|ui| {
                if ui.add(Button::new("Gruppen speichern").size(ButtonSize::Medium).disabled(!groups_changed)).clicked() {
                    let groups = self.groups.clone();
                    self.run(rt, ctx, move |s| {
                        Box::pin(async move { Reply::Done("Gruppen gespeichert", iam::iam_set_groups(&s, id, groups).await) })
                    });
                }
            });

            form.section("Passwort setzen");
            form.full(|ui| ui.add(TextInput::new(&mut self.password).label("Neues Passwort").password(true)));
            form.full(|ui| {
                let set = ui
                    .add(
                        Button::new("Passwort setzen")
                            .kind(ButtonKind::Secondary)
                            .size(ButtonSize::Medium)
                            .disabled(self.password.is_empty()),
                    )
                    .clicked();
                if set {
                    let password = std::mem::take(&mut self.password);
                    self.run(rt, ctx, move |s| {
                        Box::pin(async move { Reply::Done("Passwort geändert", iam::iam_set_password(&s, id, password).await) })
                    });
                }
            });

            form.section("Gefahrenbereich");
            form.full(|ui| {
                if ui
                    .add(Button::new("Benutzer löschen").kind(ButtonKind::DangerTertiary).icon(Icon::TrashCan).size(ButtonSize::Medium))
                    .clicked()
                {
                    self.delete_open = true;
                }
            });
        });
    }

    fn dialogs(&mut self, rt: &Runtime, ctx: &egui::Context) {
        let can_create = !self.n_email.trim().is_empty() && !self.n_password.is_empty();
        let mut create = false;
        let r = Modal::new("new-user", &mut self.new_open)
            .title("Neuer Benutzer")
            .size(ModalSize::Small)
            .primary("Anlegen")
            .secondary("Abbrechen")
            .primary_disabled(!can_create)
            .close_on_backdrop(false)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 16.0;
                ui.add(TextInput::new(&mut self.n_email).label("E-Mail").placeholder("user@example.com").on_layer(true));
                ui.add(TextInput::new(&mut self.n_username).label("Benutzername").placeholder("leer = E-Mail").on_layer(true));
                ui.add(TextInput::new(&mut self.n_password).label("Passwort").password(true).on_layer(true));
                ui.label(RichText::new("Gruppen").size(12.0).color(color::TEXT_SECONDARY));
                group_editor(ui, "new-groups", &mut self.n_groups, &mut self.n_group_input, true);
            });
        if r.is_some_and(|r| r.primary) && can_create {
            create = true;
        }
        if create {
            let email = self.n_email.trim().to_string();
            let username = match self.n_username.trim() {
                "" => email.clone(),
                name => name.to_string(),
            };
            let (password, groups) = (self.n_password.clone(), self.n_groups.clone());
            self.run(rt, ctx, move |s| {
                Box::pin(async move { Reply::Created(iam::iam_create(&s, email, username, password, groups).await) })
            });
        }

        let target = self.selected.and_then(|i| self.users.get(i)).cloned();
        let label = target.as_ref().map(|u| u.email.clone()).unwrap_or_default();
        let r = Modal::new("delete-user", &mut self.delete_open)
            .title("Benutzer löschen?")
            .label(label.clone())
            .size(ModalSize::ExtraSmall)
            .primary("Löschen")
            .secondary("Abbrechen")
            .danger(true)
            .show(ctx, |ui| {
                ui.label(format!("„{label}“ wird gelöscht. Das lässt sich nicht rückgängig machen."));
            });
        if r.is_some_and(|r| r.primary) {
            self.delete_open = false;
            if let Some(u) = target {
                self.selected_id = None;
                self.run(rt, ctx, move |s| Box::pin(async move { Reply::Done("Benutzer gelöscht", iam::iam_delete(&s, u.id).await) }));
            }
        }
    }
}

// Group list as dismissible tags plus an input; Enter or the button adds.
fn group_editor(ui: &mut Ui, id: &str, groups: &mut Vec<String>, input: &mut String, on_layer: bool) {
    if !groups.is_empty() {
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            for (i, g) in groups.iter().enumerate() {
                if Tag::new(g).size(TagSize::Medium).dismissible(true).show(ui).closed {
                    remove = Some(i);
                }
            }
        });
        if let Some(i) = remove {
            groups.remove(i);
        }
        ui.add_space(16.0);
    }
    let mut add = false;
    ui.horizontal_top(|ui| {
        let r = ui.add(
            TextInput::new(input)
                .label("Gruppe hinzufügen")
                .placeholder("oos-admin")
                .helper_text(GROUP_HINT)
                .width(320.0)
                .on_layer(on_layer)
                .id_salt((id, "input")),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            add = true;
        }
        ui.add_space(8.0);
        ui.vertical(|ui| {
            ui.add_space(24.0);
            if ui
                .add(Button::new("Hinzufügen").kind(ButtonKind::Tertiary).size(ButtonSize::Medium).disabled(input.trim().is_empty()))
                .clicked()
            {
                add = true;
            }
        });
    });
    let name = input.trim().to_string();
    if add && !name.is_empty() {
        if !groups.contains(&name) {
            groups.push(name);
        }
        input.clear();
    }
}

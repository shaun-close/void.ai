//! The window: a fixed menu bar and status bar around a dock that holds every panel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui_dock::{DockArea, DockState, Style};
use void_core::config::Theme;
use void_core::context::context_used;
use void_core::engine::{Command, Engine, EngineHandle};
use void_core::model::ConversationId;
use void_core::store::Store;

use crate::layout::{self, Tab};
use crate::panels::{Action, Panels};
use crate::state::UiState;

pub struct VoidApp {
    engine: EngineHandle,
    store: Store,
    state: UiState,
    dock: DockState<Tab>,
    inputs: HashMap<u32, String>,
    /// The Chat tab that last had focus; conversations picked in the list open there.
    focused_chat: Option<u32>,
    saved_layout: String,
    layout_checked: Instant,
    folders: (mpsc::Sender<PathBuf>, mpsc::Receiver<PathBuf>),
    applied_style: Option<(Theme, u32)>,
    title: String,
}

impl VoidApp {
    pub fn new(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>) -> Self {
        let (config_path, data_dir) = Store::default_paths();
        let store = Store::new(data_dir);
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(
            config_path,
            store.clone(),
            Arc::new(move || ctx.request_repaint()),
        );
        if let Some(p) = open {
            engine.send(Command::OpenWorkspace(Some(p)));
        }
        let dock = layout::default_layout();
        Self {
            saved_layout: layout::to_text(&dock),
            engine,
            store,
            state: UiState::default(),
            dock,
            inputs: HashMap::new(),
            focused_chat: None,
            layout_checked: Instant::now(),
            folders: mpsc::channel(),
            applied_style: None,
            title: String::new(),
        }
    }

    fn pump(&mut self) {
        while let Some(ev) = self.engine.try_recv() {
            self.state.apply(ev);
        }
        if let Some(layout) = self.state.take_layout() {
            self.dock = layout
                .as_deref()
                .and_then(layout::from_text)
                .unwrap_or_else(layout::default_layout);
            self.saved_layout = layout::to_text(&self.dock);
            self.focused_chat = None;
        }
        if let Some(id) = self.state.take_created() {
            self.apply_action(Action::Show(id));
        }
        while let Ok(folder) = self.folders.1.try_recv() {
            self.open_workspace(Some(folder));
        }
    }

    fn open_workspace(&mut self, folder: Option<PathBuf>) {
        self.save_layout_now();
        self.engine.send(Command::OpenWorkspace(folder));
    }

    fn save_layout_now(&mut self) {
        let json = layout::to_text(&self.dock);
        if json != self.saved_layout {
            if let Err(e) = self.store.save_layout(&self.state.workspace.id, &json) {
                self.state.error(format!("Couldn't save the layout: {e}"));
            }
            self.saved_layout = json;
        }
    }

    fn maybe_save_layout(&mut self) {
        if self.layout_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.layout_checked = Instant::now();
        let json = layout::to_text(&self.dock);
        if json != self.saved_layout {
            self.engine
                .save_layout(&self.state.workspace.id, json.clone());
            self.saved_layout = json;
        }
    }

    fn chat_tab_mut(&mut self, chat_id: u32) -> Option<&mut Option<ConversationId>> {
        self.dock.iter_all_tabs_mut().find_map(|(_, t)| match t {
            Tab::Chat { id, conversation } if *id == chat_id => Some(conversation),
            _ => None,
        })
    }

    fn apply_action(&mut self, action: Action) {
        let (conv, new_tab) = match action {
            Action::Show(c) => (c, false),
            Action::ShowInNewTab(c) => (c, true),
        };
        if !self.state.loaded.contains_key(&conv) {
            self.engine.send(Command::OpenConversation(conv));
        }
        // Already showing somewhere? Focus that tab.
        if let Some(path) = self
            .dock
            .find_tab_from(|t| matches!(t, Tab::Chat { conversation: Some(c), .. } if *c == conv))
            && !new_tab
        {
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(path.node_path());
            return;
        }
        let target = if new_tab {
            None
        } else {
            self.focused_chat.or_else(|| self.any_chat())
        };
        match target.and_then(|id| self.chat_tab_mut(id).map(|slot| (id, slot))) {
            Some((id, slot)) => {
                *slot = Some(conv);
                let path = self
                    .dock
                    .find_tab_from(|t| matches!(t, Tab::Chat { id: i, .. } if *i == id));
                if let Some(path) = path {
                    let _ = self.dock.set_active_tab(path);
                }
            }
            None => {
                let id = layout::next_chat_id(&self.dock);
                layout::ensure_tab(
                    &mut self.dock,
                    Tab::Chat {
                        id,
                        conversation: Some(conv),
                    },
                );
                self.focused_chat = Some(id);
            }
        }
    }

    fn any_chat(&self) -> Option<u32> {
        self.dock.iter_all_tabs().find_map(|(_, t)| match t {
            Tab::Chat { id, .. } => Some(*id),
            _ => None,
        })
    }

    fn focused_conversation(&self) -> Option<ConversationId> {
        let id = self.focused_chat?;
        self.dock.iter_all_tabs().find_map(|(_, t)| match t {
            Tab::Chat {
                id: i,
                conversation,
            } if *i == id => *conversation,
            _ => None,
        })
    }

    fn apply_style(&mut self, ctx: &egui::Context) {
        let want = (
            self.state.config.theme,
            (self.state.config.font_size * 10.0) as u32,
        );
        if self.applied_style == Some(want) {
            return;
        }
        self.applied_style = Some(want);
        ctx.set_visuals(match self.state.config.theme {
            Theme::Dark => egui::Visuals::dark(),
            Theme::Light => egui::Visuals::light(),
        });
        let size = self.state.config.font_size.clamp(10.0, 24.0);
        ctx.global_style_mut(|style| {
            use egui::{FontFamily, FontId, TextStyle};
            style.text_styles = [
                (
                    TextStyle::Small,
                    FontId::new(size - 3.0, FontFamily::Proportional),
                ),
                (TextStyle::Body, FontId::new(size, FontFamily::Proportional)),
                (
                    TextStyle::Button,
                    FontId::new(size, FontFamily::Proportional),
                ),
                (
                    TextStyle::Heading,
                    FontId::new(size + 5.0, FontFamily::Proportional),
                ),
                (
                    TextStyle::Monospace,
                    FontId::new(size - 1.0, FontFamily::Monospace),
                ),
            ]
            .into();
        });
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open Folder…").clicked() {
                    let tx = self.folders.0.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            let _ = tx.send(p);
                            ctx.request_repaint();
                        }
                    });
                }
                ui.menu_button("Recent", |ui| {
                    let recent: Vec<_> = self
                        .state
                        .recent
                        .iter()
                        .filter_map(|e| {
                            e.workspace
                                .root
                                .clone()
                                .map(|r| (e.workspace.name.clone(), r))
                        })
                        .collect();
                    if recent.is_empty() {
                        ui.label("No folders yet");
                    }
                    for (name, root) in recent {
                        if ui
                            .button(name)
                            .on_hover_text(root.display().to_string())
                            .clicked()
                        {
                            self.open_workspace(Some(root));
                        }
                    }
                });
                if ui.button("Scratch").clicked() {
                    self.open_workspace(None);
                }
                ui.separator();
                if ui.button("New chat").clicked() {
                    self.engine.send(Command::NewConversation);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("New chat tab").clicked() {
                    let id = layout::next_chat_id(&self.dock);
                    layout::ensure_tab(
                        &mut self.dock,
                        Tab::Chat {
                            id,
                            conversation: None,
                        },
                    );
                    self.focused_chat = Some(id);
                }
                for (label, tab) in [
                    ("Conversations", Tab::Conversations),
                    ("Backends", Tab::Backends),
                    ("Settings", Tab::Settings),
                ] {
                    if ui.button(label).clicked() {
                        layout::ensure_tab(&mut self.dock, tab);
                    }
                }
                ui.separator();
                if ui.button("Reset layout").clicked() {
                    self.dock = layout::default_layout();
                    self.focused_chat = None;
                }
            });
            ui.menu_button("Backend", |ui| {
                let mut active = self.state.config.active_profile.clone();
                for p in &self.state.config.profiles {
                    ui.radio_value(&mut active, p.id.clone(), &p.name);
                }
                if active != self.state.config.active_profile {
                    self.state.config.active_profile = active;
                    self.engine
                        .send(Command::SaveConfig(self.state.config.clone()));
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let conv = self.focused_conversation();
        ui.horizontal(|ui| {
            ui.label(&self.state.workspace.name);
            ui.separator();
            match self.state.profile_for(conv) {
                Some(p) => ui.label(format!("{} · {}", p.name, p.model)),
                None => ui.label("no profile"),
            };
            ui.separator();
            let streaming = conv.is_some_and(|c| self.state.streams.contains_key(&c));
            if streaming {
                ui.spinner();
                ui.label("streaming…");
            } else {
                ui.label("idle");
            }
            if let Some(tps) = self.state.last_tps {
                ui.separator();
                ui.label(format!("{tps:.1} tok/s"));
            }
            if let (Some(c), Some(p)) = (
                conv.and_then(|c| self.state.loaded.get(&c)),
                self.state.profile_for(conv),
            ) {
                ui.separator();
                let used = context_used(c);
                ui.label(format!(
                    "context {} / {}",
                    fmt_k(used),
                    fmt_k(p.context_limit)
                ))
                .on_hover_text(format!("{used} of {} tokens", p.context_limit));
            }
            if let Some(st) = &self.state.status {
                ui.separator();
                let color = if st.error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().weak_text_color()
                };
                ui.colored_label(color, &st.text);
            }
        });
    }
}

fn fmt_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

impl eframe::App for VoidApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        let ctx = ui.ctx().clone();
        self.apply_style(&ctx);
        let title = format!("void.ai — {}", self.state.workspace.name);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::Panel::top("menu_bar").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));

        let mut actions = Vec::new();
        let mut panels = Panels {
            state: &mut self.state,
            engine: &self.engine,
            inputs: &mut self.inputs,
            actions: &mut actions,
        };
        DockArea::new(&mut self.dock)
            .style(Style::from_egui(ui.style().as_ref()))
            .show_leaf_close_all_buttons(false)
            .show_inside(ui, &mut panels);

        if let Some((_, Tab::Chat { id, .. })) = self.dock.find_active_focused() {
            self.focused_chat = Some(*id);
        }
        for a in actions {
            self.apply_action(a);
        }
        self.maybe_save_layout();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_layout_now();
    }
}

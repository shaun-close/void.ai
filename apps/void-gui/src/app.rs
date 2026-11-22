//! The window: a fixed menu bar and status bar around a dock that holds every panel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui_dock::{DockArea, DockState};
use void_core::config::Theme;
use void_core::context::context_used;
use void_core::engine::{Command, Engine, EngineHandle};
use void_core::model::ConversationId;
use void_core::store::Store;

use crate::layout::{self, Tab};
use crate::panels::{Action, Panels};
use crate::state::UiState;
use crate::theme::{self, icon};
use egui::RichText;

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
    list: crate::panels::conversations::ListUi,
    backends: crate::panels::backends::BackendsUi,
    md: egui_commonmark::CommonMarkCache,
    /// A first message waiting for the conversation it asked the engine to create.
    pending_start: Option<(u32, String)>,
}

impl VoidApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        open: Option<PathBuf>,
        config_path: PathBuf,
        data_dir: PathBuf,
    ) -> Self {
        theme::install_fonts(&cc.egui_ctx);
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
            list: Default::default(),
            backends: Default::default(),
            md: Default::default(),
            pending_start: None,
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
            self.focused_chat = None;
            // Restored Chat tabs: load their conversations, forget ones that are gone.
            let known: Vec<ConversationId> =
                self.state.conversations.iter().map(|m| m.id).collect();
            let mut load = Vec::new();
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                if let Tab::Chat { conversation, .. } = tab
                    && let Some(c) = *conversation
                {
                    if known.contains(&c) {
                        load.push(c);
                    } else {
                        *conversation = None;
                    }
                }
            }
            for c in load {
                if !self.state.loaded.contains_key(&c) {
                    self.engine.send(Command::OpenConversation(c));
                }
            }
            self.saved_layout = layout::to_text(&self.dock);
        }
        for gone in self.state.take_deleted() {
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                if let Tab::Chat { conversation, .. } = tab
                    && *conversation == Some(gone)
                {
                    *conversation = None;
                }
            }
        }
        if let Some(id) = self.state.take_created() {
            match self.pending_start.take() {
                Some((tab, text)) => {
                    match self.chat_tab_mut(tab) {
                        Some(slot) => {
                            *slot = Some(id);
                            self.focused_chat = Some(tab);
                        }
                        None => self.apply_action(Action::Show(id)),
                    }
                    self.engine.send(Command::SendMessage {
                        conversation: id,
                        text,
                    });
                }
                None => self.apply_action(Action::Show(id)),
            }
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
            Action::StartChat { tab, text } => {
                self.pending_start = Some((tab, text));
                self.engine.send(Command::NewConversation);
                return;
            }
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
        let id = self.focused_chat.or_else(|| self.any_chat())?;
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
        let size = crate::panels::settings::clamp_font(self.state.config.font_size);
        theme::apply(ctx, self.state.config.theme, size);
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.add_space(2.0);
            theme::wordmark(ui, 17.0);
            ui.add_space(12.0);
            ui.menu_button("File", |ui| {
                if ui
                    .button(theme::itext(icon::FOLDER_OPEN, "Open Folder…"))
                    .clicked()
                {
                    let tx = self.folders.0.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            let _ = tx.send(p);
                            ctx.request_repaint();
                        }
                    });
                }
                ui.menu_button(theme::itext(icon::FOLDER_OPEN, "Recent"), |ui| {
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
                if ui.button(theme::itext(icon::PLANET, "Scratch")).clicked() {
                    self.open_workspace(None);
                }
                ui.separator();
                if ui.button(theme::itext(icon::PLUS, "New chat")).clicked() {
                    self.engine.send(Command::NewConversation);
                }
                ui.separator();
                if ui.button(theme::itext(icon::SIGN_OUT, "Quit")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("View", |ui| {
                if ui
                    .button(theme::itext(icon::CHAT, "New chat tab"))
                    .clicked()
                {
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
                    (theme::itext(icon::CHATS, "Chats"), Tab::Conversations),
                    (theme::itext(icon::PLUGS, "Backends"), Tab::Backends),
                    (theme::itext(icon::GEAR, "Settings"), Tab::Settings),
                ] {
                    if ui.button(label).clicked() {
                        layout::ensure_tab(&mut self.dock, tab);
                    }
                }
                ui.separator();
                if ui
                    .button(theme::itext(icon::LAYOUT, "Reset layout"))
                    .clicked()
                {
                    self.dock = layout::default_layout();
                    self.focused_chat = None;
                }
            });
            ui.menu_button("Backend", |ui| {
                let mut active = self.state.saved_config.active_profile.clone();
                for p in &self.state.saved_config.profiles {
                    ui.radio_value(&mut active, p.id.clone(), &p.name);
                }
                if active != self.state.saved_config.active_profile {
                    crate::panels::save_change(&mut self.state, &self.engine, |c| {
                        c.active_profile = active.clone()
                    });
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let pal = theme::palette_of(ui);
        let conv = self.focused_conversation();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let ws_icon = if self.state.workspace.root.is_some() {
                icon::FOLDER_OPEN
            } else {
                icon::PLANET
            };
            theme::pill(ui, ws_icon, self.state.workspace.name.clone(), pal.accent).on_hover_text(
                self.state
                    .workspace
                    .root
                    .as_ref()
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|| "Scratch: chats not tied to a folder".into()),
            );
            match self.state.profile_for(conv) {
                Some(p) => {
                    let color = match p.kind {
                        void_core::config::BackendKind::OpenAi => pal.accent2,
                        void_core::config::BackendKind::Anthropic => pal.accent3,
                    };
                    theme::pill(ui, icon::PLUGS, format!("{} · {}", p.name, p.model), color)
                        .on_hover_text(&p.base_url);
                }
                None => {
                    theme::pill(ui, icon::WARNING, "no profile", pal.err);
                }
            };
            let streaming = conv.is_some_and(|c| self.state.streams.contains_key(&c));
            if streaming {
                let t = ((ui.input(|i| i.time) * 3.0).sin() as f32 + 1.0) / 2.0;
                theme::pill(
                    ui,
                    icon::CIRCLE,
                    "streaming",
                    theme::lerp(pal.accent, pal.accent2, t),
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            } else {
                theme::pill(ui, icon::CIRCLE, "idle", pal.text_faint);
            }
            if let Some(tps) = self.state.last_tps {
                theme::pill(ui, icon::LIGHTNING, format!("{tps:.1} tok/s"), pal.warn);
            }
            if let (Some(c), Some(p)) = (
                conv.and_then(|c| self.state.loaded.get(&c)),
                self.state.profile_for(conv),
            ) {
                let used = context_used(c);
                let frac = theme::context_fraction(used, p.context_limit);
                let color = theme::meter_color(&pal, frac);
                ui.add_space(4.0);
                ui.label(theme::ico(icon::GAUGE).color(color));
                theme::meter(ui, 90.0, frac, color);
                ui.label(
                    RichText::new(format!(
                        "context {} / {}",
                        fmt_k(used),
                        fmt_k(p.context_limit)
                    ))
                    .small()
                    .color(pal.text_dim),
                )
                .on_hover_text(format!(
                    "{used} of {} tokens ({:.1}%)",
                    p.context_limit,
                    frac * 100.0
                ));
            }
            if let Some(st) = &self.state.status {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (ic, color) = if st.error {
                        (icon::WARNING, pal.err)
                    } else {
                        (icon::CHECK, pal.ok)
                    };
                    ui.label(theme::itext_styled(
                        ic,
                        &st.text,
                        theme::small_size(),
                        color,
                    ));
                });
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

        let pal = theme::palette(self.state.config.theme);
        let bar = egui::Frame::new()
            .fill(pal.bg_deep)
            .inner_margin(egui::Margin::symmetric(8, 5));
        egui::Panel::top("menu_bar")
            .frame(bar)
            .show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status_bar")
            .frame(bar)
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(pal.bg_deep))
            .show(ui, |ui| self.dock_area(ui));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_layout_now();
    }
}

impl VoidApp {
    fn dock_area(&mut self, ui: &mut egui::Ui) {
        let pal = theme::palette(self.state.config.theme);

        let mut actions = Vec::new();
        let focused = self.focused_conversation();
        let mut panels = Panels {
            state: &mut self.state,
            engine: &self.engine,
            inputs: &mut self.inputs,
            actions: &mut actions,
            list: &mut self.list,
            backends: &mut self.backends,
            md: &mut self.md,
            focused,
        };
        DockArea::new(&mut self.dock)
            .style(theme::dock_style(ui.style().as_ref(), &pal))
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
}

/// End-to-end checks of the real app against a running Strata (`strata launch --preset daily`),
/// driven headlessly by egui_kittest. Run with:
/// `cargo test -p void-gui e2e -- --ignored --nocapture`
/// Set VOID_AI_E2E_DIR to keep the config and data it writes (otherwise a temp dir is used).
#[cfg(test)]
mod e2e {
    use super::*;
    use egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use void_core::model::Role as MsgRole;

    struct Paths {
        _tmp: Option<tempfile::TempDir>,
        root: PathBuf,
    }

    fn paths() -> Paths {
        match std::env::var_os("VOID_AI_E2E_DIR") {
            Some(d) => {
                let root = PathBuf::from(d);
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(&root).unwrap();
                Paths { _tmp: None, root }
            }
            None => {
                let t = tempfile::tempdir().unwrap();
                Paths {
                    root: t.path().to_path_buf(),
                    _tmp: Some(t),
                }
            }
        }
    }

    fn harness(p: &Paths) -> Harness<'static, VoidApp> {
        let (cfg, data) = (p.root.join("config.toml"), p.root.join("data"));
        Harness::builder()
            .with_size([1280.0, 800.0])
            .with_max_steps(10)
            // egui_dock's splitters have no accessible name; that's upstream, not ours.
            .with_accessibility_check(false)
            .build_eframe(move |cc| VoidApp::new(cc, None, cfg, data))
    }

    /// Step the app (letting engine events arrive) until `done` holds.
    fn until(h: &mut Harness<'_, VoidApp>, secs: u64, what: &str, done: impl Fn(&VoidApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            h.step();
            if done(h.state()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_and_send(h: &mut Harness<'_, VoidApp>, text: &str) {
        let input = h.get_by_role(Role::MultilineTextInput);
        input.focus();
        h.step();
        h.get_by_role(Role::MultilineTextInput).type_text(text);
        h.step();
        h.key_press(egui::Key::Enter);
        h.step();
    }

    fn last_assistant(app: &VoidApp) -> Option<void_core::model::Message> {
        let c = app.focused_conversation()?;
        app.state
            .loaded
            .get(&c)?
            .messages
            .last()
            .filter(|m| m.role == MsgRole::Assistant)
            .cloned()
    }

    fn idle(app: &VoidApp) -> bool {
        app.state.streams.is_empty()
    }

    /// Offline: a first message typed into the Chat tab makes that tab's conversation the
    /// focused one (status bar, list highlight) without a click on the tab.
    #[test]
    fn first_message_focuses_its_chat_tab() {
        let p = paths_tmp();
        let cfg = void_core::config::Config {
            active_profile: "llama-server".into(),
            profiles: vec![void_core::config::Profile {
                base_url: "http://127.0.0.1:9/v1".into(),
                ..void_core::config::Config::default()
                    .profile("llama-server")
                    .unwrap()
                    .clone()
            }],
            ..void_core::config::Config::default()
        };
        void_core::config::save_config(&p.root.join("config.toml"), &cfg).unwrap();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.config.active_profile == "llama-server"
        });
        type_and_send(&mut h, "hello");
        until(&mut h, 10, "conversation", |a| {
            a.state.conversations.len() == 1
        });
        until(&mut h, 10, "focused conversation", |a| {
            a.focused_conversation().is_some()
        });
        let id = h.state().state.conversations[0].id;
        assert_eq!(h.state().focused_conversation(), Some(id));
    }

    /// Offline: after a restart, a Chat tab restored from the layout loads its conversation,
    /// and a tab whose conversation is gone falls back to an empty chat.
    #[test]
    fn restored_chat_tab_loads_its_conversation() {
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        h.state().engine.send(Command::NewConversation);
        until(&mut h, 10, "bound", |a| a.focused_conversation().is_some());
        let id = h.state().focused_conversation().unwrap();
        // A second Chat tab pointing at a conversation that doesn't exist.
        let ghost = ConversationId::new();
        let tab = layout::next_chat_id(&h.state().dock);
        layout::ensure_tab(
            &mut h.state_mut().dock,
            Tab::Chat {
                id: tab,
                conversation: Some(ghost),
            },
        );
        h.step();
        h.state_mut().save_layout_now();
        drop(h);

        let mut h = harness(&p);
        until(&mut h, 10, "restored conversation loaded", |a| {
            a.state.loaded.contains_key(&id)
        });
        let ghost_tab = h.state().dock.iter_all_tabs().find_map(|(_, t)| match t {
            Tab::Chat { id, conversation } if *id == tab => Some(*conversation),
            _ => None,
        });
        assert_eq!(
            ghost_tab,
            Some(None),
            "missing conversation cleared from its tab"
        );
    }

    /// Offline: deleting the conversation a Chat tab shows empties that tab.
    #[test]
    fn deleting_a_shown_conversation_clears_its_tab() {
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        h.state().engine.send(Command::NewConversation);
        until(&mut h, 10, "bound", |a| a.focused_conversation().is_some());
        let id = h.state().focused_conversation().unwrap();
        h.state().engine.send(Command::DeleteConversation(id));
        until(&mut h, 10, "tab cleared", |a| {
            a.focused_conversation().is_none() && !a.state.loaded.contains_key(&id)
        });
    }

    /// Offline: render the app with a seeded conversation to PNGs, for eyeballing the look.
    /// `VOID_AI_SHOT_DIR=/some/dir cargo test -p void-gui render_screenshots -- --ignored`
    #[test]
    #[ignore]
    fn render_screenshots() {
        use void_core::model::{Conversation, Message, Part, Role as R, Usage};
        let Some(out) = std::env::var_os("VOID_AI_SHOT_DIR").map(PathBuf::from) else {
            eprintln!("set VOID_AI_SHOT_DIR");
            return;
        };
        std::fs::create_dir_all(&out).unwrap();
        let p = paths_tmp();
        let store = Store::new(p.root.join("data"));
        let ws = void_core::model::Workspace::scratch();
        let mut c = Conversation::new(ws.id.clone());
        c.title = "Borrow checker vs. my HashMap".into();
        c.messages.push(Message::user(
            "Why does this fail to compile?\n\n```rust\nlet v = map.get(&k);\nmap.insert(k2, 1);\nprintln!(\"{v:?}\");\n```",
        ));
        let mut a = Message::new(
            R::Assistant,
            vec![
                Part::Reasoning("The user holds an immutable borrow from `get` across a mutable call. Explain NLL and suggest cloning or reordering.".into()),
                Part::Text("`map.get(&k)` returns a **reference into the map**, so `v` keeps an immutable borrow alive until its last use in `println!`. `insert` needs `&mut map` while that borrow is still live:\n\n1. Copy the value out first, or\n2. Move the `println!` above the `insert`.\n\n```rust\nlet v = map.get(&k).copied();\nmap.insert(k2, 1);\nprintln!(\"{v:?}\");\n```\n\n> Since Rust 2018 the borrow ends at its *last use* (non-lexical lifetimes), not at the end of the scope.".into()),
            ],
        );
        a.usage = Some(Usage {
            prompt_tokens: 812,
            completion_tokens: 164,
        });
        c.messages.push(a);
        store.save(&c).unwrap();
        let mut other = Conversation::new(ws.id.clone());
        other.title = "moon task for cargo nextest".into();
        other.updated -= chrono::Duration::hours(3);
        store.save(&other).unwrap();
        let mut third = Conversation::new(ws.id);
        third.title = "Explain SSE chunking".into();
        third.updated -= chrono::Duration::days(2);
        store.save(&third).unwrap();

        let (cfg, data) = (p.root.join("config.toml"), p.root.join("data"));
        let mut h = Harness::builder()
            .with_size([1400.0, 860.0])
            .with_max_steps(10)
            .with_accessibility_check(false)
            .wgpu()
            .build_eframe(move |cc| VoidApp::new(cc, None, cfg, data));
        until(&mut h, 10, "list", |a| a.state.conversations.len() == 3);
        let id = c.id;
        h.state_mut().apply_action(Action::Show(id));
        until(&mut h, 10, "loaded", |a| a.state.loaded.contains_key(&id));
        h.state_mut().state.last_tps = Some(58.4);
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("chat.png")).unwrap();

        // Empty chat in a fresh tab.
        let tab = layout::next_chat_id(&h.state().dock);
        layout::ensure_tab(
            &mut h.state_mut().dock,
            Tab::Chat {
                id: tab,
                conversation: None,
            },
        );
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("empty.png")).unwrap();

        // Settings tab in front.
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Settings);
        let first = h.state().state.config.profiles[0].clone();
        h.state_mut().backends.select(first);
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Backends);
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("backends.png")).unwrap();

        h.set_size(egui::vec2(560.0, 860.0));
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("narrow.png")).unwrap();
        h.set_size(egui::vec2(1400.0, 860.0));

        h.state_mut().state.config.theme = void_core::config::Theme::Light;
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("light.png")).unwrap();
    }

    fn paths_tmp() -> Paths {
        let t = tempfile::tempdir().unwrap();
        Paths {
            root: t.path().to_path_buf(),
            _tmp: Some(t),
        }
    }

    #[test]
    #[ignore]
    fn e2e_chat_with_strata() {
        let p = paths();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch" && a.state.config.active_profile == "strata"
        });

        // 1. First message from an empty Chat tab: creates the conversation and streams a reply.
        type_and_send(&mut h, "Reply with the single word pong.");
        until(&mut h, 30, "stream start", |a| !a.state.streams.is_empty());
        until(&mut h, 180, "reply", |a| {
            idle(a) && last_assistant(a).is_some()
        });
        let reply = last_assistant(h.state()).unwrap();
        println!(
            "reply: {:?} / reasoning {} chars / tps {:?}",
            reply.text(),
            reply.reasoning().len(),
            h.state().state.last_tps
        );
        assert!(reply.error.is_none(), "error: {:?}", reply.error);
        assert!(reply.text().to_lowercase().contains("pong"));
        assert!(h.state().state.last_tps.is_some());
        assert!(
            h.query_by_label_contains("tok/s").is_some(),
            "status bar shows tok/s"
        );
        assert!(
            h.query_by_label_contains("context ").is_some(),
            "status bar shows context"
        );
        assert_eq!(h.state().state.conversations.len(), 1);

        // 2. Stop mid-stream keeps the partial reply.
        type_and_send(
            &mut h,
            "Count from 1 to 400, one number per line, no other text.",
        );
        // With thinking on, the reply may arrive as reasoning first (or only); any output will do.
        until(&mut h, 120, "some streamed output", |a| {
            a.state
                .streams
                .values()
                .any(|s| s.text.len() + s.reasoning.len() > 20)
        });
        h.get_by_label_contains("Stop").click();
        until(&mut h, 10, "stopped", idle);
        let stopped = last_assistant(h.state()).unwrap();
        println!(
            "stopped after {} text / {} reasoning chars",
            stopped.text().len(),
            stopped.reasoning().len()
        );
        assert!(stopped.stopped);
        assert!(!stopped.text().is_empty() || !stopped.reasoning().is_empty());

        // 3. The same conversation through the Anthropic-style profile.
        let conv = h.state().focused_conversation().unwrap();
        h.state().engine.send(Command::SetConversationProfile(
            conv,
            Some("strata-anthropic".into()),
        ));
        until(&mut h, 10, "profile switch", |a| {
            a.state
                .loaded
                .get(&conv)
                .and_then(|c| c.profile_id.as_deref())
                == Some("strata-anthropic")
        });
        type_and_send(&mut h, "Now reply with the single word ping.");
        until(&mut h, 30, "stream start", |a| !a.state.streams.is_empty());
        until(&mut h, 180, "anthropic reply", |a| {
            idle(a) && last_assistant(a).is_some_and(|m| !m.stopped)
        });
        let ping = last_assistant(h.state()).unwrap();
        println!("anthropic reply: {:?}", ping.text());
        assert!(ping.error.is_none(), "error: {:?}", ping.error);
        assert!(ping.text().to_lowercase().contains("ping"));

        // 4. Close Settings, reopen it from View, then close it again and keep that layout.
        let path = h.state_mut().dock.find_tab(&Tab::Settings).unwrap();
        h.state_mut().dock.remove_tab(path);
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Settings);
        assert!(h.state().dock.find_tab(&Tab::Settings).is_some());
        let path = h.state_mut().dock.find_tab(&Tab::Settings).unwrap();
        h.state_mut().dock.remove_tab(path);
        h.step();
        h.state_mut().save_layout_now();
        drop(h);

        // 5. Restart: same conversations, same layout (no Settings tab).
        let mut h = harness(&p);
        until(&mut h, 10, "reopen", |a| a.state.conversations.len() == 1);
        h.step();
        assert!(
            h.state().dock.find_tab(&Tab::Settings).is_none(),
            "layout kept"
        );
        assert!(h.state().dock.find_tab(&Tab::Backends).is_some());

        // 6. A folder workspace has its own conversations; Scratch still has ours.
        let proj = p.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        h.state_mut().open_workspace(Some(proj));
        until(&mut h, 10, "folder workspace", |a| {
            a.state.workspace.name == "proj"
        });
        assert!(h.state().state.conversations.is_empty());
        assert!(
            h.state().dock.find_tab(&Tab::Settings).is_some(),
            "fresh workspace gets the default layout"
        );
        h.state_mut().open_workspace(None);
        until(&mut h, 10, "back to scratch", |a| {
            a.state.workspace.name == "Scratch"
        });
        until(&mut h, 10, "scratch list", |a| {
            a.state.conversations.len() == 1
        });
        h.step();
        assert!(
            h.state().dock.find_tab(&Tab::Settings).is_none(),
            "scratch layout restored"
        );
    }
}

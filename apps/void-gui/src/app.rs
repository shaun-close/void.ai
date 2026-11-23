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
use void_core::runner::RunId;
use void_core::store::Store;

use crate::commands::CommandId;
use crate::editor::EditorStore;
use crate::layout::{self, EditorView, Tab};
use crate::panels::{Action, Panels};
use crate::state::UiState;
use crate::theme::{self, icon};
use egui::RichText;

pub(crate) mod disk;
mod editors;
mod palette;

pub struct VoidApp {
    pub(crate) engine: EngineHandle,
    store: Store,
    pub(crate) state: UiState,
    pub(crate) dock: DockState<Tab>,
    /// Open editor buffers, shared by the tabs that show them.
    pub(crate) editors: EditorStore,
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
    pub(crate) review: crate::panels::review::ReviewUi,
    /// A first message waiting for the conversation it asked the engine to create.
    pending_start: Option<(u32, String)>,
    /// Quitting with unsaved editor buffers: the dialog is up / the user said quit anyway.
    quit_dialog: bool,
    pub(crate) quit_anyway: bool,
    /// Chat-view saves the quit dialog waits for, and what went wrong with the last try.
    quit_saving: Vec<u64>,
    quit_errors: Vec<String>,
    /// Editor tabs to close once their chat-view save lands: `(tab, request)`.
    closing: Vec<(u32, u64)>,
    /// Answers to chat-view saves someone waits for (None: it landed).
    chat_results: HashMap<u64, Option<String>>,
    /// The Files panel's tree and inline edits.
    pub(crate) files: crate::panels::files::FilesUi,
    /// Watches the workspace (and files open from elsewhere) for outside changes.
    pub(crate) disk: disk::DiskWatch,
    /// The editor tab focused last.
    last_editor: Option<u32>,
    /// The Runs panel's drafts.
    pub(crate) runs: crate::panels::runs::RunsUi,
    /// The Shift+Shift palette and what it searches.
    pub(crate) palette: crate::palette::PaletteUi,
    /// Watches raw input for a double tap of Shift.
    shift: crate::palette::DoubleShift,
    /// Run output applied by the last frame's [`drain`] (for tests).
    pub(crate) drained_last_frame: usize,
}

impl VoidApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        open: Option<PathBuf>,
        config_path: PathBuf,
        data_dir: PathBuf,
    ) -> Self {
        Self::with_factory(cc, open, config_path, data_dir, None)
    }

    /// `factory` replaces the real backends (tests).
    pub fn with_factory(
        cc: &eframe::CreationContext<'_>,
        open: Option<PathBuf>,
        config_path: PathBuf,
        data_dir: PathBuf,
        factory: Option<void_core::engine::BackendFactory>,
    ) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let store = Store::new(data_dir);
        let ctx = cc.egui_ctx.clone();
        let notify: void_core::engine::Notify = Arc::new(move || ctx.request_repaint());
        let engine = match factory {
            Some(f) => Engine::start_with(config_path, store.clone(), notify, f),
            None => Engine::start(config_path, store.clone(), notify),
        };
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
            editors: EditorStore::new(if cfg!(test) {
                crate::editor::Clipboard::memory()
            } else {
                crate::editor::Clipboard::system()
            }),
            inputs: HashMap::new(),
            focused_chat: None,
            layout_checked: Instant::now(),
            folders: mpsc::channel(),
            applied_style: None,
            title: String::new(),
            list: Default::default(),
            backends: Default::default(),
            md: Default::default(),
            review: Default::default(),
            pending_start: None,
            quit_dialog: false,
            quit_anyway: false,
            quit_saving: Vec::new(),
            quit_errors: Vec::new(),
            closing: Vec::new(),
            chat_results: HashMap::new(),
            files: Default::default(),
            disk: Default::default(),
            last_editor: None,
            runs: Default::default(),
            palette: Default::default(),
            shift: Default::default(),
            drained_last_frame: 0,
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let (engine, state) = (&self.engine, &mut self.state);
        let d = drain(
            || engine.try_recv(),
            |ev| state.apply(ev),
            DRAIN_BYTES,
            DRAIN_TIME,
        );
        self.drained_last_frame = d.bytes;
        if d.more {
            // The rest next frame: this one still draws (and Stop stays clickable).
            ctx.request_repaint();
        }
        if let Some(layout) = self.state.take_layout() {
            self.dock = layout
                .as_deref()
                .and_then(layout::from_text)
                .unwrap_or_else(layout::default_layout);
            layout::drop_run_outputs(&mut self.dock);
            // Drafts in the Runs panel belonged to the previous workspace.
            self.runs = Default::default();
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
            // Chat views read this workspace's layers; banners need every chat's changes.
            self.editors.overlays =
                self.state
                    .workspace
                    .root
                    .clone()
                    .map(|root| crate::editor::ChatOverlays {
                        store: self.store.clone(),
                        workspace: self.state.workspace.id.clone(),
                        root,
                    });
            self.engine.send(Command::ListWorkspaceChanges);
        }
        for gone in self.state.take_deleted() {
            for (_, tab) in self.dock.iter_all_tabs_mut() {
                match tab {
                    Tab::Chat { conversation, .. } if *conversation == Some(gone) => {
                        *conversation = None;
                    }
                    // Its pending changes went with it: show the file on disk.
                    Tab::Editor { view, .. } if *view == EditorView::Chat(gone) => {
                        *view = EditorView::Disk;
                    }
                    _ => {}
                }
            }
        }
        self.chat_view_events();
        if self.state.take_review_request() {
            layout::ensure_tab(&mut self.dock, Tab::Review);
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
        for run in self.state.take_started_runs() {
            self.show_run_output(run, true);
        }
        // Finished runs whose tab was closed are forgotten.
        let shown: std::collections::HashSet<_> = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::RunOutput { run } => Some(*run),
                _ => None,
            })
            .collect();
        self.state
            .run_views
            .retain(|id, v| v.finished.is_none() || shown.contains(id));
    }

    /// Bring `run`'s output tab forward, opening one if needed: in place of a finished run of
    /// the same config or task (`reuse`, for a new run), else beside the other output tabs,
    /// else in a new strip below the editors/chats. Keyboard focus stays where it was.
    fn show_run_output(&mut self, run: RunId, reuse: bool) {
        if let Some(path) = self.dock.find_tab(&Tab::RunOutput { run }) {
            let _ = self.dock.set_active_tab(path);
            return;
        }
        let mut focused = self.dock.focused_leaf();
        let origin = self.state.run_views.get(&run).map(|v| v.origin.clone());
        let views = &self.state.run_views;
        let stale = reuse
            .then(|| {
                self.dock.find_tab_from(|t| {
                    matches!(t, Tab::RunOutput { run: old } if views.get(old).is_some_and(|v| {
                        v.finished.is_some() && Some(&v.origin) == origin.as_ref()
                    }))
                })
            })
            .flatten();
        if let Some(path) = stale {
            if let Some((_, tab)) = self.dock.iter_all_tabs_mut().find(|(p, _)| *p == path) {
                *tab = Tab::RunOutput { run };
            }
            let _ = self.dock.set_active_tab(path);
        } else if let Some(near) = self
            .dock
            .find_tab_from(|t| matches!(t, Tab::RunOutput { .. }))
        {
            self.dock.set_focused_node_and_surface(near.node_path());
            self.dock.push_to_focused_leaf(Tab::RunOutput { run });
        } else if let Some(main) = self
            .dock
            .find_tab_from(|t| matches!(t, Tab::Editor { .. }))
            .or_else(|| self.dock.find_tab_from(|t| matches!(t, Tab::Chat { .. })))
        {
            let at = main.node_path();
            let [kept, _] = self.dock.split(
                at,
                egui_dock::Split::Below,
                0.68,
                egui_dock::Node::leaf_with(vec![Tab::RunOutput { run }]),
            );
            // Splitting a leaf moves its tabs to a new index (the old one is now the parent).
            if focused == Some(at) {
                focused = Some(egui_dock::NodePath {
                    surface: at.surface,
                    node: kept,
                });
            }
        } else {
            self.dock.push_to_focused_leaf(Tab::RunOutput { run });
        }
        if let Some(f) = focused {
            self.dock.set_focused_node_and_surface(f);
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
            Action::OpenFile(path) => {
                let path = match &self.state.workspace.root {
                    Some(root) => root.join(path),
                    None => path,
                };
                self.open_editor(path, EditorView::Disk, None);
                return;
            }
            Action::CloseEditor(tab) => {
                self.close_editor_tab(tab);
                return;
            }
            Action::DuplicateEditor(tab) => {
                if let Some((path, view)) = self.editor_key(tab) {
                    self.open_editor_tab(path, view);
                }
                return;
            }
            Action::Renamed { from, to } => {
                self.follow_rename(&from, &to);
                return;
            }
            Action::ReviewFile { conversation, path } => {
                self.review.conversation = Some(conversation);
                self.review.selected = Some(path);
                layout::ensure_tab(&mut self.dock, Tab::Review);
                return;
            }
            Action::SwitchEditorView { tab, view } => {
                self.switch_editor_view(tab, view);
                return;
            }
            Action::OpenAt { path, line } => {
                self.open_editor(path, EditorView::Disk, Some(line));
                return;
            }
            Action::ShowRun(run) => {
                self.show_run_output(run, false);
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
                self.command_button(ui, CommandId::OpenFolder);
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
                self.command_button(ui, CommandId::Scratch);
                ui.separator();
                self.command_button(ui, CommandId::NewChat);
                ui.separator();
                self.command_button(ui, CommandId::Quit);
            });
            ui.menu_button("View", |ui| {
                for id in [
                    CommandId::NewChatTab,
                    CommandId::ShowChats,
                    CommandId::ShowFiles,
                    CommandId::ShowRuns,
                    CommandId::ShowBackends,
                    CommandId::ShowSettings,
                    CommandId::ShowReview,
                ] {
                    self.command_button(ui, id);
                }
                ui.separator();
                self.command_button(ui, CommandId::ResetLayout);
            });
            ui.menu_button("Run", |ui| {
                self.command_button(ui, CommandId::RerunLast);
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
            if self.state.workspace.root.is_some() {
                self.env_pill(ui, &pal);
            }
            match self.state.profile_for(conv) {
                Some(p) => {
                    let color = match p.kind {
                        void_core::config::BackendKind::OpenAi => pal.accent2,
                        void_core::config::BackendKind::Anthropic => pal.accent3,
                        void_core::config::BackendKind::ClaudeCode => pal.accent,
                    };
                    let reported = conv
                        .and_then(|c| self.state.claude_models.get(&c))
                        .map(String::as_str);
                    let (label, hover) = profile_pill(p, reported);
                    theme::pill(ui, icon::PLUGS, label, color).on_hover_text(hover);
                }
                None => {
                    theme::pill(ui, icon::WARNING, "no profile", pal.err);
                }
            };
            let streaming = conv.is_some_and(|c| self.state.streams.contains_key(&c));
            let turn_status = conv.and_then(|c| self.state.turn_status.get(&c));
            if let Some(st) = turn_status {
                // Waiting (for the sandbox, an API retry…): say so instead of "streaming".
                theme::pill(ui, icon::ARROW_CLOCKWISE, shorten(st, 48), pal.warn).on_hover_text(st);
            } else if streaming {
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
            let pending = conv
                .and_then(|c| self.state.changes.get(&c))
                .map_or(0, Vec::len);
            if pending > 0
                && theme::pill(ui, icon::GIT_DIFF, format!("{pending} pending"), pal.accent)
                    .interact(egui::Sense::click())
                    .on_hover_text("Open Review")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            {
                layout::ensure_tab(&mut self.dock, Tab::Review);
            }
            if self.state.workspace.root.is_some() {
                match &self.state.sandbox {
                    Some(void_core::sandbox::Availability::Ready { landlock: true }) => {
                        theme::pill(ui, icon::SHIELD_CHECK, "sandboxed", pal.ok).on_hover_text(
                            "bash runs in bubblewrap + Landlock: the rest of the disk is read-only",
                        );
                    }
                    Some(void_core::sandbox::Availability::Ready { landlock: false }) => {
                        theme::pill(
                            ui,
                            icon::SHIELD_WARNING,
                            "sandboxed (no landlock)",
                            pal.warn,
                        )
                        .on_hover_text("bash runs in bubblewrap; the Landlock helper isn't active");
                    }
                    Some(void_core::sandbox::Availability::Unavailable(why)) => {
                        theme::pill(ui, icon::SHIELD_WARNING, "no sandbox", pal.err)
                            .on_hover_text(format!("bash is unavailable: {why}"));
                    }
                    None => {}
                }
            }
            if let (Some(c), Some(p)) = (
                conv.and_then(|c| self.state.loaded.get(&c)),
                self.state.profile_for(conv),
            ) {
                let used = context_used(c);
                ui.add_space(4.0);
                if p.kind == void_core::config::BackendKind::ClaudeCode {
                    // Claude Code manages its own window; the profile's limit means nothing
                    // here, so no meter or percentage, just what the latest request used.
                    ui.label(theme::ico(icon::GAUGE).color(pal.text_dim));
                    ui.label(
                        RichText::new(format!("context {}", fmt_k(used)))
                            .small()
                            .color(pal.text_dim),
                    )
                    .on_hover_text(format!(
                        "About {used} tokens in this chat's latest request. Claude Code manages \
                         (and compacts) its own context window."
                    ));
                } else {
                    let frac = theme::context_fraction(used, p.context_limit);
                    let color = theme::meter_color(&pal, frac);
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

impl VoidApp {
    /// The active environment profile for runs; a click opens a menu to switch.
    fn env_pill(&mut self, ui: &mut egui::Ui, pal: &theme::Palette) {
        let active = self.state.envs.active.clone();
        let (label, color) = match &active {
            Some(name) => (name.as_str(), pal.accent2),
            None => ("no env", pal.text_faint),
        };
        let r = theme::pill_button(ui, icon::GLOBE, label, color)
            .on_hover_text("Environment profile for runs (click to switch)");
        let names: Vec<String> = self
            .state
            .envs
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        egui::Popup::menu(&r).show(|ui| {
            let mut pick = None;
            if ui
                .selectable_label(active.is_none(), "No environment")
                .clicked()
            {
                pick = Some(None);
            }
            for name in names {
                if ui
                    .selectable_label(active.as_ref() == Some(&name), &name)
                    .clicked()
                {
                    pick = Some(Some(name));
                }
            }
            ui.separator();
            if ui
                .button(theme::itext(icon::PENCIL, "Edit environments…"))
                .clicked()
            {
                self.runs.show_envs = true;
                layout::ensure_tab(&mut self.dock, Tab::Runs);
                ui.close();
            }
            if let Some(pick) = pick {
                if pick != active {
                    self.engine.send(Command::SetActiveEnv(pick));
                }
                ui.close();
            }
        });
    }
}

/// The profile pill's label and hover text. Claude Code profiles have no URL: they show the
/// model Claude Code reported for this chat (`claude_model`), else the configured one, else
/// "default model", and the command they run.
fn profile_pill(p: &void_core::config::Profile, claude_model: Option<&str>) -> (String, String) {
    let set = |s: &str| !s.trim().is_empty();
    match p.kind {
        void_core::config::BackendKind::ClaudeCode => {
            let name = if set(&p.name) {
                p.name.as_str()
            } else {
                "Claude Code"
            };
            let reported = claude_model.filter(|m| set(m));
            let configured = if set(&p.model) {
                p.model.as_str()
            } else {
                "default model"
            };
            let mut hover = format!(
                "Claude Code: runs `{}` · model: {configured}",
                p.claude_command()
            );
            if let Some(m) = reported {
                hover.push_str(&format!("\nThis chat's session uses {m}"));
            }
            (
                format!("{name} · {}", reported.unwrap_or(configured)),
                hover,
            )
        }
        _ => {
            let label = if set(&p.model) {
                format!("{} · {}", p.name, p.model)
            } else {
                p.name.clone()
            };
            let hover = if set(&p.base_url) {
                p.base_url.clone()
            } else {
                "No base URL set".into()
            };
            (label, hover)
        }
    }
}

/// At most `max` characters of `s`, with "…" when cut.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn fmt_k(n: u32) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f32 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Run output applied per frame at most (its terminal parsing runs on the UI thread)...
const DRAIN_BYTES: usize = 1024 * 1024;
/// ...and how long a frame spends applying events at most.
const DRAIN_TIME: Duration = Duration::from_millis(12);

struct Drained {
    /// Run output applied.
    bytes: usize,
    /// Stopped at a budget with events possibly left.
    more: bool,
}

/// Apply events from `next` in order until there are none or a budget is spent (checked
/// after each event, so at least one is applied).
fn drain(
    mut next: impl FnMut() -> Option<void_core::engine::Event>,
    mut apply: impl FnMut(void_core::engine::Event),
    max_bytes: usize,
    max_time: Duration,
) -> Drained {
    let start = Instant::now();
    let mut bytes = 0;
    while let Some(ev) = next() {
        if let void_core::engine::Event::RunOutput { text, .. } = &ev {
            bytes += text.len();
        }
        apply(ev);
        if bytes >= max_bytes || start.elapsed() >= max_time {
            return Drained { bytes, more: true };
        }
    }
    Drained { bytes, more: false }
}

impl eframe::App for VoidApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump(&ctx);
        self.apply_style(&ctx);
        let title = format!("void.ai — {}", self.state.workspace.name);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        self.editors.settings = self.state.config.editor.clone();
        self.editors.dialog = self.quit_dialog || self.files.confirm_delete.is_some();
        self.editors.focused_widget = None;
        self.editors.busy_chats = self
            .state
            .active_turns
            .iter()
            .chain(self.state.streams.keys())
            .copied()
            .collect();
        self.poll_disk(&ctx);
        self.editors.poll(&ctx);
        self.global_save(&ctx);
        // Re-run last: app-wide, taken before an editor could see it.
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                egui::Key::F10,
            )
        }) {
            self.engine.send(Command::RerunLast);
        }
        self.palette_keys(&ctx);
        self.palette_ui(&ctx);

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
        self.close_dialog(&ctx);
        self.quit_dialog(&ctx);
        crate::panels::files::delete_dialog(&ctx, &mut self.files, &mut self.state);
        // Chat-view saves made this frame (Ctrl+S, :w, dialogs) go to the engine now, before
        // a quit could end the app.
        self.send_overlay_saves();
        // One clipboard for the app: text egui copies (text fields, copy buttons) goes through the
        // same `editor::Clipboard` that pastes and Vim's "+ / "* registers read.
        let clipboard = &self.editors.clipboard;
        ctx.output_mut(|o| {
            o.commands.retain(|c| match c {
                egui::OutputCommand::CopyText(text) => {
                    clipboard.set(text);
                    false
                }
                _ => true,
            })
        });
    }

    /// Clipboard shortcuts: keys for a focused editor (Vim's Ctrl+V), clipboard events for
    /// everything else (see `editor::input::route_clipboard_keys`).
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let editor = self.editors.focused_widget.is_some()
            && ctx.memory(|m| m.focused()) == self.editors.focused_widget;
        crate::editor::input::route_clipboard_keys(
            &mut raw_input.events,
            editor,
            &self.editors.clipboard,
        );
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
        let focused_editor = match self.dock.find_active_focused() {
            Some((_, Tab::Editor { id, .. })) => Some(*id),
            _ => None,
        };
        if focused_editor.is_some() {
            // Recent files are files on disk: a chat's version stays an open tab in the
            // palette (reopening it from a path would land on the Disk view).
            if focused_editor != self.last_editor
                && let Some((path, EditorView::Disk)) =
                    focused_editor.and_then(|id| self.editor_key(id))
            {
                self.palette.recent.touch(&path);
            }
            self.last_editor = focused_editor;
        }
        let active_file = self
            .last_editor
            .and_then(|id| self.editor_key(id))
            .map(|(path, _)| path);
        let view_chats = self.view_chats();
        let mut panels = Panels {
            state: &mut self.state,
            engine: &self.engine,
            inputs: &mut self.inputs,
            actions: &mut actions,
            list: &mut self.list,
            backends: &mut self.backends,
            md: &mut self.md,
            review: &mut self.review,
            focused,
            editors: &mut self.editors,
            focused_editor,
            files: &mut self.files,
            active_file,
            view_chats,
            runs: &mut self.runs,
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
        for a in std::mem::take(&mut self.editors.actions) {
            self.apply_editor_action(a);
        }
        let shown = self.editor_tab_counts();
        self.editors.retain_shown(&shown);
        self.maybe_save_layout();
    }
}

/// End-to-end checks of the real app against a running Strata (`strata launch --preset daily`),
/// driven headlessly by egui_kittest. Run with:
/// `cargo test -p void-gui e2e -- --ignored --nocapture`
/// Set VOID_AI_E2E_DIR to keep the config and data it writes (otherwise a temp dir is used).
#[cfg(test)]
mod drain_tests {
    use super::*;
    use void_core::engine::Event;

    /// A flood is applied a budget's worth per call, in order, with the run's end after all
    /// of its output.
    #[test]
    fn drain_stops_at_the_budget_and_keeps_order() {
        let chunk = "x".repeat(64 * 1024);
        let mut queue: std::collections::VecDeque<Event> = (0..100)
            .map(|_| Event::RunOutput {
                run: RunId(1),
                text: chunk.clone(),
            })
            .collect();
        queue.push_back(Event::Info("after".into()));
        let mut got = Vec::new();
        let mut calls = 0;
        loop {
            calls += 1;
            let d = drain(
                || queue.pop_front(),
                |e| got.push(e),
                1024 * 1024,
                Duration::from_secs(3600),
            );
            assert!(d.bytes <= 1024 * 1024, "{}", d.bytes);
            if !d.more {
                break;
            }
        }
        assert!(calls >= 7, "{calls}");
        assert_eq!(got.len(), 101);
        assert!(matches!(got.last(), Some(Event::Info(t)) if t == "after"));
        // A spent time budget still applies one event.
        queue.push_back(Event::Info("one".into()));
        queue.push_back(Event::Info("two".into()));
        let d = drain(|| queue.pop_front(), |e| got.push(e), 1, Duration::ZERO);
        assert!(d.more && queue.len() == 1);
    }
}

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
                void_core::fsutil::remove_tree(&root).expect("clean the e2e dir");
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

        // An agent conversation in a folder workspace: tool cards, live bash, an approval.
        let proj = p.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let wsp = store.workspace_for(&proj).unwrap();
        let mut ac = Conversation::new(wsp.id.clone());
        ac.title = "Fix the flaky SSE test".into();
        ac.messages.push(Message::user(
            "The SSE chunking test fails sometimes. Can you look?",
        ));
        ac.messages.push(Message::new(
            R::Assistant,
            vec![
                Part::Text("Let me look at the parser first.".into()),
                Part::ToolCall {
                    id: "t1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"src/backend/sse.rs"}"#.into(),
                },
                Part::ToolCall {
                    id: "t2".into(),
                    name: "grep".into(),
                    arguments: r#"{"pattern":"after_cr"}"#.into(),
                },
            ],
        ));
        ac.messages.push(Message::new(
            R::Tool,
            vec![Part::ToolResult {
                call_id: "t1".into(),
                content: "     1\t//! A Server-Sent Events parser…".into(),
            }],
        ));
        ac.messages.push(Message::new(
            R::Tool,
            vec![Part::ToolResult {
                call_id: "t2".into(),
                content: "src/backend/sse.rs:17:    after_cr: bool,".into(),
            }],
        ));
        ac.messages.push(Message::new(
            R::Assistant,
            vec![
                Part::Text(
                    "A `\\r` at the end of a chunk loses its state. I'll fix it and run the tests."
                        .into(),
                ),
                Part::ToolCall {
                    id: "t3".into(),
                    name: "edit".into(),
                    arguments: r#"{"path":"src/backend/sse.rs"}"#.into(),
                },
                Part::ToolCall {
                    id: "t4".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"cargo test -p void-core sse"}"#.into(),
                },
            ],
        ));
        ac.messages.push(Message::new(
            R::Tool,
            vec![Part::ToolResult {
                call_id: "t3".into(),
                content: "edited src/backend/sse.rs (1 replacement)".into(),
            }],
        ));
        store.save(&ac).unwrap();
        std::fs::create_dir_all(proj.join("src/backend")).unwrap();
        let sse: String = (1..=30).map(|i| format!("    line {i};\n")).collect();
        std::fs::write(proj.join("src/backend/sse.rs"), &sse).unwrap();
        let ov =
            void_core::overlay::Overlay::open(&store.overlay_dir(&wsp.id, &ac.id), &proj).unwrap();
        ov.write(
            std::path::Path::new("src/backend/sse.rs"),
            sse.replace(
                "    line 4;\n",
                "    if self.after_cr && b == b'\\n' {\n        continue;\n    }\n",
            )
            .as_bytes(),
        )
        .unwrap();
        ov.write(
            std::path::Path::new("tests/chunking.rs"),
            b"#[test]\nfn split_crlf() {}\n",
        )
        .unwrap();
        h.state_mut().open_workspace(Some(proj.clone()));
        until(&mut h, 10, "proj", |a| a.state.workspace.name == "proj");
        let aid = ac.id;
        h.state_mut().apply_action(Action::Show(aid));
        until(&mut h, 10, "agent conv", |a| {
            a.state.loaded.contains_key(&aid)
        });
        h.state_mut()
            .state
            .apply(void_core::engine::Event::ToolStarted {
                conversation: aid,
                call_id: "t4".into(),
                name: "bash".into(),
                summary: "cargo test -p void-core sse".into(),
            });
        h.state_mut().state.apply(void_core::engine::Event::ToolOutput {
            conversation: aid,
            call_id: "t4".into(),
            text: "   Compiling void-core v0.1.0\n    Finished `test` profile\n     Running unittests\n".into(),
        });
        h.state_mut()
            .state
            .apply(void_core::engine::Event::ApprovalNeeded(
                void_core::permissions::ApprovalRequest {
                    id: 9,
                    conversation: aid,
                    tool: "bash".into(),
                    summary: "git stash".into(),
                    command: Some("git stash && cargo test".into()),
                    kind: Default::default(),
                },
            ));
        for _ in 0..6 {
            h.step();
        }
        h.render().unwrap().save(out.join("agent.png")).unwrap();

        // A Claude Code chat: text, a subagent's Task (its work nested, collapsed), an Edit,
        // a Bash and a pending approval offering "Always allow".
        let cc = void_core::config::Profile {
            id: "claude-code".into(),
            name: "Claude Code".into(),
            kind: void_core::config::BackendKind::ClaudeCode,
            base_url: String::new(),
            model: String::new(),
            ..h.state().state.saved_config.profiles[0].clone()
        };
        h.state_mut().state.saved_config.profiles.push(cc);
        let sse_path = proj.join("src/backend/sse.rs").display().to_string();
        let mut cq = Conversation::new(wsp.id.clone());
        cq.title = "Claude: fix the SSE chunking".into();
        cq.profile_id = Some("claude-code".into());
        let tcall = |id: &str, name: &str, args: serde_json::Value| Part::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        };
        let tres = |id: &str, content: &str| {
            Message::new(
                R::Tool,
                vec![Part::ToolResult {
                    call_id: id.into(),
                    content: content.into(),
                }],
            )
        };
        cq.messages.push(Message::user(
            "The SSE chunking test is flaky. Find the cause, fix it and push.",
        ));
        cq.messages.push(Message::new(
            R::Assistant,
            vec![
                Part::Text("I'll have a subagent find the parser first.".into()),
                tcall(
                    "cc-t1",
                    "Task",
                    serde_json::json!({"description": "Find the SSE parser", "prompt": "Find where SSE chunks are split", "subagent_type": "general-purpose"}),
                ),
            ],
        ));
        cq.messages.push(tres(
            "cc-t1",
            "The parser is src/backend/sse.rs; `after_cr` is reset at every chunk.",
        ));
        let mut fix = Message::new(
            R::Assistant,
            vec![
                Part::Text(
                    "Found it: a `\\r` at the end of one chunk and `\\n` at the start of the next \
                     count as two line ends. Fixing that and running the tests:"
                        .into(),
                ),
                tcall(
                    "cc-e1",
                    "Edit",
                    serde_json::json!({
                        "file_path": sse_path,
                        "old_string": "            self.after_cr = false;",
                        "new_string": "            if self.after_cr && b == b'\\n' {\n                self.after_cr = false;\n                continue;\n            }",
                    }),
                ),
                tcall(
                    "cc-b1",
                    "Bash",
                    serde_json::json!({"command": "cargo test -p void-core sse", "description": "Run the SSE tests"}),
                ),
            ],
        );
        fix.usage = Some(Usage {
            prompt_tokens: 41_870,
            completion_tokens: 236,
        });
        cq.messages.push(fix);
        cq.messages.push(tres(
            "cc-e1",
            "The file src/backend/sse.rs has been updated.",
        ));
        cq.messages.push(tres(
            "cc-b1",
            "running 12 tests\n............\ntest result: ok. 12 passed; 0 failed",
        ));
        store.save(&cq).unwrap();
        let cid = cq.id;
        h.state_mut().apply_action(Action::Show(cid));
        until(&mut h, 10, "claude conv", |a| {
            a.state.loaded.contains_key(&cid)
        });
        {
            use void_core::engine::Event as E;
            let st = &mut h.state_mut().state;
            st.apply(E::ClaudeInit {
                conversation: cid,
                model: "claude-opus-4-5".into(),
            });
            st.apply(E::ToolStarted {
                conversation: cid,
                call_id: "cc-t1".into(),
                name: "Task".into(),
                summary: "Find the SSE parser".into(),
            });
            for text in [
                "› Grep after_cr\n",
                "› Read src/backend/sse.rs\n",
                "The parser resets after_cr per chunk.",
            ] {
                st.apply(E::ToolOutput {
                    conversation: cid,
                    call_id: "cc-t1".into(),
                    text: text.into(),
                });
            }
            st.apply(E::ToolFinished {
                conversation: cid,
                call_id: "cc-t1".into(),
                output: void_core::tools::ToolOutput::ok(
                    "The parser is src/backend/sse.rs; `after_cr` is reset at every chunk.",
                ),
            });
            // The reply in progress: its Bash waits on the approval below.
            st.apply(E::StreamStarted(cid));
            st.apply(E::Delta {
                conversation: cid,
                kind: void_core::engine::DeltaKind::Text,
                text: "All 12 pass. Pushing the fix:".into(),
            });
            st.apply(E::ToolStarted {
                conversation: cid,
                call_id: "cc-b2".into(),
                name: "Bash".into(),
                summary: "git push origin fix-sse-crlf".into(),
            });
            st.apply(E::ApprovalNeeded(void_core::permissions::ApprovalRequest {
                id: 11,
                conversation: cid,
                tool: "Bash".into(),
                summary: "git push origin fix-sse-crlf".into(),
                command: Some("git push origin fix-sse-crlf".into()),
                kind: void_core::permissions::ApprovalKind::ClaudeCode {
                    always_allow: true,
                    title: Some("Claude wants to push to the remote".into()),
                    description: Some("Push the fix branch to origin".into()),
                    blocked_path: None,
                },
            }));
        }
        h.set_size(egui::vec2(1400.0, 1180.0));
        for _ in 0..6 {
            h.step();
        }
        h.render()
            .unwrap()
            .save(out.join("claude-code-chat.png"))
            .unwrap();
        h.set_size(egui::vec2(1400.0, 860.0));
        {
            let st = &mut h.state_mut().state;
            st.apply(void_core::engine::Event::ApprovalResolved { id: 11 });
            st.streams.remove(&cid);
        }
        h.state_mut().apply_action(Action::Show(aid));
        until(&mut h, 10, "agent conv again", |a| {
            a.focused_conversation() == Some(aid)
        });

        layout::ensure_tab(&mut h.state_mut().dock, Tab::Review);
        h.state_mut().review.selected = Some("src/backend/sse.rs".into());
        until(&mut h, 10, "diff", |a| !a.state.diffs.is_empty());
        for _ in 0..4 {
            h.step();
        }
        h.render().unwrap().save(out.join("review.png")).unwrap();

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

        // An editor tab: Rust, highlighted, Normal mode with a Visual selection.
        h.state_mut().state.config.theme = void_core::config::Theme::Dark;
        let src = proj.join("src/backend/chunk.rs");
        std::fs::write(
            &src,
            r#"//! Split a byte stream into Server-Sent Events.

use std::collections::VecDeque;

/// One event: its name (`message` when the stream gave none) and data lines.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub name: String,
    pub data: String,
}

pub struct Parser {
    buf: Vec<u8>,
    after_cr: bool,
    ready: VecDeque<SseEvent>,
}

impl Parser {
    pub fn new() -> Self {
        Parser { buf: Vec::with_capacity(4096), after_cr: false, ready: VecDeque::new() }
    }

    /// Feed a chunk; complete events queue up for `next`.
    pub fn push(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if self.after_cr && b == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = b == b'\r';
            match b {
                b'\r' | b'\n' => self.line(),
                _ => self.buf.push(b),
            }
        }
    }

    fn line(&mut self) {
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        if let Some(data) = text.strip_prefix("data: ") {
            self.ready.push_back(SseEvent { name: "message".into(), data: data.to_string() });
        }
    }
}
"#,
        )
        .unwrap();
        h.state_mut()
            .open_editor(src.clone(), EditorView::Disk, Some(24));
        until(&mut h, 10, "highlighted", |a| {
            a.editors
                .doc(&(src.clone(), EditorView::Disk))
                .is_some_and(|d| d.highlighted())
        });
        h.event(egui::Event::Text("wvjje".into()));
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("editor.png")).unwrap();

        // The Files panel beside the editor, revealing its file; the edited buffer's file
        // changed on disk behind it.
        h.key_press(egui::Key::Escape);
        for (path, text) in [
            (".gitignore", "target/\n*.log\n"),
            ("Cargo.toml", "[package]\nname = \"proj\"\n"),
            ("README.md", "# proj\n"),
            ("build.log", "ok\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/backend/mod.rs", "pub mod chunk;\npub mod sse;\n"),
            ("target/debug/proj", "bin"),
            ("tests/chunking.rs", "#[test]\nfn split_crlf() {}\n"),
        ] {
            let f = proj.join(path);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        until(&mut h, 10, "watcher", |a| a.disk.ready());
        h.event(egui::Event::Text("O// tidy".into()));
        h.key_press(egui::Key::Escape);
        h.step();
        std::fs::write(&src, "// rewritten by another editor\n").unwrap();
        let key = (src.clone(), EditorView::Disk);
        until(&mut h, 10, "changed on disk", |a| {
            a.editors.doc(&key).is_some_and(|d| d.disk_changed())
        });
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Files);
        h.state_mut().files.reveal(&src);
        for _ in 0..5 {
            h.step();
        }
        h.render().unwrap().save(out.join("files.png")).unwrap();

        // A file the agent changed: its Disk tab with the pending-changes banner, then the
        // chat's version of it.
        let sse = proj.join("src/backend/sse.rs");
        h.state_mut()
            .open_editor(sse.clone(), EditorView::Disk, Some(4));
        for _ in 0..5 {
            h.step();
        }
        h.render()
            .unwrap()
            .save(out.join("editor-pending.png"))
            .unwrap();
        // "View void's version" switches the same tab.
        let tab = h.state().last_editor.unwrap();
        h.state_mut().apply_action(Action::SwitchEditorView {
            tab,
            view: EditorView::Chat(aid),
        });
        until(&mut h, 10, "chat view highlighted", |a| {
            a.editors
                .doc(&(sse.clone(), EditorView::Chat(aid)))
                .is_some_and(|d| d.highlighted())
        });
        for _ in 0..5 {
            h.step();
        }
        h.render()
            .unwrap()
            .save(out.join("editor-chat-view.png"))
            .unwrap();

        // The same editor tab in a narrow window.
        h.set_size(egui::vec2(560.0, 860.0));
        for _ in 0..5 {
            h.step();
        }
        h.render()
            .unwrap()
            .save(out.join("editor-narrow.png"))
            .unwrap();
        h.set_size(egui::vec2(1400.0, 860.0));

        // The runner: tasks, configs in groups with one open in the editor, profiles with a
        // masked value, a finished run and a running one with a progress bar.
        {
            use void_core::engine::Event as E;
            use void_core::runner::{
                EnvProfile, EnvValue, Envs, Group, MoonTask, OnFailure, RunConfig, RunId, Runs,
                Step, StepConfig,
            };
            let task = |project: &str, id: &str, command: &str| MoonTask {
                project: project.into(),
                id: id.into(),
                target: format!("{project}:{id}"),
                command: command.into(),
                description: None,
                source: Some(project.into()),
                run_from_workspace_root: false,
            };
            let step = |target: &str, on_failure| StepConfig {
                step: Step::Moon {
                    target: target.into(),
                    args: vec![],
                },
                on_failure,
            };
            let mut check = RunConfig::new(
                "Check everything",
                vec![
                    step("void-core:lint", OnFailure::Continue),
                    step("void-core:test", OnFailure::Stop),
                    StepConfig {
                        step: Step::Shell {
                            command: "cargo build --release".into(),
                        },
                        on_failure: OnFailure::Stop,
                    },
                ],
            );
            check.env_profile = Some("dev".into());
            let runs = Runs {
                groups: vec![
                    Group {
                        name: "CI".into(),
                        configs: vec![
                            check.clone(),
                            RunConfig::new(
                                "Format",
                                vec![step("void-core:format", OnFailure::Stop)],
                            ),
                        ],
                    },
                    Group {
                        name: "Dev".into(),
                        configs: vec![RunConfig::new(
                            "Serve docs",
                            vec![step("docs:serve", OnFailure::Stop)],
                        )],
                    },
                ],
            };
            let envs = Envs {
                active: Some("dev".into()),
                profiles: vec![
                    EnvProfile {
                        name: "dev".into(),
                        vars: [
                            ("API_TOKEN".to_string(), EnvValue::Value("hunter2".into())),
                            ("RUST_LOG".to_string(), EnvValue::Value("debug".into())),
                            ("HOME_DIR".to_string(), EnvValue::Env("HOME".into())),
                        ]
                        .into(),
                    },
                    EnvProfile {
                        name: "prod".into(),
                        vars: [(
                            "API_TOKEN".to_string(),
                            EnvValue::Cmd("pass show api".into()),
                        )]
                        .into(),
                    },
                ],
            };
            h.state().engine.send(Command::SaveRuns(runs));
            h.state().engine.send(Command::SaveEnvs(envs));
            until(&mut h, 10, "runs saved", |a| {
                a.state.runs.groups.len() == 2 && a.state.envs.profiles.len() == 2
            });
            std::fs::create_dir_all(proj.join("crates/core/src")).unwrap();
            std::fs::write(
                proj.join("crates/core/src/lib.rs"),
                (1..60).map(|i| format!("// {i}\n")).collect::<String>(),
            )
            .unwrap();
            let st = &mut h.state_mut().state;
            st.apply(E::Tasks(vec![
                task("void-core", "format", "cargo fmt --all"),
                task(
                    "void-core",
                    "lint",
                    "cargo clippy --workspace -- -D warnings",
                ),
                task("void-core", "test", "cargo test --workspace"),
                task("void-gui", "run", "cargo run --package void-gui"),
                task("docs", "serve", "mdbook serve"),
            ]));
            let done = RunId(9001);
            st.apply(E::RunStarted {
                run: done,
                name: "Check everything".into(),
                steps: 3,
                origin: void_core::runner::RunOrigin::Config {
                    id: check.id.clone(),
                },
            });
            st.apply(E::RunStep {
                run: done,
                index: 0,
                label: "moon run void-core:lint".into(),
                base_dirs: vec![proj.clone()],
            });
            st.apply(E::RunOutput {
                run: done,
                text: "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `chunk`\x1b[0m\n   \x1b[1m\x1b[94m--> \x1b[0mcrates/core/src/lib.rs:42:9\n    \x1b[94m|\x1b[0m\n\x1b[32m    Finished\x1b[0m `dev` profile in 3.21s\n".into(),
            });
            st.apply(E::RunStep {
                run: done,
                index: 1,
                label: "moon run void-core:test".into(),
                base_dirs: vec![proj.clone()],
            });
            st.apply(E::RunOutput {
                run: done,
                text: "running 3 tests\ntest sse::split_crlf ... \x1b[31mFAILED\x1b[0m\n\nthread 'sse::split_crlf' panicked at crates/core/src/lib.rs:17:5:\nassertion failed\n\x1b[31merror\x1b[0m: test failed\n".into(),
            });
            st.apply(E::RunFinished {
                run: done,
                ok: false,
                failed_step: Some(1),
                exit_code: Some(101),
                stopped: false,
                error: None,
                duration_ms: 14_320,
            });
            let live = RunId(9002);
            st.apply(E::RunStarted {
                run: live,
                name: "void-gui:run".into(),
                steps: 1,
                origin: void_core::runner::RunOrigin::Task {
                    target: "void-gui:run".into(),
                    args: vec![],
                },
            });
            st.apply(E::RunStep {
                run: live,
                index: 0,
                label: "moon run void-gui:run".into(),
                base_dirs: vec![proj.clone()],
            });
            st.apply(E::RunOutput {
                run: live,
                text: "\x1b[32m   Compiling\x1b[0m egui v0.37.0\n\x1b[36m    Building\x1b[0m [=====>      ] 211/412\r\x1b[36m    Building\x1b[0m [=========>  ] 380/412: void-gui".into(),
            });
            st.runs_warning = Some(
                "runs.toml was unreadable (expected `]`); kept as runs.toml.bad-1 and started fresh"
                    .into(),
            );
        }
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Runs);
        let first = h.state().state.runs.groups[0].configs[0].id.clone();
        let runs = h.state().state.runs.clone();
        h.state_mut().runs.edit_config(&runs, &first);
        h.state_mut().runs.show_envs = true;
        h.set_size(egui::vec2(1400.0, 1000.0));
        for _ in 0..8 {
            h.step();
        }
        h.render().unwrap().save(out.join("runs.png")).unwrap();
        // The finished run's tab in front.
        let path = h
            .state()
            .dock
            .find_tab(&Tab::RunOutput {
                run: void_core::runner::RunId(9001),
            })
            .unwrap();
        let _ = h.state_mut().dock.set_active_tab(path);
        for _ in 0..4 {
            h.step();
        }
        h.render()
            .unwrap()
            .save(out.join("run-output.png"))
            .unwrap();
        // The environments, with the config editor closed and the env pill's menu open.
        h.state_mut().runs = Default::default();
        h.state_mut().runs.show_envs = true;
        for _ in 0..4 {
            h.step();
        }
        h.get_by_label_contains("\u{E288}dev").click();
        for _ in 0..4 {
            h.step();
        }
        h.render().unwrap().save(out.join("runs-envs.png")).unwrap();
        h.set_size(egui::vec2(1400.0, 860.0));
        h.key_press(egui::Key::Escape);
        h.step();

        // The palette: recent files and tabs, then a query over everything, then `!` runs.
        until(&mut h, 10, "palette files", |a| {
            !a.palette.files.files().is_empty()
        });
        let shift_shift = |h: &mut Harness<'_, VoidApp>| {
            for m in [
                egui::Modifiers::SHIFT,
                egui::Modifiers::NONE,
                egui::Modifiers::SHIFT,
                egui::Modifiers::NONE,
            ] {
                h.event(egui::Event::ModifiersChanged(m));
            }
            for _ in 0..4 {
                h.step();
            }
        };
        shift_shift(&mut h);
        h.render().unwrap().save(out.join("palette.png")).unwrap();
        for (query, name) in [("re", "palette-query.png"), ("!", "palette-runs.png")] {
            h.event(egui::Event::Text(query.into()));
            for _ in 0..4 {
                h.step();
            }
            h.render().unwrap().save(out.join(name)).unwrap();
            h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
            h.key_press(egui::Key::Backspace);
            h.step();
        }
        h.key_press(egui::Key::Escape);
        h.step();
    }

    /// Offline: an approval card's "Allow once" sends Approve; the engine answers with
    /// ApprovalResolved and the card goes away.
    #[test]
    fn approval_card_buttons_send_commands() {
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        h.state().engine.send(Command::NewConversation);
        until(&mut h, 10, "bound", |a| a.focused_conversation().is_some());
        let conv = h.state().focused_conversation().unwrap();
        h.state_mut()
            .state
            .apply(void_core::engine::Event::ApprovalNeeded(
                void_core::permissions::ApprovalRequest {
                    id: 42,
                    conversation: conv,
                    tool: "bash".into(),
                    summary: "cargo test".into(),
                    command: Some("cargo test -p void-core".into()),
                    kind: Default::default(),
                },
            ));
        h.step();
        h.step();
        assert!(
            h.query_by_label_contains("Always allow `cargo test`")
                .is_some()
        );
        h.get_by_label_contains("Allow once").click();
        until(&mut h, 10, "approval resolved", |a| {
            a.state.approvals.is_empty()
        });
    }

    /// Offline: Claude Code's requests offer "Always allow" only when Claude Code suggested a
    /// permission update (never void's prefix rule); Allow once and Deny always.
    #[test]
    fn claude_approval_card_always_only_with_suggestions() {
        use void_core::permissions::{ApprovalKind, ApprovalRequest};
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        h.state().engine.send(Command::NewConversation);
        until(&mut h, 10, "bound", |a| a.focused_conversation().is_some());
        let conv = h.state().focused_conversation().unwrap();
        let req = |id, always_allow| ApprovalRequest {
            id,
            conversation: conv,
            tool: "Bash".into(),
            summary: "cargo test".into(),
            command: Some("cargo test -p void-core".into()),
            kind: ApprovalKind::ClaudeCode {
                always_allow,
                title: None,
                description: None,
                blocked_path: None,
            },
        };
        h.state_mut()
            .state
            .apply(void_core::engine::Event::ApprovalNeeded(req(43, false)));
        h.step();
        h.step();
        assert!(h.query_by_label_contains("Allow once").is_some());
        assert!(h.query_by_label_contains("Deny").is_some());
        assert!(h.query_by_label_contains("Always allow").is_none());
        h.get_by_label_contains("Deny").click();
        until(&mut h, 10, "approval resolved", |a| {
            a.state.approvals.is_empty()
        });
        h.state_mut()
            .state
            .apply(void_core::engine::Event::ApprovalNeeded(req(44, true)));
        h.step();
        h.step();
        assert!(
            h.query_by_label_contains("Always allow `cargo test`")
                .is_none()
        );
        h.get_by_label_contains("Always allow").click();
        until(&mut h, 10, "approval resolved", |a| {
            a.state.approvals.is_empty()
        });
    }

    /// A model that replies with the next scripted stream on each request.
    struct Scripted(
        std::sync::Mutex<std::collections::VecDeque<Vec<void_core::backend::StreamEvent>>>,
    );

    impl void_core::backend::Backend for Scripted {
        fn stream(&self, _req: void_core::backend::ChatRequest) -> void_core::backend::EventStream {
            use futures_util::StreamExt;
            let mut q = self.0.lock().unwrap();
            let s = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().cloned().unwrap_or_default()
            };
            futures_util::stream::iter(s.into_iter().map(Ok)).boxed()
        }
        fn list_models(
            &self,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<Vec<String>, void_core::backend::BackendError>,
        > {
            Box::pin(async { Ok(vec![]) })
        }
    }

    /// Offline: an agent write shows up in Review (opened automatically) and "Accept file"
    /// puts it in the real workspace.
    #[test]
    fn review_accept_file_updates_workspace() {
        use void_core::backend::{StopReason, StreamEvent};
        let p = paths_tmp();
        let proj = p.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let scripts = vec![
            vec![
                StreamEvent::ToolCall {
                    id: "w1".into(),
                    name: "write".into(),
                    arguments: r#"{"path":"notes.txt","content":"banana"}"#.into(),
                },
                StreamEvent::Done(StopReason::ToolUse),
            ],
            vec![
                StreamEvent::TextDelta("written".into()),
                StreamEvent::Done(StopReason::EndTurn),
            ],
        ];
        let backend: Arc<dyn void_core::backend::Backend> =
            Arc::new(Scripted(std::sync::Mutex::new(scripts.into())));
        let factory: void_core::engine::BackendFactory = Arc::new(move |_p, _k| backend.clone());
        let (cfg, data) = (p.root.join("config.toml"), p.root.join("data"));
        let open = Some(proj.clone());
        let mut h = Harness::builder()
            .with_size([1400.0, 860.0])
            .with_max_steps(10)
            .with_accessibility_check(false)
            .build_eframe(move |cc| VoidApp::with_factory(cc, open, cfg, data, Some(factory)));
        until(&mut h, 10, "proj", |a| a.state.workspace.name == "proj");
        type_and_send(&mut h, "write the notes");
        until(&mut h, 10, "changes", |a| {
            a.state.changes.values().any(|c| !c.is_empty())
        });
        until(&mut h, 10, "reply", |a| a.state.streams.is_empty());
        h.step();
        assert!(h.state().dock.find_tab(&Tab::Review).is_some());
        assert!(
            !proj.join("notes.txt").exists(),
            "nothing on disk before accepting"
        );
        h.get_by_role_and_label(Role::Button, "notes.txt").click();
        until(&mut h, 10, "diff", |a| !a.state.diffs.is_empty());
        h.step();
        h.get_by_label_contains("Accept file").click();
        until(&mut h, 10, "accepted", |_| proj.join("notes.txt").exists());
        assert_eq!(
            std::fs::read_to_string(proj.join("notes.txt")).unwrap(),
            "banana"
        );
    }

    /// Offline: accepting an agent's changes refreshes open buffers right away, without the
    /// file watcher (paused here): a clean buffer reloads, a dirty one gets the banner.
    #[test]
    fn review_accept_refreshes_open_buffers() {
        use void_core::backend::{StopReason, StreamEvent};
        let p = paths_tmp();
        let proj = p.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let proj = proj.canonicalize().unwrap();
        std::fs::write(proj.join("clean.txt"), "old\n").unwrap();
        std::fs::write(proj.join("dirty.txt"), "old\n").unwrap();
        let write = |id: &str, path: &str, content: &str| StreamEvent::ToolCall {
            id: id.into(),
            name: "write".into(),
            arguments: format!(r#"{{"path":"{path}","content":"{content}"}}"#),
        };
        let scripts = vec![
            vec![
                write("w1", "clean.txt", "banana"),
                write("w2", "dirty.txt", "kiwi"),
                StreamEvent::Done(StopReason::ToolUse),
            ],
            vec![
                StreamEvent::TextDelta("written".into()),
                StreamEvent::Done(StopReason::EndTurn),
            ],
        ];
        let backend: Arc<dyn void_core::backend::Backend> =
            Arc::new(Scripted(std::sync::Mutex::new(scripts.into())));
        let factory: void_core::engine::BackendFactory = Arc::new(move |_p, _k| backend.clone());
        let (cfg, data) = (p.root.join("config.toml"), p.root.join("data"));
        let open = Some(proj.clone());
        let mut h = Harness::builder()
            .with_size([1400.0, 860.0])
            .with_max_steps(10)
            .with_accessibility_check(false)
            .build_eframe(move |cc| VoidApp::with_factory(cc, open, cfg, data, Some(factory)));
        until(&mut h, 10, "proj", |a| a.state.workspace.name == "proj");
        h.state_mut().disk.pause();
        let clean = (proj.join("clean.txt"), EditorView::Disk);
        let dirty = (proj.join("dirty.txt"), EditorView::Disk);
        h.state_mut()
            .open_editor(clean.0.clone(), EditorView::Disk, None);
        h.state_mut()
            .open_editor(dirty.0.clone(), EditorView::Disk, None);
        h.state_mut()
            .editors
            .doc_mut(&dirty)
            .unwrap()
            .core
            .replace(0..0, "mine ");
        h.step();
        let chat = h
            .state()
            .dock
            .find_tab_from(|t| matches!(t, Tab::Chat { .. }))
            .unwrap();
        let _ = h.state_mut().dock.set_active_tab(chat);
        h.step();
        type_and_send(&mut h, "write both");
        until(&mut h, 10, "changes", |a| {
            a.state.changes.values().any(|c| c.len() == 2)
        });
        until(&mut h, 10, "reply", |a| a.state.streams.is_empty());
        let conv = h.state().focused_conversation().unwrap();
        h.state().engine.send(Command::AcceptAll {
            conversation: conv,
            keep_ignored: false,
        });
        until(&mut h, 10, "clean buffer reloaded", |a| {
            a.editors.doc(&clean).is_some_and(|d| d.text() == "banana")
        });
        until(&mut h, 10, "dirty buffer flagged", |a| {
            a.editors.doc(&dirty).is_some_and(|d| d.disk_changed())
        });
        assert_eq!(h.state().editors.doc(&dirty).unwrap().text(), "mine old");
        assert_eq!(std::fs::read_to_string(&dirty.0).unwrap(), "kiwi");
    }

    /// The Claude Code profile has no URL, limits or extra body: the editor shows only its
    /// command and model, while an API profile still shows the URL fields.
    #[test]
    fn backends_panel_hides_url_fields_for_claude_code() {
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Backends);
        let api = h.state().state.config.profiles[0].clone();
        h.state_mut().backends.select(api.clone());
        h.run();
        for l in ["Base URL", "Context limit", "Max output", "Command"] {
            assert_eq!(h.query_by_label(l).is_some(), l != "Command", "API: {l}");
        }
        assert!(
            h.query_by_label_contains("Extra request fields").is_some(),
            "API: extra body"
        );

        let cc = void_core::config::Profile {
            id: "cc".into(),
            name: "Claude Code".into(),
            kind: void_core::config::BackendKind::ClaudeCode,
            base_url: String::new(),
            ..api
        };
        h.state_mut().backends.select(cc);
        h.run();
        for l in ["Base URL", "Context limit", "Max output"] {
            assert!(h.query_by_label(l).is_none(), "Claude Code shows {l}");
        }
        assert!(
            h.query_by_label_contains("Extra request fields").is_none(),
            "Claude Code shows the extra body"
        );
        assert!(h.query_by_label("Command").is_some());
        assert!(h.query_by_label("Model").is_some());
    }

    /// A Claude Code chat's status bar: the model Claude Code reported at init (else "default
    /// model", never "Name · "), the turn's status while it lasts, and the context used with no
    /// made-up limit.
    #[test]
    fn status_shows_claude_model() {
        use void_core::engine::Event;
        let p = paths_tmp();
        let base = void_core::config::Config::default();
        let cc = void_core::config::Profile {
            id: "cc".into(),
            name: "Claude Code".into(),
            kind: void_core::config::BackendKind::ClaudeCode,
            base_url: String::new(),
            model: String::new(),
            ..base.profiles[0].clone()
        };
        let cfg = void_core::config::Config {
            active_profile: "cc".into(),
            profiles: vec![cc.clone()],
            ..base
        };
        void_core::config::save_config(&p.root.join("config.toml"), &cfg).unwrap();
        let mut h = harness(&p);
        until(&mut h, 10, "config", |a| {
            a.state.config.active_profile == "cc"
        });
        h.state().engine.send(Command::NewConversation);
        until(&mut h, 10, "bound", |a| a.focused_conversation().is_some());
        let conv = h.state().focused_conversation().unwrap();
        h.step();
        assert!(h.query_by_label("Claude Code · default model").is_some());
        assert!(h.query_by_label("Claude Code · ").is_none());
        let (label, hover) = profile_pill(&cc, None);
        assert_eq!(label, "Claude Code · default model");
        assert!(
            hover.contains("claude") && !hover.trim().is_empty(),
            "{hover}"
        );
        // No profile limit for Claude Code: tokens used only.
        use egui_kittest::kittest::NodeT;
        let ctx = h.get_by_label_contains("context ").accesskit_node().label();
        assert!(!ctx.unwrap_or_default().contains('/'), "a limit is shown");

        h.state_mut().state.apply(Event::ClaudeInit {
            conversation: conv,
            model: "claude-opus-4-5".into(),
        });
        h.state_mut().state.apply(Event::TurnStatus {
            conversation: conv,
            status: "Waiting for the sandbox — another chat's command is using it…".into(),
        });
        h.step();
        h.step();
        assert!(h.query_by_label("Claude Code · claude-opus-4-5").is_some());
        assert!(
            h.query_by_label_contains("Waiting for the sandbox")
                .is_some()
        );
        // The status lasts until the turn moves on.
        h.state_mut().state.apply(Event::StreamStarted(conv));
        h.state_mut().state.apply(Event::Delta {
            conversation: conv,
            kind: void_core::engine::DeltaKind::Text,
            text: "Hi".into(),
        });
        h.step();
        h.step();
        assert!(
            h.query_by_label_contains("Waiting for the sandbox")
                .is_none()
        );
        assert!(h.query_by_label("Claude Code · claude-opus-4-5").is_some());
    }

    /// A Test result from before the profile's kind changed isn't shown for the new kind.
    #[test]
    fn stale_test_result_cleared_on_kind_switch() {
        let p = paths_tmp();
        let mut h = harness(&p);
        until(&mut h, 10, "workspace", |a| {
            a.state.workspace.name == "Scratch"
        });
        layout::ensure_tab(&mut h.state_mut().dock, Tab::Backends);
        let api = h.state().state.config.profiles[0].clone();
        h.state_mut().backends.select(api.clone());
        h.state_mut().backends.tested(&api.id, api.kind);
        h.state_mut()
            .state
            .tests
            .insert(api.id.clone(), Ok(vec!["m1".into(), "m2".into()]));
        h.run();
        assert!(h.query_by_label_contains("Connected: 2 models").is_some());
        h.state_mut().backends.select(void_core::config::Profile {
            kind: void_core::config::BackendKind::ClaudeCode,
            ..api.clone()
        });
        h.run();
        assert!(h.query_by_label_contains("Found Claude Code").is_none());
        assert!(!h.state().state.tests.contains_key(&api.id));
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

    /// Live: the real model writes a file and cats it through the sandbox; the write waits in
    /// Review and lands on disk only after Accept all.
    #[test]
    #[ignore]
    fn e2e_agent_with_strata() {
        let p = paths();
        let proj = p.root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("hello.txt"), "pineapple\n").unwrap();
        let mut h = harness(&p);
        until(&mut h, 10, "start", |a| a.state.sandbox.is_some());
        println!("sandbox: {:?}", h.state().state.sandbox);
        h.state_mut().open_workspace(Some(proj.clone()));
        until(&mut h, 10, "proj", |a| a.state.workspace.name == "proj");
        type_and_send(
            &mut h,
            "Create notes.txt containing just the word banana using the write tool, then run `cat notes.txt` with the bash tool. Then stop.",
        );
        // Approve each bash call the model makes (and anything else it asks for).
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            h.step();
            if let Some(req) = h.state().state.approvals.first().cloned() {
                println!("approving {} {:?}", req.tool, req.command);
                h.state().engine.send(Command::Approve {
                    id: req.id,
                    decision: void_core::permissions::Decision::Once,
                });
            }
            let a = h.state();
            let busy = !a.state.streams.is_empty()
                || !a.state.approvals.is_empty()
                || a.state.tool_calls.values().any(|t| t.done.is_none());
            let bash_done = a.state.tool_calls.values().any(|t| {
                t.done
                    .as_ref()
                    .is_some_and(|o| o.content.contains("banana"))
            });
            if bash_done && !busy {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; tool calls: {:?}",
                a.state.tool_calls.keys().collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        for t in h.state().state.tool_calls.values() {
            println!(
                "tool: {} → {}",
                t.summary,
                t.done
                    .as_ref()
                    .map(|o| o.content.lines().take(3).collect::<Vec<_>>().join(" | "))
                    .unwrap_or_default()
            );
        }
        assert!(
            !proj.join("notes.txt").exists(),
            "nothing on disk before review"
        );
        let conv = h.state().focused_conversation().unwrap();
        until(&mut h, 10, "changes", |a| {
            a.state.changes.get(&conv).is_some_and(|c| {
                c.iter()
                    .any(|x| x.path == std::path::Path::new("notes.txt"))
            })
        });
        assert!(h.state().dock.find_tab(&Tab::Review).is_some());
        h.step();
        h.get_by_label_contains("Accept all").click();
        until(&mut h, 10, "accepted", |_| proj.join("notes.txt").exists());
        assert_eq!(
            std::fs::read_to_string(proj.join("notes.txt"))
                .unwrap()
                .trim(),
            "banana"
        );
        assert_eq!(
            std::fs::read_to_string(proj.join("hello.txt")).unwrap(),
            "pineapple\n"
        );
    }
}

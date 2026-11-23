//! The Runs panel: moon tasks, the workspace's run configs in their groups (with an editor),
//! and environment profiles. The engine owns `runs.toml` / `env.toml`: every change here is
//! sent as a whole (`SaveRuns`, `SaveEnvs`, `SetActiveEnv`) and comes back as `RunsLoaded`.
//! Literal environment values are masked unless revealed, and never logged: the drafts here
//! have no `Debug`.

use std::collections::{BTreeMap, HashSet};

use egui::{CollapsingHeader, Id, RichText, TextEdit, Ui};
use void_core::engine::Command;
use void_core::runner::{
    EnvProfile, EnvValue, Envs, Group, MoonTask, OnFailure, RunConfig, RunId, Runs, Step,
    StepConfig,
};

use super::{Action, Panels};
use crate::state::UiState;
use crate::theme::{self, Palette, icon};

#[cfg(test)]
mod tests;

/// Shown with a config's steps: what happens to processes a step leaves behind.
pub const LEFTOVER_HINT: &str = "When a step ends, anything it left running is stopped. A step \
     that starts a long-lived server (a dev server, a watcher) should be the last step, or a \
     config of its own.";

/// The panel's own state: drafts being edited and what's revealed.
#[derive(Default)]
pub struct RunsUi {
    /// The config open in the editor (a draft; Save sends it).
    edit: Option<ConfigDraft>,
    /// The environment profiles being edited, and the saved profiles they started from.
    envs: Option<(Vec<EnvProfile>, EnvsDraft)>,
    /// Literal values shown in clear: (profile, variable) positions in the draft.
    revealed: HashSet<(usize, usize)>,
    /// What saving the profiles last refused.
    envs_error: Option<String>,
    rename: Option<Rename>,
    /// The rename field still needs focus.
    rename_focus: bool,
    /// Open the Environments section (the env pill's "Edit environments…").
    pub show_envs: bool,
}

impl RunsUi {
    /// Open config `id` in the editor.
    pub fn edit_config(&mut self, runs: &Runs, id: &str) {
        if let Some((g, i)) = find_config(runs, id) {
            self.edit = Some(ConfigDraft::new(&runs.groups[g].configs[i]));
        }
    }
}

struct ConfigDraft {
    cfg: RunConfig,
    /// Each step's arguments as typed (split on whitespace when saved).
    args: Vec<String>,
}

impl ConfigDraft {
    fn new(cfg: &RunConfig) -> Self {
        let args = cfg
            .steps
            .iter()
            .map(|s| match &s.step {
                Step::Moon { args, .. } => args.join(" "),
                Step::Shell { .. } => String::new(),
            })
            .collect();
        Self {
            cfg: cfg.clone(),
            args,
        }
    }

    fn to_config(&self) -> RunConfig {
        let mut cfg = self.cfg.clone();
        for (s, typed) in cfg.steps.iter_mut().zip(&self.args) {
            if let Step::Moon { args, .. } = &mut s.step {
                *args = typed.split_whitespace().map(str::to_string).collect();
            }
        }
        cfg
    }
}

#[derive(Clone, PartialEq)]
enum Rename {
    Group(usize, String),
    Config(String, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Value,
    Env,
    Cmd,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Value => "value",
            Kind::Env => "env var",
            Kind::Cmd => "command",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Kind::Value => "A literal value (kept in env.toml, readable only by you)",
            Kind::Env => "Copied from this variable in void's own environment",
            Kind::Cmd => "The output of this command (bash, outside any sandbox, 10 s at most)",
        }
    }
}

#[derive(Clone, PartialEq)]
struct VarDraft {
    name: String,
    kind: Kind,
    value: String,
}

#[derive(Clone, PartialEq)]
struct ProfileDraft {
    name: String,
    /// Its saved name (None: new).
    was: Option<String>,
    vars: Vec<VarDraft>,
}

#[derive(Clone, PartialEq)]
struct EnvsDraft {
    profiles: Vec<ProfileDraft>,
}

impl EnvsDraft {
    fn new(profiles: &[EnvProfile]) -> Self {
        let profiles = profiles
            .iter()
            .map(|p| ProfileDraft {
                name: p.name.clone(),
                was: Some(p.name.clone()),
                vars: p
                    .vars
                    .iter()
                    .map(|(name, v)| {
                        let (kind, value) = match v {
                            EnvValue::Value(s) => (Kind::Value, s.clone()),
                            EnvValue::Env(s) => (Kind::Env, s.clone()),
                            EnvValue::Cmd(s) => (Kind::Cmd, s.clone()),
                        };
                        VarDraft {
                            name: name.clone(),
                            kind,
                            value,
                        }
                    })
                    .collect(),
            })
            .collect();
        Self { profiles }
    }

    /// The same names and values (whatever the profiles were called when saved).
    fn same(&self, other: &EnvsDraft) -> bool {
        let plain = |d: &EnvsDraft| {
            let mut d = d.clone();
            for p in &mut d.profiles {
                p.was = None;
            }
            d
        };
        plain(self) == plain(other)
    }

    /// Saving this draft would keep exactly `profiles`: it is them as typed, or once
    /// normalised (trimmed names, variables in order) as a save stores them.
    fn matches(&self, profiles: &[EnvProfile]) -> bool {
        self.same(&EnvsDraft::new(profiles))
            || self
                .to_envs(&[], None)
                .is_ok_and(|(envs, _)| envs.profiles == profiles)
    }

    /// The profiles to save (with `active` following a rename, or cleared when its profile
    /// went), and each of the `saved` profiles' new name when it changed (None: deleted). Errors never quote values.
    #[allow(clippy::type_complexity)]
    fn to_envs(
        &self,
        saved: &[EnvProfile],
        active: Option<&str>,
    ) -> Result<(Envs, Vec<(String, Option<String>)>), String> {
        let mut names = HashSet::new();
        let mut profiles = Vec::new();
        for p in &self.profiles {
            let name = p.name.trim().to_string();
            if name.is_empty() {
                return Err("Every environment profile needs a name.".into());
            }
            if !names.insert(name.clone()) {
                return Err(format!("There are two profiles called '{name}'."));
            }
            let mut vars = BTreeMap::new();
            for v in &p.vars {
                let var = v.name.trim().to_string();
                if !valid_var_name(&var) {
                    return Err(format!(
                        "'{var}' in profile '{name}' isn't a valid variable name (letters, digits \
                         and _, not starting with a digit)."
                    ));
                }
                let value = match v.kind {
                    Kind::Value => EnvValue::Value(v.value.clone()),
                    Kind::Env => EnvValue::Env(v.value.trim().to_string()),
                    Kind::Cmd => EnvValue::Cmd(v.value.clone()),
                };
                if vars.insert(var.clone(), value).is_some() {
                    return Err(format!("Profile '{name}' sets {var} twice."));
                }
            }
            profiles.push(EnvProfile { name, vars });
        }
        let renamed_to = |old: &str| {
            self.profiles
                .iter()
                .find(|p| p.was.as_deref() == Some(old))
                .map(|p| p.name.trim().to_string())
        };
        let mut renames = Vec::new();
        for old in saved.iter().map(|p| p.name.clone()) {
            let new = renamed_to(&old);
            if new.as_deref() != Some(old.as_str()) {
                renames.push((old, new));
            }
        }
        let active = active.and_then(renamed_to);
        Ok((Envs { active, profiles }, renames))
    }
}

/// A shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn valid_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The payload of a config being dragged: its id.
struct DragConfig(String);

/// What the panel's widgets asked for, applied once it's drawn.
enum Do {
    Send(Command),
    Show(RunId),
    DismissWarning,
    Edit(String),
    NewConfig {
        group: Option<usize>,
        name: String,
        steps: Vec<StepConfig>,
    },
    AddStep {
        config: String,
        step: StepConfig,
    },
    Move {
        id: String,
        to: usize,
        index: Option<usize>,
    },
    NewGroup,
    StartRename(Rename),
    CancelRename,
    RenameGroup(usize, String),
    RenameConfig(String, String),
    DeleteGroup(usize),
    DeleteConfig(String),
    SaveEdit {
        run: bool,
    },
    CloseEdit,
    SaveEnvs,
    RevertEnvs,
}

fn moon_step(target: &str) -> StepConfig {
    StepConfig {
        step: Step::Moon {
            target: target.to_string(),
            args: Vec::new(),
        },
        on_failure: OnFailure::Stop,
    }
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    let pal = theme::palette_of(ui);
    let mut todo = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("runs-panel")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::heading(ui, icon::PLAY, "Runs");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::icon_button(ui, icon::ARROW_CLOCKWISE, "Look for moon tasks again")
                        .clicked()
                    {
                        todo.push(Do::Send(Command::RefreshTasks));
                    }
                    if theme::icon_button(ui, icon::PLAY, "Re-run last (Ctrl+Shift+F10)").clicked()
                    {
                        todo.push(Do::Send(Command::RerunLast));
                    }
                });
            });
            if p.state.workspace.root.is_none() {
                ui.label(
                    RichText::new("Open a folder to run its tasks.")
                        .color(pal.text_faint)
                        .small(),
                );
                return;
            }
            if let Some(w) = &p.state.runs_warning {
                warning(ui, &pal, w, &mut todo);
            }
            running(ui, &pal, p.state, &mut todo);
            tasks(ui, &pal, p.state, &mut todo);
            configs(ui, &pal, p.state, p.runs, &mut todo);
            if p.runs.edit.is_some() {
                ui.add_space(4.0);
                editor(ui, &pal, p.state, p.runs, &mut todo);
            }
            ui.add_space(4.0);
            environments(ui, &pal, p.state, p.runs, &mut todo);
        });
    for d in todo {
        apply(p, d);
    }
}

fn section(text: &str) -> RichText {
    RichText::new(text).family(egui::FontFamily::Name(theme::BOLD.into()))
}

/// The ui's small text size (not the process-wide one: tests run several apps at once).
fn small(ui: &Ui) -> f32 {
    egui::TextStyle::Small.resolve(ui.style()).size
}

fn faint(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.label(RichText::new(text).color(pal.text_faint).small());
}

fn warning(ui: &mut Ui, pal: &Palette, text: &str, todo: &mut Vec<Do>) {
    egui::Frame::new()
        .fill(theme::with_alpha(pal.warn, 28))
        .stroke(egui::Stroke::new(1.0, theme::with_alpha(pal.warn, 90)))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(theme::ico(icon::WARNING).color(pal.warn));
                let width = (ui.available_width() - 28.0).max(40.0);
                ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).small().color(pal.text)).wrap());
                });
                if theme::icon_button(ui, icon::X_CIRCLE, "Dismiss").clicked() {
                    todo.push(Do::DismissWarning);
                }
            });
        });
    ui.add_space(4.0);
}

fn running(ui: &mut Ui, pal: &Palette, state: &UiState, todo: &mut Vec<Do>) {
    let active: Vec<(RunId, String, String)> = state
        .active_runs()
        .map(|(id, v)| (id, v.name.clone(), v.status_text()))
        .collect();
    if active.is_empty() {
        return;
    }
    CollapsingHeader::new(section(&format!("Running · {}", active.len())))
        .id_salt("runs-running")
        .default_open(true)
        .show(ui, |ui| {
            for (id, name, status) in active {
                ui.horizontal(|ui| {
                    ui.label(theme::ico(icon::PLAY).color(pal.accent2));
                    if ui
                        .selectable_label(false, &name)
                        .on_hover_text("Show its output")
                        .clicked()
                    {
                        todo.push(Do::Show(id));
                    }
                    ui.label(RichText::new(status).small().color(pal.text_dim));
                    if theme::icon_button(ui, icon::STOP, "Stop").clicked() {
                        todo.push(Do::Send(Command::StopRun(id)));
                    }
                });
            }
        });
}

fn tasks(ui: &mut Ui, pal: &Palette, state: &UiState, todo: &mut Vec<Do>) {
    CollapsingHeader::new(section(&format!("Tasks · {}", state.tasks.len())))
        .id_salt("runs-tasks")
        .default_open(true)
        .show(ui, |ui| {
            if state.tasks.is_empty() {
                faint(ui, pal, "No moon tasks here.");
                return;
            }
            let mut by_project: BTreeMap<&str, Vec<&MoonTask>> = BTreeMap::new();
            for t in &state.tasks {
                by_project.entry(&t.project).or_default().push(t);
            }
            for (project, list) in by_project {
                CollapsingHeader::new(theme::itext(icon::FOLDER, project))
                    .id_salt(("runs-project", project))
                    .default_open(false)
                    .show(ui, |ui| {
                        for t in list {
                            task_row(ui, pal, &state.runs, t, todo);
                        }
                    });
            }
        });
}

fn task_row(ui: &mut Ui, pal: &Palette, runs: &Runs, t: &MoonTask, todo: &mut Vec<Do>) {
    let mut hover = t.command.clone();
    if let Some(d) = &t.description {
        hover = format!("{d}\n{hover}");
    }
    hover.push_str("\nDouble-click to run");
    let r = ui
        .selectable_label(false, theme::itext(icon::TERMINAL, &t.id))
        .on_hover_text(hover);
    let run = || {
        Do::Send(Command::RunTask {
            target: t.target.clone(),
            args: Vec::new(),
        })
    };
    if r.double_clicked() {
        todo.push(run());
    }
    r.context_menu(|ui| {
        if ui.button(theme::itext(icon::PLAY, "Run")).clicked() {
            todo.push(run());
            ui.close();
        }
        ui.menu_button(theme::itext(icon::PLUS, "Add to run config…"), |ui| {
            for g in &runs.groups {
                if g.configs.is_empty() {
                    continue;
                }
                ui.label(RichText::new(&g.name).small().color(pal.text_faint));
                for c in &g.configs {
                    if ui.button(&c.name).clicked() {
                        todo.push(Do::AddStep {
                            config: c.id.clone(),
                            step: moon_step(&t.target),
                        });
                        ui.close();
                    }
                }
            }
            if runs.groups.iter().any(|g| !g.configs.is_empty()) {
                ui.separator();
            }
            if ui.button(theme::itext(icon::PLUS, "New config")).clicked() {
                todo.push(Do::NewConfig {
                    group: None,
                    name: t.target.clone(),
                    steps: vec![moon_step(&t.target)],
                });
                ui.close();
            }
        });
    });
}

/// A name field for a rename: Some(true) to commit, Some(false) to cancel.
fn rename_field(ui: &mut Ui, text: &mut String, focus: &mut bool) -> Option<bool> {
    let r = ui.add(TextEdit::singleline(text).desired_width(ui.available_width().min(220.0)));
    if std::mem::take(focus) {
        r.request_focus();
    }
    if r.lost_focus() {
        return Some(!ui.input(|i| i.key_pressed(egui::Key::Escape)));
    }
    None
}

fn configs(ui: &mut Ui, pal: &Palette, state: &UiState, runs_ui: &mut RunsUi, todo: &mut Vec<Do>) {
    CollapsingHeader::new(section("My run configs"))
        .id_salt("runs-configs")
        .default_open(true)
        .show(ui, |ui| {
            if state.runs.groups.is_empty() {
                faint(
                    ui,
                    pal,
                    "No run configs yet: make one, or add a task to one from its menu.",
                );
            }
            let editing = runs_ui.edit.as_ref().map(|d| d.cfg.id.clone());
            for (g, group) in state.runs.groups.iter().enumerate() {
                if let Some(Rename::Group(rg, text)) = &mut runs_ui.rename
                    && *rg == g
                {
                    match rename_field(ui, text, &mut runs_ui.rename_focus) {
                        Some(true) => todo.push(Do::RenameGroup(g, text.clone())),
                        Some(false) => todo.push(Do::CancelRename),
                        None => {}
                    }
                    continue;
                }
                let resp =
                    CollapsingHeader::new(format!("{} · {}", group.name, group.configs.len()))
                        .id_salt(("runs-group", g))
                        .default_open(true)
                        .show(ui, |ui| {
                            let (_, dropped) = ui.dnd_drop_zone::<DragConfig, ()>(
                                egui::Frame::new()
                                    .fill(pal.bg_panel)
                                    .inner_margin(egui::Margin::same(2)),
                                |ui| {
                                    ui.set_width(ui.available_width());
                                    for (i, c) in group.configs.iter().enumerate() {
                                        config_row(
                                            ui,
                                            pal,
                                            g,
                                            i,
                                            c,
                                            editing.as_deref(),
                                            runs_ui,
                                            todo,
                                        );
                                    }
                                    if group.configs.is_empty() {
                                        faint(ui, pal, "Empty: drag configs here.");
                                    }
                                },
                            );
                            if let Some(d) = dropped {
                                todo.push(Do::Move {
                                    id: d.0.clone(),
                                    to: g,
                                    index: None,
                                });
                            }
                        });
                if let Some(d) = resp.header_response.dnd_release_payload::<DragConfig>() {
                    todo.push(Do::Move {
                        id: d.0.clone(),
                        to: g,
                        index: None,
                    });
                }
                resp.header_response.context_menu(|ui| {
                    if ui.button(theme::itext(icon::PLUS, "New config")).clicked() {
                        todo.push(Do::NewConfig {
                            group: Some(g),
                            name: "New config".into(),
                            steps: Vec::new(),
                        });
                        ui.close();
                    }
                    if ui.button(theme::itext(icon::PENCIL, "Rename")).clicked() {
                        todo.push(Do::StartRename(Rename::Group(g, group.name.clone())));
                        ui.close();
                    }
                    let label = match group.configs.len() {
                        0 => "Delete group".to_string(),
                        1 => "Delete group and its config".to_string(),
                        n => format!("Delete group and its {n} configs"),
                    };
                    if ui.button(theme::itext(icon::TRASH, &label)).clicked() {
                        todo.push(Do::DeleteGroup(g));
                        ui.close();
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.button(theme::itext(icon::PLUS, "New config")).clicked() {
                    todo.push(Do::NewConfig {
                        group: None,
                        name: "New config".into(),
                        steps: Vec::new(),
                    });
                }
                if ui
                    .button(theme::itext(icon::FOLDER_PLUS, "New group"))
                    .clicked()
                {
                    todo.push(Do::NewGroup);
                }
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn config_row(
    ui: &mut Ui,
    pal: &Palette,
    g: usize,
    i: usize,
    c: &RunConfig,
    editing: Option<&str>,
    runs_ui: &mut RunsUi,
    todo: &mut Vec<Do>,
) {
    ui.horizontal(|ui| {
        ui.dnd_drag_source(
            Id::unique(("runs-drag", &c.id)),
            DragConfig(c.id.clone()),
            |ui| {
                ui.label(theme::ico(icon::LIST).color(pal.text_faint));
            },
        )
        .response
        .on_hover_text("Drag to move it to another group");
        if let Some(Rename::Config(id, text)) = &mut runs_ui.rename
            && *id == c.id
        {
            match rename_field(ui, text, &mut runs_ui.rename_focus) {
                Some(true) => todo.push(Do::RenameConfig(c.id.clone(), text.clone())),
                Some(false) => todo.push(Do::CancelRename),
                None => {}
            }
            return;
        }
        let run = || {
            Do::Send(Command::RunConfig {
                config_id: c.id.clone(),
            })
        };
        if theme::icon_button(ui, icon::PLAY, "Run").clicked() {
            todo.push(run());
        }
        let r = ui
            .selectable_label(editing == Some(c.id.as_str()), &c.name)
            .on_hover_text(match c.steps.len() {
                1 => "1 step · click to edit, double-click to run".to_string(),
                n => format!("{n} steps · click to edit, double-click to run"),
            });
        if r.double_clicked() {
            todo.push(run());
        } else if r.clicked() {
            todo.push(Do::Edit(c.id.clone()));
        }
        if let Some(d) = r.dnd_release_payload::<DragConfig>() {
            todo.push(Do::Move {
                id: d.0.clone(),
                to: g,
                index: Some(i),
            });
        }
        r.context_menu(|ui| {
            if ui.button(theme::itext(icon::PLAY, "Run")).clicked() {
                todo.push(run());
                ui.close();
            }
            if ui.button(theme::itext(icon::PENCIL, "Edit")).clicked() {
                todo.push(Do::Edit(c.id.clone()));
                ui.close();
            }
            if ui.button(theme::itext(icon::PENCIL, "Rename")).clicked() {
                todo.push(Do::StartRename(Rename::Config(
                    c.id.clone(),
                    c.name.clone(),
                )));
                ui.close();
            }
            if ui.button(theme::itext(icon::TRASH, "Delete")).clicked() {
                todo.push(Do::DeleteConfig(c.id.clone()));
                ui.close();
            }
        });
    });
}

fn editor(ui: &mut Ui, pal: &Palette, state: &UiState, runs_ui: &mut RunsUi, todo: &mut Vec<Do>) {
    let Some(d) = &mut runs_ui.edit else {
        return;
    };
    let saved = find_config(&state.runs, &d.cfg.id).map(|(g, i)| &state.runs.groups[g].configs[i]);
    let dirty = saved != Some(&d.to_config());
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(section("Run config").color(pal.text));
            if dirty {
                ui.label(RichText::new("unsaved").small().color(pal.warn));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icon::X_CIRCLE, "Close the editor").clicked() {
                    todo.push(Do::CloseEdit);
                }
            });
        });
        egui::Grid::new("runs-edit-grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Name");
                ui.add(TextEdit::singleline(&mut d.cfg.name).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Environment");
                let missing = d
                    .cfg
                    .env_profile
                    .clone()
                    .filter(|p| !state.envs.profiles.iter().any(|e| e.name == *p));
                let shown = match (&d.cfg.env_profile, &missing) {
                    (_, Some(p)) => RichText::new(format!("{p} (missing)")).color(pal.warn),
                    (Some(p), None) => RichText::new(p),
                    (None, None) => RichText::new("the active profile"),
                };
                egui::ComboBox::from_id_salt("runs-edit-env")
                    .selected_text(shown)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.cfg.env_profile, None, "the active profile");
                        for p in &state.envs.profiles {
                            ui.selectable_value(
                                &mut d.cfg.env_profile,
                                Some(p.name.clone()),
                                &p.name,
                            );
                        }
                    });
                ui.end_row();
                if let Some(p) = missing {
                    ui.label("");
                    ui.label(
                        RichText::new(format!(
                            "Profile '{p}' was deleted: this config won't run until you pick \
                             another."
                        ))
                        .color(pal.warn)
                        .small(),
                    );
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
        ui.label(RichText::new("Steps").color(pal.text_dim).small())
            .on_hover_text(LEFTOVER_HINT);
        let mut remove = None;
        let mut swap = None;
        let n = d.cfg.steps.len();
        for i in 0..n {
            let ConfigDraft { cfg, args } = &mut *d;
            step_row(
                ui,
                pal,
                &state.tasks,
                i,
                n,
                &mut cfg.steps[i],
                &mut args[i],
                &mut remove,
                &mut swap,
            );
        }
        if let Some(i) = remove {
            d.cfg.steps.remove(i);
            d.args.remove(i);
        }
        if let Some((a, b)) = swap {
            d.cfg.steps.swap(a, b);
            d.args.swap(a, b);
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button(theme::itext(icon::PLUS, "moon step")).clicked() {
                d.cfg.steps.push(moon_step(""));
                d.args.push(String::new());
            }
            if ui.button(theme::itext(icon::PLUS, "shell step")).clicked() {
                d.cfg.steps.push(StepConfig {
                    step: Step::Shell {
                        command: String::new(),
                    },
                    on_failure: OnFailure::Stop,
                });
                d.args.push(String::new());
            }
        });
        ui.add(
            egui::Label::new(theme::itext_styled(
                icon::WARNING,
                LEFTOVER_HINT,
                small(ui),
                pal.text_faint,
            ))
            .wrap(),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(dirty, egui::Button::new(theme::itext(icon::FLOPPY, "Save")))
                .clicked()
            {
                todo.push(Do::SaveEdit { run: false });
            }
            let label = if dirty { "Save and run" } else { "Run" };
            if theme::primary_button(ui, icon::PLAY, label).clicked() {
                todo.push(Do::SaveEdit { run: true });
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn step_row(
    ui: &mut Ui,
    pal: &Palette,
    tasks: &[MoonTask],
    i: usize,
    n: usize,
    step: &mut StepConfig,
    args: &mut String,
    remove: &mut Option<usize>,
    swap: &mut Option<(usize, usize)>,
) {
    let mono = egui::TextStyle::Monospace;
    egui::Frame::new()
        .fill(pal.bg_panel)
        .stroke(egui::Stroke::new(1.0, pal.border))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).color(pal.text_faint));
                let is_moon = matches!(step.step, Step::Moon { .. });
                if ui.selectable_label(is_moon, "moon").clicked() && !is_moon {
                    step.step = Step::Moon {
                        target: String::new(),
                        args: Vec::new(),
                    };
                }
                if ui.selectable_label(!is_moon, "shell").clicked() && is_moon {
                    step.step = Step::Shell {
                        command: String::new(),
                    };
                }
                match &mut step.step {
                    Step::Moon { target, .. } => {
                        ui.add(
                            TextEdit::singleline(target)
                                .hint_text("project:task")
                                .font(mono.clone())
                                .desired_width(170.0),
                        );
                        ui.menu_button(theme::ico(icon::CARET_DOWN), |ui| {
                            if tasks.is_empty() {
                                ui.label("No moon tasks found");
                            }
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .show(ui, |ui| {
                                    for t in tasks {
                                        if ui.button(&t.target).on_hover_text(&t.command).clicked()
                                        {
                                            *target = t.target.clone();
                                            ui.close();
                                        }
                                    }
                                });
                        })
                        .response
                        .on_hover_text("Pick one of the workspace's moon tasks");
                        ui.add(
                            TextEdit::singleline(args)
                                .hint_text("args")
                                .font(mono.clone())
                                .desired_width(120.0),
                        );
                    }
                    Step::Shell { command } => {
                        ui.add(
                            TextEdit::singleline(command)
                                .hint_text("shell command")
                                .font(mono.clone())
                                .desired_width(260.0),
                        );
                    }
                }
                ui.label(RichText::new("on failure").small().color(pal.text_faint));
                ui.selectable_value(&mut step.on_failure, OnFailure::Stop, "stop")
                    .on_hover_text("A failure ends the run here");
                ui.selectable_value(&mut step.on_failure, OnFailure::Continue, "continue")
                    .on_hover_text("Go on with the next step even if this one fails");
                if i > 0 && theme::icon_button(ui, icon::CARET_UP, "Move up").clicked() {
                    *swap = Some((i - 1, i));
                }
                if i + 1 < n && theme::icon_button(ui, icon::CARET_DOWN, "Move down").clicked() {
                    *swap = Some((i, i + 1));
                }
                if theme::icon_button(ui, icon::TRASH, "Remove this step").clicked() {
                    *remove = Some(i);
                }
            });
        })
        .response
        .on_hover_text(LEFTOVER_HINT);
}

fn environments(
    ui: &mut Ui,
    pal: &Palette,
    state: &UiState,
    runs_ui: &mut RunsUi,
    todo: &mut Vec<Do>,
) {
    let mut header = CollapsingHeader::new(section("Environments"))
        .id_salt("runs-envs")
        .default_open(false);
    if std::mem::take(&mut runs_ui.show_envs) {
        header = header.open(Some(true));
    }
    header.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Active").small().color(pal.text_dim));
            let active = state.envs.active.as_deref();
            if ui.radio(active.is_none(), "none").clicked() && active.is_some() {
                todo.push(Do::Send(Command::SetActiveEnv(None)));
            }
            for p in &state.envs.profiles {
                if ui.radio(active == Some(p.name.as_str()), &p.name).clicked()
                    && active != Some(p.name.as_str())
                {
                    todo.push(Do::Send(Command::SetActiveEnv(Some(p.name.clone()))));
                }
            }
        });
        ui.add_space(4.0);
        // A draft of the profiles: rebuilt from what's saved unless it holds unsaved edits.
        let saved = &state.envs.profiles;
        let stale = runs_ui.envs.as_ref().is_some_and(|(base, draft)| {
            base != saved && (draft.matches(base) || draft.matches(saved))
        });
        if runs_ui.envs.is_none() || stale {
            runs_ui.envs = Some((saved.clone(), EnvsDraft::new(saved)));
            runs_ui.revealed.clear();
        }
        let Some((base, draft)) = &mut runs_ui.envs else {
            return;
        };
        let mut remove_profile = None;
        for (pi, prof) in draft.profiles.iter_mut().enumerate() {
            if profile_card(ui, pal, pi, prof, &mut runs_ui.revealed) {
                remove_profile = Some(pi);
            }
            ui.add_space(4.0);
        }
        if let Some(pi) = remove_profile {
            draft.profiles.remove(pi);
            runs_ui.revealed.clear();
        }
        let dirty = !draft.matches(saved);
        ui.horizontal_wrapped(|ui| {
            if ui.button(theme::itext(icon::PLUS, "Profile")).clicked() {
                draft.profiles.push(ProfileDraft {
                    name: String::new(),
                    was: None,
                    vars: Vec::new(),
                });
            }
            if ui
                .add_enabled(dirty, egui::Button::new(theme::itext(icon::FLOPPY, "Save")))
                .clicked()
            {
                todo.push(Do::SaveEnvs);
            }
            if ui
                .add_enabled(
                    dirty,
                    egui::Button::new(theme::itext(icon::ARROW_CLOCKWISE, "Revert")),
                )
                .clicked()
            {
                todo.push(Do::RevertEnvs);
            }
            if base != saved && dirty {
                ui.label(
                    RichText::new("Changed elsewhere since you started editing")
                        .small()
                        .color(pal.warn),
                );
            }
        });
        if let Some(e) = &runs_ui.envs_error {
            ui.label(theme::itext_styled(icon::WARNING, e, small(ui), pal.err));
        }
        faint(
            ui,
            pal,
            "Values are kept in this workspace's env.toml (readable only by you) and passed to \
             runs only: never to the agent, logs or chats.",
        );
    });
}

/// One profile's card. True when its delete button was pressed.
fn profile_card(
    ui: &mut Ui,
    pal: &Palette,
    pi: usize,
    prof: &mut ProfileDraft,
    revealed: &mut HashSet<(usize, usize)>,
) -> bool {
    let mut delete = false;
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(theme::ico(icon::GLOBE).color(pal.accent2));
            ui.add(
                TextEdit::singleline(&mut prof.name)
                    .hint_text("profile name")
                    .desired_width(160.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icon::TRASH, "Delete this profile").clicked() {
                    delete = true;
                }
            });
        });
        let mut remove = None;
        for (vi, v) in prof.vars.iter_mut().enumerate() {
            // One row per variable, sized to the panel: name, source, value, eye, remove.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let icon_w = ui.spacing().interact_size.y + 4.0;
                let width = ui.available_width();
                let name_w = (width * 0.28).clamp(60.0, 180.0);
                let kind_w = 96.0;
                let value_w = (width - name_w - kind_w - 2.0 * icon_w - 24.0).max(60.0);
                ui.add(
                    TextEdit::singleline(&mut v.name)
                        .hint_text("NAME")
                        .font(egui::TextStyle::Monospace)
                        .desired_width(name_w),
                );
                egui::ComboBox::from_id_salt(("runs-env-kind", pi, vi))
                    .selected_text(v.kind.label())
                    .width(kind_w)
                    .show_ui(ui, |ui| {
                        for k in [Kind::Value, Kind::Env, Kind::Cmd] {
                            ui.selectable_value(&mut v.kind, k, k.label())
                                .on_hover_text(k.help());
                        }
                    });
                let shown = revealed.contains(&(pi, vi));
                let (hint, masked) = match v.kind {
                    Kind::Value => ("value", !shown),
                    Kind::Env => ("VARIABLE", false),
                    Kind::Cmd => ("command", false),
                };
                ui.add(
                    TextEdit::singleline(&mut v.value)
                        .hint_text(hint)
                        .password(masked)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(value_w),
                )
                .on_hover_text(v.kind.help());
                if v.kind == Kind::Value {
                    let (glyph, tip) = if shown {
                        (icon::EYE_SLASH, "Hide the value")
                    } else {
                        (icon::EYE, "Show the value")
                    };
                    if theme::icon_button(ui, glyph, tip).clicked() {
                        if shown {
                            revealed.remove(&(pi, vi));
                        } else {
                            revealed.insert((pi, vi));
                        }
                    }
                } else {
                    // Keep the rows' remove buttons in line.
                    ui.add_visible(
                        false,
                        egui::Button::new(theme::ico(icon::EYE).size(15.0)).frame(false),
                    );
                }
                if theme::icon_button(ui, icon::TRASH, "Remove this variable").clicked() {
                    remove = Some(vi);
                }
            });
        }
        if let Some(vi) = remove {
            prof.vars.remove(vi);
            revealed.retain(|(p, _)| *p != pi);
        }
        if ui.button(theme::itext(icon::PLUS, "Variable")).clicked() {
            prof.vars.push(VarDraft {
                name: String::new(),
                kind: Kind::Value,
                value: String::new(),
            });
        }
    });
    delete
}

/// Persist `runs`. The panel shows it once the engine's `RunsLoaded` says it's kept (a
/// refused save answers with what is).
fn save_runs(p: &mut Panels, runs: Runs) {
    p.engine.send(Command::SaveRuns(runs));
}

/// "New group", "New group 2", … whichever is free.
fn free_group_name(runs: &Runs) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                "New group".to_string()
            } else {
                format!("New group {n}")
            }
        })
        .find(|name| !runs.groups.iter().any(|g| g.name == *name))
        .expect("unbounded")
}

fn apply(p: &mut Panels, d: Do) {
    match d {
        Do::Send(c) => p.engine.send(c),
        Do::Show(run) => p.actions.push(Action::ShowRun(run)),
        Do::DismissWarning => p.state.runs_warning = None,
        Do::Edit(id) => p.runs.edit_config(&p.state.runs, &id),
        Do::CloseEdit => p.runs.edit = None,
        Do::NewConfig { group, name, steps } => {
            let mut runs = p.state.runs.clone();
            if runs.groups.is_empty() {
                runs.groups.push(Group {
                    name: "Configs".into(),
                    configs: Vec::new(),
                });
            }
            let g = group.unwrap_or(0).min(runs.groups.len() - 1);
            let cfg = RunConfig::new(name, steps);
            p.runs.edit = Some(ConfigDraft::new(&cfg));
            runs.groups[g].configs.push(cfg);
            save_runs(p, runs);
        }
        Do::AddStep { config, step } => {
            let mut runs = p.state.runs.clone();
            if let Some((g, i)) = find_config(&runs, &config) {
                runs.groups[g].configs[i].steps.push(step);
                let cfg = runs.groups[g].configs[i].clone();
                save_runs(p, runs);
                p.runs.edit = Some(ConfigDraft::new(&cfg));
            }
        }
        Do::Move { id, to, index } => {
            let mut runs = p.state.runs.clone();
            if move_config(&mut runs, &id, to, index) && runs != p.state.runs {
                save_runs(p, runs);
            }
        }
        Do::NewGroup => {
            let mut runs = p.state.runs.clone();
            let name = free_group_name(&runs);
            runs.groups.push(Group {
                name: name.clone(),
                configs: Vec::new(),
            });
            p.runs.rename = Some(Rename::Group(runs.groups.len() - 1, name));
            p.runs.rename_focus = true;
            save_runs(p, runs);
        }
        Do::StartRename(r) => {
            p.runs.rename = Some(r);
            p.runs.rename_focus = true;
        }
        Do::CancelRename => p.runs.rename = None,
        Do::RenameGroup(g, name) => {
            p.runs.rename = None;
            let name = name.trim().to_string();
            let mut runs = p.state.runs.clone();
            if let Some(group) = runs.groups.get_mut(g)
                && !name.is_empty()
                && group.name != name
            {
                group.name = name;
                save_runs(p, runs);
            }
        }
        Do::RenameConfig(id, name) => {
            p.runs.rename = None;
            let name = name.trim().to_string();
            let mut runs = p.state.runs.clone();
            if let Some((g, i)) = find_config(&runs, &id)
                && !name.is_empty()
                && runs.groups[g].configs[i].name != name
            {
                runs.groups[g].configs[i].name = name.clone();
                if let Some(d) = p.runs.edit.as_mut().filter(|d| d.cfg.id == id) {
                    d.cfg.name = name;
                }
                save_runs(p, runs);
            }
        }
        Do::DeleteGroup(g) => {
            let mut runs = p.state.runs.clone();
            if g < runs.groups.len() {
                let gone = runs.groups.remove(g);
                if p.runs
                    .edit
                    .as_ref()
                    .is_some_and(|d| gone.configs.iter().any(|c| c.id == d.cfg.id))
                {
                    p.runs.edit = None;
                }
                save_runs(p, runs);
            }
        }
        Do::DeleteConfig(id) => {
            let mut runs = p.state.runs.clone();
            if let Some((g, i)) = find_config(&runs, &id) {
                runs.groups[g].configs.remove(i);
                if p.runs.edit.as_ref().is_some_and(|d| d.cfg.id == id) {
                    p.runs.edit = None;
                }
                save_runs(p, runs);
            }
        }
        Do::SaveEdit { run } => {
            let Some(d) = &p.runs.edit else {
                return;
            };
            let cfg = d.to_config();
            let id = cfg.id.clone();
            let mut runs = p.state.runs.clone();
            match find_config(&runs, &id) {
                Some((g, i)) => runs.groups[g].configs[i] = cfg.clone(),
                None => {
                    if runs.groups.is_empty() {
                        runs.groups.push(Group {
                            name: "Configs".into(),
                            configs: Vec::new(),
                        });
                    }
                    runs.groups[0].configs.push(cfg.clone());
                }
            }
            if runs != p.state.runs {
                save_runs(p, runs);
            }
            p.runs.edit = Some(ConfigDraft::new(&cfg));
            if run {
                p.engine.send(Command::RunConfig { config_id: id });
            }
        }
        Do::SaveEnvs => {
            let Some((_, draft)) = &p.runs.envs else {
                return;
            };
            match draft.to_envs(&p.state.envs.profiles, p.state.envs.active.as_deref()) {
                Ok((envs, renames)) => {
                    p.runs.envs_error = None;
                    let mut runs = p.state.runs.clone();
                    follow_renames(&mut runs, &renames);
                    if runs != p.state.runs {
                        save_runs(p, runs);
                    }
                    // The draft stays until `RunsLoaded` shows it saved (or, refused, keeps
                    // the edits).
                    p.engine.send(Command::SaveEnvs(envs));
                }
                Err(e) => p.runs.envs_error = Some(e),
            }
        }
        Do::RevertEnvs => {
            p.runs.envs = None;
            p.runs.envs_error = None;
            p.runs.revealed.clear();
        }
    }
}

/// Configs follow a renamed profile. One whose profile was deleted keeps the reference: the
/// engine refuses to run it and its editor says the profile is missing, so it never quietly
/// runs against whichever profile happens to be active.
fn follow_renames(runs: &mut Runs, renames: &[(String, Option<String>)]) {
    for c in runs.groups.iter_mut().flat_map(|g| &mut g.configs) {
        if let Some((_, Some(new))) = renames
            .iter()
            .find(|(old, _)| c.env_profile.as_deref() == Some(old.as_str()))
        {
            c.env_profile = Some(new.clone());
        }
    }
}

/// Move config `id` into group `to` at `index` (the end when None or past it). False (and
/// nothing changed) when there's no such config or group.
pub fn move_config(runs: &mut Runs, id: &str, to: usize, index: Option<usize>) -> bool {
    if to >= runs.groups.len() {
        return false;
    }
    let Some((g, i)) = find_config(runs, id) else {
        return false;
    };
    let cfg = runs.groups[g].configs.remove(i);
    let dest = &mut runs.groups[to].configs;
    let at = index.unwrap_or(dest.len()).min(dest.len());
    dest.insert(at, cfg);
    true
}

/// (group, index) of config `id`.
pub fn find_config(runs: &Runs, id: &str) -> Option<(usize, usize)> {
    runs.groups.iter().enumerate().find_map(|(g, group)| {
        group
            .configs
            .iter()
            .position(|c| c.id == id)
            .map(|i| (g, i))
    })
}

#[cfg(test)]
mod unit {
    use super::*;
    use void_core::runner::{Group, RunConfig};

    fn names(runs: &Runs) -> Vec<Vec<&str>> {
        runs.groups
            .iter()
            .map(|g| g.configs.iter().map(|c| c.name.as_str()).collect())
            .collect()
    }

    #[test]
    fn runs_tree_moves_config_between_groups() {
        let cfg = |n: &str| RunConfig::new(n, vec![]);
        let (a1, a2, b1) = (cfg("a1"), cfg("a2"), cfg("b1"));
        let mut runs = Runs {
            groups: vec![
                Group {
                    name: "A".into(),
                    configs: vec![a1.clone(), a2.clone()],
                },
                Group {
                    name: "B".into(),
                    configs: vec![b1.clone()],
                },
            ],
        };
        assert!(move_config(&mut runs, &a1.id, 1, Some(0)));
        assert_eq!(names(&runs), [vec!["a2"], vec!["a1", "b1"]]);
        // Within a group, to the end.
        assert!(move_config(&mut runs, &a1.id, 1, None));
        assert_eq!(names(&runs), [vec!["a2"], vec!["b1", "a1"]]);
        // Within a group, forward: the index is where it ends up.
        assert!(move_config(&mut runs, &b1.id, 1, Some(1)));
        assert_eq!(names(&runs), [vec!["a2"], vec!["a1", "b1"]]);
        // Past the end appends; the last config leaving a group leaves it empty, not gone.
        assert!(move_config(&mut runs, &a2.id, 1, Some(99)));
        assert_eq!(names(&runs), [vec![], vec!["a1", "b1", "a2"]]);
        // No such config or group: nothing moves.
        let before = runs.clone();
        assert!(!move_config(&mut runs, "nope", 0, None));
        assert!(!move_config(&mut runs, &a1.id, 7, None));
        assert_eq!(runs, before);
    }

    #[test]
    fn env_draft_round_trips_and_follows_renames() {
        let envs = Envs {
            active: Some("dev".into()),
            profiles: vec![
                EnvProfile {
                    name: "dev".into(),
                    vars: [
                        ("A".to_string(), EnvValue::Value("secret".into())),
                        ("B".to_string(), EnvValue::Env("HOME".into())),
                        ("C".to_string(), EnvValue::Cmd("pass show x".into())),
                    ]
                    .into(),
                },
                EnvProfile {
                    name: "prod".into(),
                    vars: Default::default(),
                },
            ],
        };
        let mut d = EnvsDraft::new(&envs.profiles);
        assert_eq!(
            d.to_envs(&envs.profiles, Some("dev")).unwrap(),
            (envs.clone(), vec![])
        );
        // Renaming the active profile keeps it active; deleting one is reported.
        d.profiles[0].name = "local".into();
        d.profiles.remove(1);
        let (saved, renames) = d.to_envs(&envs.profiles, Some("dev")).unwrap();
        assert_eq!(saved.active.as_deref(), Some("local"));
        assert_eq!(
            renames,
            [
                ("dev".to_string(), Some("local".to_string())),
                ("prod".to_string(), None)
            ]
        );
        // Bad names are refused, without quoting any value.
        d.profiles[0].vars[0].name = "1BAD".into();
        let err = d.to_envs(&envs.profiles, None).unwrap_err();
        assert!(err.contains("1BAD") && !err.contains("secret"), "{err}");
        d.profiles[0].vars[0].name = "B".into();
        assert!(
            d.to_envs(&envs.profiles, None)
                .unwrap_err()
                .contains("twice")
        );
    }

    /// Configs follow a renamed profile; one whose profile was deleted keeps pointing at it
    /// (so it can't silently run against another profile).
    #[test]
    fn configs_follow_renames_but_keep_deleted_profiles() {
        let pin = |name: &str, p: Option<&str>| {
            let mut c = RunConfig::new(name, vec![]);
            c.env_profile = p.map(str::to_string);
            c
        };
        let mut runs = Runs {
            groups: vec![Group {
                name: "g".into(),
                configs: vec![
                    pin("a", Some("dev")),
                    pin("b", Some("staging")),
                    pin("c", None),
                ],
            }],
        };
        follow_renames(
            &mut runs,
            &[
                ("dev".to_string(), Some("local".to_string())),
                ("staging".to_string(), None),
            ],
        );
        let got: Vec<_> = runs.groups[0]
            .configs
            .iter()
            .map(|c| c.env_profile.as_deref())
            .collect();
        assert_eq!(got, [Some("local"), Some("staging"), None]);
    }

    #[test]
    fn small_text_follows_the_ui_style() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|s| {
            s.text_styles
                .insert(egui::TextStyle::Small, egui::FontId::proportional(21.5));
        });
        let mut size = 0.0;
        let mut out = ctx.run_ui(Default::default(), |ui| size = small(ui));
        out.textures_delta.clear();
        assert_eq!(size, 21.5);
    }
}

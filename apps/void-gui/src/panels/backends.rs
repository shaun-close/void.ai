//! The Backends panel: list, edit, test and activate backend profiles.

use egui::{DragValue, RichText, TextEdit, Ui};
use serde_json::{Map, Value};
use void_core::config::{ApiKeySource, BackendKind, Config, Profile};
use void_core::engine::Command;

use super::Panels;
use crate::theme::{self, icon};

/// Editing state: the profile being edited (a copy until Save) and its `extra_body` text.
#[derive(Debug, Default)]
pub struct BackendsUi {
    draft: Option<Profile>,
    extra_text: String,
    confirm_delete: bool,
}

/// A profile id from its name: lowercase ASCII words joined by `-`, made unique with `-2`, `-3`….
pub fn profile_slug(name: &str, existing: &[Profile]) -> String {
    let mut base = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    let base = match base.trim_end_matches('-') {
        "" => "profile".to_string(),
        b => b.to_string(),
    };
    let taken = |id: &str| existing.iter().any(|p| p.id == id);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .unwrap_or(base)
}

/// Parse the `extra_body` editor: empty is `{}`, anything else must be a JSON object.
pub fn parse_extra_body(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("must be a JSON object".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// The config with `p` replacing the profile of the same id, or appended.
pub fn with_profile(c: &Config, p: Profile) -> Config {
    let mut c = c.clone();
    match c.profiles.iter_mut().find(|x| x.id == p.id) {
        Some(slot) => *slot = p,
        None => c.profiles.push(p),
    }
    c
}

#[cfg(test)]
impl BackendsUi {
    /// Start editing a profile.
    pub fn select(&mut self, p: Profile) {
        edit(self, p);
    }
}

fn edit(ui: &mut BackendsUi, p: Profile) {
    ui.extra_text = if p.extra_body.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(&p.extra_body).unwrap_or_default()
    };
    ui.draft = Some(p);
    ui.confirm_delete = false;
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| show_inner(ui, p));
}

fn show_inner(ui: &mut Ui, p: &mut Panels) {
    let active = p.state.saved_config.active_profile.clone();
    let profiles = p.state.saved_config.profiles.clone();
    let selected = p.backends.draft.as_ref().map(|d| d.id.clone());

    let pal = theme::palette_of(ui);
    theme::heading(ui, icon::PLUGS, "Backend profiles");
    ui.horizontal_wrapped(|ui| {
        if theme::primary_button(ui, icon::PLUS, "Add").clicked() {
            let id = profile_slug("New profile", &profiles);
            edit(
                p.backends,
                Profile {
                    id,
                    name: "New profile".into(),
                    kind: BackendKind::OpenAi,
                    base_url: "http://127.0.0.1:8080/v1".into(),
                    api_key: ApiKeySource::None,
                    model: String::new(),
                    context_limit: 32768,
                    max_output: 8192,
                    extra_body: Map::new(),
                    command: None,
                },
            );
        }
        if let Some(d) = p.backends.draft.clone()
            && ui.button(theme::itext(icon::COPY, "Duplicate")).clicked()
        {
            let name = format!("{} copy", d.name);
            let id = profile_slug(&name, &profiles);
            edit(p.backends, Profile { id, name, ..d });
        }
    });
    ui.add_space(4.0);
    for prof in &profiles {
        let sel = selected.as_deref() == Some(prof.id.as_str());
        let resp = egui::Frame::new()
            .fill(if sel {
                theme::lerp(pal.bg_raised, pal.accent, 0.16)
            } else {
                pal.bg_raised
            })
            .stroke(egui::Stroke::new(
                1.0,
                if sel { pal.accent } else { pal.border },
            ))
            .corner_radius(egui::CornerRadius::same(9))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&prof.name)
                            .family(egui::FontFamily::Name(theme::BOLD.into()))
                            .color(pal.text),
                    );
                    if prof.id == active {
                        ui.label(theme::ico(icon::STAR).color(pal.warn))
                            .on_hover_text("default profile");
                    }
                    let (name, color) = match prof.kind {
                        BackendKind::OpenAi => ("OpenAI API", pal.accent2),
                        BackendKind::Anthropic => ("Anthropic API", pal.accent3),
                        BackendKind::ClaudeCode => ("Claude Code", pal.accent),
                    };
                    theme::pill(ui, icon::CPU, name, color);
                });
                let host = match prof.kind {
                    BackendKind::ClaudeCode => prof.claude_command().to_string(),
                    _ => prof
                        .base_url
                        .split("//")
                        .nth(1)
                        .unwrap_or(&prof.base_url)
                        .trim_end_matches('/')
                        .to_string(),
                };
                let model = if prof.model.is_empty() && prof.kind == BackendKind::ClaudeCode {
                    "default model"
                } else {
                    prof.model.as_str()
                };
                ui.label(
                    RichText::new(format!("{model} · {host}"))
                        .small()
                        .color(pal.text_faint),
                );
            })
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            edit(p.backends, prof.clone());
        }
        ui.add_space(2.0);
    }
    ui.add_space(6.0);

    let Some(mut draft) = p.backends.draft.take() else {
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Pick a profile to edit it").color(pal.text_faint));
        });
        return;
    };
    let d = &mut draft;
    let claude_code = d.kind == BackendKind::ClaudeCode;
    let mut keep = true;
    let saved = profiles.iter().any(|x| x.id == d.id);
    ui.vertical(|ui| {
        theme::heading(ui, icon::PENCIL, "Edit profile");
        ui.add_space(2.0);
        egui::Grid::new("profile-editor")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut d.name);
                ui.end_row();

                ui.label("API");
                egui::ComboBox::from_id_salt("profile-kind")
                    .selected_text(match d.kind {
                        BackendKind::OpenAi => "OpenAI-compatible",
                        BackendKind::Anthropic => "Anthropic",
                        BackendKind::ClaudeCode => "Claude Code",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.kind, BackendKind::OpenAi, "OpenAI-compatible");
                        ui.selectable_value(&mut d.kind, BackendKind::Anthropic, "Anthropic");
                        ui.selectable_value(&mut d.kind, BackendKind::ClaudeCode, "Claude Code");
                    });
                ui.end_row();

                if claude_code {
                    ui.label("Command");
                    let mut cmd = d.command.clone().unwrap_or_default();
                    ui.add(
                        TextEdit::singleline(&mut cmd)
                            .hint_text("claude")
                            .desired_width(f32::INFINITY),
                    );
                    d.command = Some(cmd).filter(|c| !c.trim().is_empty());
                    ui.end_row();
                } else {
                    ui.label("Base URL");
                    ui.add(TextEdit::singleline(&mut d.base_url).desired_width(f32::INFINITY));
                    ui.end_row();
                }

                ui.label("API key");
                ui.vertical(|ui| {
                    let labels = ["None", "Key", "Environment variable", "Command"];
                    let current = match d.api_key {
                        ApiKeySource::None => 0,
                        ApiKeySource::Literal(_) => 1,
                        ApiKeySource::Env(_) => 2,
                        ApiKeySource::Command(_) => 3,
                    };
                    let mut k = current;
                    egui::ComboBox::from_id_salt("profile-key")
                        .selected_text(labels[k])
                        .show_ui(ui, |ui| {
                            for (i, l) in labels.iter().enumerate() {
                                ui.selectable_value(&mut k, i, *l);
                            }
                        });
                    if k != current {
                        d.api_key = match k {
                            1 => ApiKeySource::Literal(String::new()),
                            2 => ApiKeySource::Env(String::new()),
                            3 => ApiKeySource::Command(String::new()),
                            _ => ApiKeySource::None,
                        };
                    }
                    match &mut d.api_key {
                        ApiKeySource::Literal(s) => {
                            ui.add(
                                TextEdit::singleline(s)
                                    .password(true)
                                    .desired_width(f32::INFINITY),
                            );
                        }
                        ApiKeySource::Env(s) => {
                            ui.add(TextEdit::singleline(s).hint_text("VARIABLE_NAME"));
                        }
                        ApiKeySource::Command(s) => {
                            ui.add(
                                TextEdit::singleline(s)
                                    .hint_text("command that prints the key")
                                    .desired_width(f32::INFINITY),
                            );
                        }
                        ApiKeySource::None => {}
                    }
                });
                ui.end_row();

                ui.label("Model");
                ui.vertical(|ui| {
                    let mut model = TextEdit::singleline(&mut d.model).desired_width(f32::INFINITY);
                    if claude_code {
                        model = model.hint_text("empty = Claude Code's default, e.g. opus, sonnet");
                    }
                    ui.add(model);
                    if !claude_code
                        && let Some(Ok(models)) = p.state.tests.get(&d.id)
                        && !models.is_empty()
                    {
                        egui::ComboBox::from_id_salt("profile-models")
                            .selected_text(format!("{} models available", models.len()))
                            .show_ui(ui, |ui| {
                                for m in models {
                                    ui.selectable_value(&mut d.model, m.clone(), m);
                                }
                            });
                    }
                });
                ui.end_row();

                if !claude_code {
                    ui.label("Context limit");
                    ui.add(
                        DragValue::new(&mut d.context_limit)
                            .range(512..=10_000_000)
                            .speed(256),
                    );
                    ui.end_row();

                    ui.label("Max output");
                    ui.add(
                        DragValue::new(&mut d.max_output)
                            .range(16..=1_000_000)
                            .speed(64),
                    );
                    ui.end_row();
                }
            });

        let extra = if claude_code {
            Ok(d.extra_body.clone())
        } else {
            ui.label("Extra request fields (JSON object, merged into every request)");
            ui.add(
                TextEdit::multiline(&mut p.backends.extra_text)
                    .code_editor()
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            let extra = parse_extra_body(&p.backends.extra_text);
            if let Err(e) = &extra {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            extra
        };

        match p.state.tests.get(&d.id) {
            Some(Ok(v)) if claude_code => {
                ui.label(RichText::new(format!("Found Claude Code {}", v.join(" "))).weak());
            }
            Some(Ok(models)) => {
                ui.label(RichText::new(format!("Connected: {} models", models.len())).weak());
            }
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            None => {}
        }

        ui.horizontal(|ui| {
            if ui
                .button(theme::itext(
                    icon::LIGHTNING,
                    if claude_code {
                        "Test"
                    } else {
                        "Test connection"
                    },
                ))
                .clicked()
            {
                let mut test = d.clone();
                if let Ok(m) = &extra {
                    test.extra_body = m.clone();
                }
                p.state.tests.remove(&d.id);
                p.engine.send(Command::TestProfile(test));
            }
            if ui
                .add_enabled(
                    extra.is_ok(),
                    egui::Button::new(theme::itext_styled(
                        icon::FLOPPY,
                        "Save",
                        theme::body_size(),
                        egui::Color32::WHITE,
                    ))
                    .fill(pal.accent),
                )
                .clicked()
                && let Ok(m) = &extra
            {
                d.extra_body = m.clone();
                let prof = d.clone();
                super::save_change(p.state, p.engine, |c| *c = with_profile(c, prof.clone()));
            }
            if ui
                .add_enabled(
                    saved && d.id != active,
                    egui::Button::new(theme::itext(icon::STAR, "Make default")),
                )
                .clicked()
            {
                let id = d.id.clone();
                super::save_change(p.state, p.engine, |c| c.active_profile = id.clone());
            }
            if saved && d.id != active {
                if !p.backends.confirm_delete {
                    if ui
                        .button(theme::itext_styled(
                            icon::TRASH,
                            "Delete",
                            theme::body_size(),
                            pal.err,
                        ))
                        .clicked()
                    {
                        p.backends.confirm_delete = true;
                    }
                } else if ui
                    .add(
                        egui::Button::new(theme::itext_styled(
                            icon::TRASH,
                            "Really delete?",
                            theme::body_size(),
                            egui::Color32::WHITE,
                        ))
                        .fill(pal.err),
                    )
                    .clicked()
                {
                    let id = d.id.clone();
                    super::save_change(p.state, p.engine, |c| c.profiles.retain(|x| x.id != id));
                    keep = false;
                    p.backends.confirm_delete = false;
                }
            }
        });
    });
    // Put the draft back unless it was deleted or replaced (a profile was picked meanwhile).
    if keep && p.backends.draft.is_none() {
        p.backends.draft = Some(draft);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use void_core::config::Config;

    #[test]
    fn profile_slug_is_unique() {
        let c = Config::default();
        assert_eq!(profile_slug("Strata", &c.profiles), "strata-2");
        assert_eq!(profile_slug("My Box (LAN)", &c.profiles), "my-box-lan");
        assert_eq!(profile_slug("!!!", &c.profiles), "profile");
    }

    #[test]
    fn extra_body_parse() {
        let ok = parse_extra_body(r#"{"temperature": 0.2}"#).unwrap();
        assert_eq!(ok["temperature"], 0.2);
        assert!(parse_extra_body("").unwrap().is_empty());
        assert_eq!(
            parse_extra_body("[1]").unwrap_err(),
            "must be a JSON object"
        );
        assert!(parse_extra_body("{oops").is_err());
    }

    #[test]
    fn with_profile_replaces_or_appends() {
        let c = Config::default();
        let mut p = c.profile("ollama").unwrap().clone();
        p.model = "other".into();
        let c2 = with_profile(&c, p.clone());
        assert_eq!(c2.profiles.len(), c.profiles.len());
        assert_eq!(c2.profile("ollama").unwrap().model, "other");
        p.id = "new-one".into();
        let c3 = with_profile(&c2, p);
        assert_eq!(c3.profiles.len(), c.profiles.len() + 1);
    }
}

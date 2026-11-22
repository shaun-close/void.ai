//! The Settings panel: theme, font size, default profile and the base system prompt.

use egui::{RichText, TextEdit, Ui};
use void_core::config::Theme;

use super::Panels;
use crate::theme::{self, icon};

pub fn clamp_font(size: f32) -> f32 {
    size.clamp(10.0, 24.0)
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    let pal = theme::palette_of(ui);
    let c = &mut p.state.config;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            theme::heading(ui, icon::GEAR, "Settings");
            ui.add_space(4.0);
            theme::card(ui, |ui| {
                ui.label(RichText::new("Appearance").color(pal.text_dim).small());
                egui::Grid::new("settings-look")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Theme");
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut c.theme,
                                Theme::Dark,
                                theme::itext(icon::PLANET, "Void"),
                            );
                            ui.selectable_value(
                                &mut c.theme,
                                Theme::Light,
                                theme::itext(icon::SPARKLE, "Light"),
                            );
                        });
                        ui.end_row();
                        ui.label("Font size");
                        let mut size = c.font_size;
                        ui.add(egui::Slider::new(&mut size, 10.0..=24.0).step_by(0.5));
                        c.font_size = clamp_font(size);
                        ui.end_row();
                    });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(RichText::new("Model").color(pal.text_dim).small());
                ui.horizontal(|ui| {
                    ui.label("Default profile");
                    let name = c
                        .profile(&c.active_profile)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| c.active_profile.clone());
                    let ids: Vec<_> = c
                        .profiles
                        .iter()
                        .map(|p| (p.id.clone(), p.name.clone()))
                        .collect();
                    egui::ComboBox::from_id_salt("settings-profile")
                        .selected_text(theme::itext(icon::PLUGS, &name))
                        .show_ui(ui, |ui| {
                            for (id, name) in ids {
                                ui.selectable_value(&mut c.active_profile, id, name);
                            }
                        });
                });
            });
            ui.add_space(6.0);
            theme::card(ui, |ui| {
                ui.label(
                    RichText::new("Base system prompt")
                        .color(pal.text_dim)
                        .small(),
                );
                ui.add(
                    TextEdit::multiline(&mut c.base_system_prompt)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );
            });
        });
    let dirty = p.state.config != p.state.saved_config;
    ui.horizontal(|ui| {
        let save = egui::Button::new(theme::itext_styled(
            icon::FLOPPY,
            "Save",
            theme::body_size(),
            egui::Color32::WHITE,
        ))
        .fill(pal.accent);
        if ui.add_enabled(dirty, save).clicked() {
            super::save_config(p.state, p.engine);
        }
        if ui
            .add_enabled(
                dirty,
                egui::Button::new(theme::itext(icon::ARROW_CLOCKWISE, "Revert")),
            )
            .clicked()
        {
            p.state.config = p.state.saved_config.clone();
        }
        if dirty {
            ui.label(RichText::new("Unsaved changes").color(pal.warn).small());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_is_clamped() {
        assert_eq!(clamp_font(8.0), 10.0);
        assert_eq!(clamp_font(30.0), 24.0);
        assert_eq!(clamp_font(15.0), 15.0);
    }
}

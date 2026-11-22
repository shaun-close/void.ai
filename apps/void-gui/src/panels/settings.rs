//! The Settings panel: theme, font size, default profile and the base system prompt.

use egui::{RichText, TextEdit, Ui};
use void_core::config::Theme;

use super::Panels;

pub fn clamp_font(size: f32) -> f32 {
    size.clamp(10.0, 24.0)
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    let c = &mut p.state.config;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Theme");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut c.theme, Theme::Dark, "Dark");
                        ui.radio_value(&mut c.theme, Theme::Light, "Light");
                    });
                    ui.end_row();

                    ui.label("Font size");
                    let mut size = c.font_size;
                    ui.add(egui::Slider::new(&mut size, 10.0..=24.0).step_by(0.5));
                    c.font_size = clamp_font(size);
                    ui.end_row();

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
                        .selected_text(name)
                        .show_ui(ui, |ui| {
                            for (id, name) in ids {
                                ui.selectable_value(&mut c.active_profile, id, name);
                            }
                        });
                    ui.end_row();
                });
            ui.label("Base system prompt");
            ui.add(
                TextEdit::multiline(&mut c.base_system_prompt)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY),
            );
        });
    let dirty = p.state.config != p.state.saved_config;
    ui.horizontal(|ui| {
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            super::save_config(p.state, p.engine);
        }
        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
            p.state.config = p.state.saved_config.clone();
        }
        if dirty {
            ui.label(RichText::new("Unsaved changes").weak());
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

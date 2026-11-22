//! The Review panel: a chat's pending changes as diffs, accepted or rejected per change, per
//! file or all at once. Nothing reaches the real workspace without an accept here.

use std::collections::HashSet;
use std::path::PathBuf;

use egui::{Color32, CornerRadius, FontFamily, Margin, RichText, Stroke, Ui};
use void_core::engine::Command;
use void_core::model::ConversationId;
use void_core::overlay::{Change, ChangeKind, Hunk};

use super::Panels;
use crate::theme::{self, BOLD, Palette, icon, with_alpha};

/// What the panel remembers between frames.
#[derive(Debug, Default)]
pub struct ReviewUi {
    /// The chat being reviewed when picked explicitly (else the focused chat).
    pub conversation: Option<ConversationId>,
    pub selected: Option<PathBuf>,
    pub keep_ignored: bool,
    listed: HashSet<ConversationId>,
    requested: Option<(ConversationId, PathBuf, u64)>,
}

pub fn diff_line_color(p: &Palette, c: char) -> Color32 {
    match c {
        '+' => p.ok,
        '-' => p.err,
        _ => p.text_dim,
    }
}

pub fn change_badge(p: &Palette, kind: ChangeKind) -> (&'static str, Color32) {
    match kind {
        ChangeKind::Added => ("A", p.ok),
        ChangeKind::Modified => ("M", p.warn),
        ChangeKind::Deleted => ("D", p.err),
        ChangeKind::ReplacedDir => ("R", p.accent),
    }
}

pub fn hunk_header(h: &Hunk) -> String {
    format!(
        "@@ -{},{} +{},{} @@",
        h.old_start, h.old_len, h.new_start, h.new_len
    )
}

fn badge(ui: &mut Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(with_alpha(color, 40))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(5, 0))
        .show(ui, |ui| {
            ui.label(RichText::new(label).monospace().color(color));
        });
}

pub fn show(ui: &mut Ui, p: &mut Panels) {
    let pal = theme::palette_of(ui);
    // Which chat: the explicit pick, else the focused chat, else any with changes.
    let with_changes: Vec<ConversationId> = p
        .state
        .changes
        .iter()
        .filter(|(_, c)| !c.is_empty())
        .map(|(id, _)| *id)
        .collect();
    let conv = p
        .review
        .conversation
        .filter(|c| with_changes.contains(c))
        .or(p.focused)
        .or_else(|| with_changes.first().copied());
    let Some(conv) = conv else {
        empty(ui, &pal);
        return;
    };
    if p.review.listed.insert(conv) {
        p.engine.send(Command::ListChanges(conv));
    }
    let changes: Vec<Change> = p.state.changes.get(&conv).cloned().unwrap_or_default();

    ui.horizontal(|ui| {
        theme::heading(ui, icon::GIT_DIFF, "Review");
        if with_changes.len() > 1 || (p.review.conversation.is_some() && Some(conv) != p.focused) {
            let title = |id: ConversationId| {
                p.state
                    .conversations
                    .iter()
                    .find(|m| m.id == id)
                    .map(|m| m.title.clone())
                    .unwrap_or_else(|| "chat".into())
            };
            let mut pick = conv;
            egui::ComboBox::from_id_salt("review-chat")
                .selected_text(theme::itext_small(icon::CHAT, &title(conv)))
                .show_ui(ui, |ui| {
                    for id in &with_changes {
                        ui.selectable_value(&mut pick, *id, title(*id));
                    }
                });
            if pick != conv {
                p.review.conversation = Some(pick);
                p.review.selected = None;
            }
        }
    });
    if changes.is_empty() {
        empty(ui, &pal);
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} pending", changes.len())).color(pal.text_dim));
        if theme::primary_button(ui, icon::CHECK, "Accept all").clicked() {
            p.engine.send(Command::AcceptAll {
                conversation: conv,
                keep_ignored: p.review.keep_ignored,
            });
        }
        if ui
            .button(theme::itext_styled(
                icon::X_CIRCLE,
                "Reject all",
                theme::body_size(),
                pal.err,
            ))
            .clicked()
        {
            p.engine.send(Command::RejectAll(conv));
            p.review.selected = None;
        }
        ui.checkbox(
            &mut p.review.keep_ignored,
            RichText::new("keep ignored files").small(),
        )
        .on_hover_text("Also copy build output and other .gitignored files the agent produced");
    });
    ui.add_space(4.0);

    let list_height = (ui.available_height() * 0.35).clamp(60.0, 260.0);
    egui::ScrollArea::vertical()
        .id_salt("review-files")
        .max_height(list_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for c in &changes {
                ui.horizontal(|ui| {
                    let (label, color) = change_badge(&pal, c.kind);
                    badge(ui, label, color);
                    let selected = p.review.selected.as_ref() == Some(&c.path);
                    let path = c.path.display().to_string();
                    if ui
                        .selectable_label(selected, RichText::new(&path).monospace())
                        .clicked()
                    {
                        p.review.selected = Some(c.path.clone());
                    }
                    if c.ignored {
                        ui.label(RichText::new("ignored").small().color(pal.text_faint));
                    }
                    if c.conflict {
                        ui.label(theme::ico(icon::WARNING).color(pal.err))
                            .on_hover_text("Changed on disk since void edited it");
                    }
                });
            }
        });
    ui.separator();

    let Some(path) = p
        .review
        .selected
        .clone()
        .filter(|s| changes.iter().any(|c| &c.path == s))
    else {
        ui.label(RichText::new("Pick a file to see its changes").color(pal.text_faint));
        return;
    };
    let change = changes.iter().find(|c| c.path == path).cloned().unwrap();
    let key = (conv, path.clone());
    let epoch = p.state.diffs_epoch;
    if !p.state.diffs.contains_key(&key) && p.review.requested != Some((conv, path.clone(), epoch))
    {
        p.review.requested = Some((conv, path.clone(), epoch));
        p.engine.send(Command::Diff {
            conversation: conv,
            path: path.clone(),
        });
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(path.display().to_string())
                .family(FontFamily::Name(BOLD.into()))
                .color(pal.text),
        );
        if change.conflict {
            ui.label(theme::itext_styled(
                icon::WARNING,
                "Changed on disk since void edited it",
                theme::small_size(),
                pal.err,
            ));
            if ui.button(theme::itext(icon::CHECK, "Overwrite")).clicked() {
                p.engine.send(Command::AcceptFile {
                    conversation: conv,
                    path: path.clone(),
                    force: true,
                });
            }
        } else if theme::primary_button(ui, icon::CHECK, "Accept file").clicked() {
            p.engine.send(Command::AcceptFile {
                conversation: conv,
                path: path.clone(),
                force: false,
            });
        }
        if ui
            .button(theme::itext_styled(
                icon::X_CIRCLE,
                "Reject file",
                theme::body_size(),
                pal.err,
            ))
            .clicked()
        {
            p.engine.send(Command::RejectFile {
                conversation: conv,
                path: path.clone(),
            });
        }
    });
    let Some(hunks) = p.state.diffs.get(&key).cloned() else {
        ui.spinner();
        return;
    };
    let disk = p.state.disk_diffs.get(&key).cloned().unwrap_or_default();
    egui::ScrollArea::vertical().id_salt("review-diff").auto_shrink([false, false]).show(ui, |ui| {
        if change.conflict && !disk.is_empty() {
            ui.label(theme::itext_styled(icon::WARNING, "On disk now — your edits since void changed this file (Overwrite discards them):", theme::small_size(), pal.err));
            for h in &disk {
                diff_lines(ui, &pal, h);
            }
            ui.add_space(6.0);
            ui.label(RichText::new("void's version:").small().color(pal.text_dim));
        }
        if hunks.is_empty() {
            ui.label(RichText::new("No text changes (a new empty file, a deletion of a folder, or binary content)").color(pal.text_faint));
        }
        for h in &hunks {
            egui::Frame::new()
                .fill(pal.bg_deep)
                .stroke(Stroke::new(1.0, pal.border))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(hunk_header(h)).monospace().small().color(pal.accent2));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(theme::itext_styled(icon::X_CIRCLE, "Reject", theme::small_size(), pal.err)).clicked() {
                                p.engine.send(Command::RejectHunk { conversation: conv, path: path.clone(), index: h.index });
                            }
                            // On a conflict only the whole file can be overwritten, deliberately.
                            if !change.conflict
                                && ui
                                    .small_button(theme::itext_styled(icon::CHECK, "Accept", theme::small_size(), pal.ok))
                                    .clicked()
                            {
                                p.engine.send(Command::AcceptHunk {
                                    conversation: conv,
                                    path: path.clone(),
                                    index: h.index,
                                    force: false,
                                });
                            }
                        });
                    });
                    for (c, line) in &h.lines {
                        let color = diff_line_color(&pal, *c);
                        let bg = match c {
                            '+' => with_alpha(pal.ok, 22),
                            '-' => with_alpha(pal.err, 22),
                            _ => Color32::TRANSPARENT,
                        };
                        egui::Frame::new().fill(bg).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.add(egui::Label::new(RichText::new(format!("{c} {line}")).monospace().color(color)).wrap());
                        });
                    }
                });
            ui.add_space(6.0);
        }
    });
}

fn diff_lines(ui: &mut Ui, pal: &Palette, h: &Hunk) {
    egui::Frame::new()
        .fill(pal.bg_deep)
        .stroke(Stroke::new(1.0, with_alpha(pal.err, 90)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (c, line) in &h.lines {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{c} {line}"))
                            .monospace()
                            .color(diff_line_color(pal, *c)),
                    )
                    .wrap(),
                );
            }
        });
}

fn empty(ui: &mut Ui, pal: &Palette) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(theme::ico(icon::GIT_DIFF).size(30.0).color(pal.text_faint));
        ui.label(RichText::new("No pending changes").color(pal.text_faint));
        ui.label(
            RichText::new("When void edits files, they wait here for you")
                .small()
                .color(pal.text_faint),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use void_core::config::Theme;
    use void_core::overlay::ChangeKind;

    #[test]
    fn diff_line_colours() {
        let p = theme::palette(Theme::Dark);
        assert_eq!(diff_line_color(&p, '+'), p.ok);
        assert_eq!(diff_line_color(&p, '-'), p.err);
        assert_eq!(diff_line_color(&p, ' '), p.text_dim);
    }

    #[test]
    fn change_badges() {
        let p = theme::palette(Theme::Dark);
        assert_eq!(change_badge(&p, ChangeKind::Added), ("A", p.ok));
        assert_eq!(change_badge(&p, ChangeKind::Modified), ("M", p.warn));
        assert_eq!(change_badge(&p, ChangeKind::Deleted), ("D", p.err));
        assert_eq!(change_badge(&p, ChangeKind::ReplacedDir), ("R", p.accent));
    }

    #[test]
    fn hunk_header_reads_like_git() {
        let h = void_core::overlay::Hunk {
            index: 0,
            old_start: 3,
            old_len: 2,
            new_start: 3,
            new_len: 4,
            lines: vec![],
        };
        assert_eq!(hunk_header(&h), "@@ -3,2 +3,4 @@");
    }
}

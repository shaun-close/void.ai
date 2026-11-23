//! Drawing an editor tab: only the visible lines, with the gutter, selections, search
//! matches and the cursor; then the `:` line and the status line. Mouse clicks and drags.

use egui::accesskit::Role;
use egui::{
    CornerRadius, FontId, Pos2, Rect, ScrollArea, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2,
    pos2, vec2,
};
use void_vim::Mode;

use super::host::{Ctx, Host};
pub use super::paint::relative_line_numbers;
use super::paint::{Row, TEXT_PAD, layout_row, matching_bracket, search_matches, selected_cols};
use super::state::Doc;
use super::{DocKey, input, status};
use crate::panels::Panels;
use crate::theme::{self, with_alpha};

pub fn editor(ui: &mut Ui, p: &mut Panels, tab: u32, key: &DocKey) {
    let pal = theme::palette_of(ui);
    let id = ui.scope_id().with("editor");
    let vim = p.editors.settings.vim;
    let relative = p.editors.settings.relative_numbers;
    if p.editors.focus == Some(tab)
        || (p.focused_editor == Some(tab) && ui.memory(|m| m.focused().is_none()))
    {
        ui.memory_mut(|m| m.request_focus(id));
        if p.editors.focus == Some(tab) {
            p.editors.focus = None;
        }
    }
    let focused = ui.memory(|m| m.has_focus(id));
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
    }
    let store = &mut *p.editors;
    let Some(doc) = store.docs.get_mut(key) else {
        return;
    };
    if let Some(v) = doc.views.remove(&tab) {
        doc.core.view = v;
    }
    let mut cx = Ctx {
        path: &key.0,
        view: &key.1,
        tab,
        clipboard: &store.clipboard,
        actions: &mut store.actions,
    };

    // Input first, so this frame draws its result.
    let mut follow = false;
    if focused {
        let events = ui.input(|i| i.events.clone());
        doc.core.view.scrolled = false;
        let before = (doc.core.sel, doc.core.buffer.version());
        if input::handle(&events, doc, &mut cx, vim) {
            follow = (doc.core.sel, doc.core.buffer.version()) != before;
            if follow {
                doc.core.ensure_visible();
            }
        }
    }

    let font = FontId::monospace(theme::body_size() - 1.0);
    let (glyph_h, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, 'M')));
    let row_h = (glyph_h * 1.35).round();
    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let notice = doc.core.buffer.notice.clone();
    let notice_h = if notice.is_some() { 26.0 } else { 0.0 };
    let status_h = 26.0;
    let prompt_h = if doc.core.prompt.is_some() {
        row_h + 6.0
    } else {
        0.0
    };
    let area = Rect::from_min_max(
        pos2(full.left(), full.top() + notice_h),
        pos2(full.right(), full.bottom() - status_h - prompt_h),
    );
    let n_lines = doc.core.buffer.line_count();
    let digits = n_lines.to_string().len().max(3);
    let gutter_w = (digits + 2) as f32 * char_w;
    let gutter = Rect::from_min_max(area.min, pos2(area.left() + gutter_w, area.bottom()));
    let text_rect = Rect::from_min_max(pos2(gutter.right(), area.top()), area.max);
    let painter = ui.painter_at(full);
    painter.rect_filled(gutter, CornerRadius::ZERO, pal.bg_panel);

    if let Some(n) = &notice {
        let r = Rect::from_min_size(full.min, vec2(full.width(), notice_h - 4.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(r));
        child.label(theme::itext_styled(
            theme::icon::WARNING,
            n,
            theme::small_size(),
            pal.warn,
        ));
    }

    // The visual-block shape comes from the engine.
    let block = {
        let mut sink = Vec::new();
        let mut cx = Ctx {
            path: &key.0,
            view: &key.1,
            tab,
            clipboard: &store.clipboard,
            actions: &mut sink,
        };
        let host = Host {
            core: &mut doc.core,
            cx: &mut cx,
        };
        doc.vim.visual_block(&host)
    };

    let (cursor_line, cursor_col) = doc.core.line_col();
    let mode = doc.mode();
    let mut sa = ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .animated(false);
    if doc.core.view.apply {
        sa = sa
            .vertical_scroll_offset(doc.core.view.top as f32 * row_h)
            .horizontal_scroll_offset(doc.core.view.left);
        doc.core.view.apply = false;
    }
    let mut child = ui.new_child(UiBuilder::new().max_rect(text_rect));
    child.spacing_mut().item_spacing = Vec2::ZERO;
    let out = sa.show_rows(&mut child, row_h, n_lines, |ui, rows| {
        let origin = ui.max_rect().left_top();
        let text_x = origin.x + TEXT_PAD;
        let painter = ui.painter().clone();
        let full_w = ui.clip_rect().right() - origin.x;
        let spans = doc.spans(rows.clone());
        let b = &doc.core.buffer;
        let vis_start = b.line_start(rows.start.min(n_lines - 1));
        let vis_end = if rows.end >= n_lines {
            doc.core.len()
        } else {
            b.line_start(rows.end)
        };
        let matches = search_matches(doc, vis_start..vis_end);
        let head = doc.core.head();
        let bracket = matching_bracket(b.rope(), head).map(|m| (head, m));
        let mut drawn = Vec::with_capacity(rows.len());
        let mut max_w: f32 = 0.0;
        for (i, line) in rows.clone().enumerate() {
            let y = origin.y + i as f32 * row_h;
            let (galley, cols, len) = layout_row(ui, doc, line, &spans[i], &font, &pal);
            let row = Row {
                line,
                y,
                galley,
                cols,
                len,
            };
            let rect_of = |from: usize, to: usize| {
                Rect::from_min_max(
                    pos2(text_x + row.x(from), y),
                    pos2(text_x + row.x(to), y + row_h),
                )
            };
            if line == cursor_line {
                painter.rect_filled(
                    Rect::from_min_size(
                        pos2(ui.clip_rect().left(), y),
                        vec2(full_w.max(0.0) + 4000.0, row_h),
                    ),
                    CornerRadius::ZERO,
                    with_alpha(pal.text, if pal.dark { 9 } else { 14 }),
                );
            }
            let line_start = doc.core.buffer.line_start(line);
            for m in &matches {
                if m.end <= line_start || m.start > line_start + len {
                    continue;
                }
                let (s, e) = (m.start.max(line_start), m.end.min(line_start + len));
                let r = rect_of(s - line_start, e - line_start);
                painter.rect_filled(r, CornerRadius::same(2), with_alpha(pal.warn, 70));
            }
            if let Some((from, to, past_end)) = selected_cols(doc, block, vim, line) {
                let mut r = rect_of(from, to);
                if past_end {
                    r.max.x += char_w;
                }
                painter.rect_filled(r, CornerRadius::ZERO, with_alpha(pal.accent, 95));
            }
            if let Some((a, b)) = bracket {
                for at in [a, b] {
                    if at >= line_start && at < line_start + len {
                        let c = at - line_start;
                        let r = Rect::from_min_size(
                            pos2(text_x + row.x(c), y + 1.0),
                            vec2(row.width(c, char_w), row_h - 2.0),
                        );
                        painter.rect_stroke(
                            r,
                            CornerRadius::same(2),
                            Stroke::new(1.0, pal.text_dim),
                            StrokeKind::Inside,
                        );
                    }
                }
            }
            if line == cursor_line {
                let x = text_x + row.x(cursor_col);
                let insert = !vim || matches!(mode, Mode::Insert);
                if insert {
                    painter.rect_filled(
                        Rect::from_min_size(pos2(x - 1.0, y + 1.0), vec2(2.0, row_h - 2.0)),
                        CornerRadius::ZERO,
                        if focused { pal.accent2 } else { pal.text_faint },
                    );
                } else {
                    let r = Rect::from_min_size(
                        pos2(x, y + 1.0),
                        vec2(row.width(cursor_col, char_w), row_h - 2.0),
                    );
                    if focused {
                        let c = if mode == Mode::Replace {
                            pal.err
                        } else {
                            pal.accent2
                        };
                        painter.rect_filled(r, CornerRadius::same(1), with_alpha(c, 150));
                    } else {
                        painter.rect_stroke(
                            r,
                            CornerRadius::same(1),
                            Stroke::new(1.0, pal.text_faint),
                            StrokeKind::Inside,
                        );
                    }
                }
            }
            painter.galley(
                pos2(text_x, y + (row_h - glyph_h) / 2.0),
                row.galley.clone(),
                pal.text,
            );
            max_w = max_w.max(row.galley.size().x);
            drawn.push(row);
        }
        ui.allocate_exact_size(
            vec2(max_w + TEXT_PAD + char_w * 8.0, rows.len() as f32 * row_h),
            Sense::hover(),
        );
        (drawn, text_x)
    });
    let (drawn, text_x) = out.inner;
    let offset = out.state.clamped_offset();
    let view = &mut doc.core.view;
    view.top = (offset.y / row_h).round() as usize;
    view.rows = ((out.inner_rect.height() / row_h).floor() as usize).max(1);
    view.left = offset.x;

    // Keep the cursor in view sideways.
    if follow && let Some(row) = drawn.iter().find(|r| r.line == cursor_line) {
        let x = row.x(cursor_col);
        let w = out.inner_rect.width() - TEXT_PAD - char_w * 2.0;
        let left = if x < offset.x {
            Some((x - char_w * 4.0).max(0.0))
        } else if x > offset.x + w {
            Some(x - w + char_w * 4.0)
        } else {
            None
        };
        if let Some(left) = left {
            doc.core.view.left = left;
            doc.core.view.apply = true;
            ui.ctx().request_repaint();
        }
    }

    // Gutter numbers.
    let gp = ui.painter_at(gutter);
    let small = FontId::monospace(theme::body_size() - 2.0);
    let first = drawn.first().map_or(0, |r| r.line);
    let labels = if relative {
        relative_line_numbers(cursor_line, first..first + drawn.len())
    } else {
        (first + 1..first + drawn.len() + 1).collect()
    };
    for (row, n) in drawn.iter().zip(labels) {
        let current = row.line == cursor_line;
        if current {
            gp.rect_filled(
                Rect::from_min_size(pos2(gutter.left(), row.y), vec2(gutter.width(), row_h)),
                CornerRadius::ZERO,
                with_alpha(pal.text, if pal.dark { 9 } else { 14 }),
            );
        }
        gp.text(
            pos2(gutter.right() - char_w * 1.2, row.y + row_h / 2.0),
            egui::Align2::RIGHT_CENTER,
            n.to_string(),
            small.clone(),
            if current { pal.text } else { pal.text_faint },
        );
    }

    // Mouse: click to place the cursor, drag to select.
    let resp = ui.interact(out.inner_rect, id, Sense::click_and_drag());
    if (resp.clicked() || resp.drag_started() || resp.dragged())
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some(at) = char_at(doc, &drawn, text_x, row_h, pos)
    {
        if resp.clicked() || resp.drag_started() {
            ui.memory_mut(|m| m.request_focus(id));
        }
        mouse(
            doc,
            &mut cx,
            vim,
            at,
            resp.dragged() && !resp.drag_started(),
            ui.input(|i| i.modifiers.shift),
        );
    }
    let visible: String = drawn
        .iter()
        .map(|r| doc.core.buffer.rope().line(r.line).to_string())
        .collect();
    resp.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(Role::MultilineTextInput, true, "Text editor");
        info.current_text_value = Some(visible.clone());
        info
    });

    if let Some(pr) = &doc.core.prompt {
        let r = Rect::from_min_max(
            pos2(full.left(), area.bottom() + 2.0),
            pos2(full.right(), area.bottom() + prompt_h),
        );
        status::prompt_line(ui, pr, r, &font, char_w, row_h, &pal);
    }

    doc.views.insert(tab, doc.core.view.clone());

    let status = Rect::from_min_max(pos2(full.left(), full.bottom() - status_h + 2.0), full.max);
    status::status_line(ui, doc, vim, status, &pal);
}

/// The char a pointer position falls on (clamped to the drawn rows).
fn char_at(doc: &Doc, drawn: &[Row], text_x: f32, row_h: f32, pos: Pos2) -> Option<usize> {
    let first = drawn.first()?;
    let i = ((pos.y - first.y) / row_h).floor();
    let row = if i < 0.0 {
        first
    } else {
        drawn.get(i as usize).unwrap_or(drawn.last()?)
    };
    let col = row.col_at(pos.x - text_x).min(row.len);
    Some(doc.core.buffer.line_start(row.line) + col)
}

/// A click or drag at char `at`.
fn mouse(doc: &mut Doc, cx: &mut Ctx, vim: bool, at: usize, dragging: bool, shift: bool) {
    if !vim {
        doc.core.sel = if dragging || shift {
            (doc.core.sel.0, at)
        } else {
            (at, at)
        };
        doc.typing = false;
        return;
    }
    let mode = doc.mode();
    if dragging {
        if mode == Mode::Normal && at != doc.core.sel.0 {
            let anchor = doc.core.sel.0;
            let mut host = Host {
                core: &mut doc.core,
                cx,
            };
            doc.vim.key(&mut host, "v");
            doc.core.sel = (anchor, at);
        } else if mode.is_visual() {
            doc.core.sel.1 = at;
        }
        return;
    }
    if mode.is_visual() {
        let mut host = Host {
            core: &mut doc.core,
            cx,
        };
        doc.vim.key(&mut host, "<Esc>");
    }
    // Normal mode can't sit on a line's end.
    let b = &doc.core.buffer;
    let line = b.line_of(at);
    let end = b.line_start(line) + b.line_len(line);
    let at = if doc.mode() == Mode::Normal && at == end && b.line_len(line) > 0 {
        at - 1
    } else {
        at
    };
    doc.core.sel = (at, at);
}

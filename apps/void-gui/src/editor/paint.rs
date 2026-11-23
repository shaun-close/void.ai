//! What goes on a line: colours from the syntax tree, tab expansion, selections, matching
//! brackets, search matches and the gutter numbers.

use std::ops::Range;
use std::sync::Arc;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Color32, FontId, Galley, Ui, vec2};
use void_editor::{Hl, Span};
use void_vim::{Block, Mode};

use super::state::Doc;
use crate::theme::{Palette, lerp};

const TAB_WIDTH: usize = 4;
/// Longer lines are cut for drawing (a minified file shouldn't stall a frame).
const MAX_DRAWN_CHARS: usize = 10_000;
/// How far to look for a matching bracket.
const BRACKET_SCAN: usize = 20_000;
/// Space between the gutter and the text.
pub const TEXT_PAD: f32 = 6.0;

/// Gutter labels for `rows`: the cursor line shows its own (1-based) number, the others their
/// distance from it, as Vim's `number` + `relativenumber`.
pub fn relative_line_numbers(cursor: usize, rows: Range<usize>) -> Vec<usize> {
    rows.map(|r| {
        if r == cursor {
            r + 1
        } else {
            r.abs_diff(cursor)
        }
    })
    .collect()
}

/// The colour for a highlight class, from the void palette.
pub fn syntax_color(p: &Palette, hl: Hl) -> Color32 {
    match hl {
        Hl::Keyword => lerp(p.accent, p.text, 0.25),
        Hl::Type => p.accent2,
        Hl::Function => lerp(p.accent2, p.accent, 0.45),
        Hl::String => p.ok,
        Hl::Number | Hl::Constant => p.warn,
        Hl::Comment => p.text_faint,
        Hl::Macro | Hl::Tag | Hl::Escape => p.accent3,
        Hl::Attribute => lerp(p.accent3, p.text_dim, 0.4),
        Hl::Lifetime => lerp(p.warn, p.accent3, 0.5),
        Hl::Property => lerp(p.text, p.accent2, 0.35),
        Hl::Variable => p.text,
        Hl::Namespace => lerp(p.accent2, p.text, 0.4),
        Hl::Parameter => lerp(p.text, p.warn, 0.3),
        Hl::Operator | Hl::Punctuation => p.text_dim,
        Hl::Heading => p.accent,
        Hl::Link => p.accent2,
    }
}

/// One line as drawn: tabs expanded, plus where each char lands in the drawn text.
pub struct Row {
    pub line: usize,
    pub y: f32,
    pub galley: Arc<Galley>,
    /// Display char index of each char of the line, plus one past the end.
    pub cols: Vec<usize>,
    pub len: usize,
}

impl Row {
    /// X offset (from the text's left edge) of char column `col`.
    pub fn x(&self, col: usize) -> f32 {
        let d = self.cols[col.min(self.cols.len() - 1)];
        self.galley.pos_from_cursor(CCursor::new(d)).min.x
    }

    /// Width of the char at `col` (one space at the line end).
    pub fn width(&self, col: usize, char_w: f32) -> f32 {
        if col >= self.len {
            char_w
        } else {
            (self.x(col + 1) - self.x(col)).max(char_w * 0.5)
        }
    }

    /// The char column under x.
    pub fn col_at(&self, x: f32) -> usize {
        let d = self.galley.cursor_from_pos(vec2(x, 1.0)).index.0;
        self.cols.partition_point(|c| *c <= d).saturating_sub(1)
    }
}

/// Lay out line `line` with its colours.
pub fn layout_row(
    ui: &Ui,
    doc: &Doc,
    line: usize,
    spans: &[Span],
    font: &FontId,
    pal: &Palette,
) -> (Arc<Galley>, Vec<usize>, usize) {
    let r = doc.core.buffer.rope();
    let len = doc.core.buffer.line_len(line);
    let shown = len.min(MAX_DRAWN_CHARS);
    let mut cols = Vec::with_capacity(shown + 1);
    let mut job = LayoutJob::default();
    let mut run = String::new();
    let mut run_color = pal.text;
    let mut disp = 0;
    let mut span = 0;
    for (i, c) in r.line(line).chars().take(shown).enumerate() {
        cols.push(disp);
        while span < spans.len() && spans[span].end <= i {
            span += 1;
        }
        let color = match spans.get(span) {
            Some(s) if s.start <= i => syntax_color(pal, s.hl),
            _ => pal.text,
        };
        if color != run_color && !run.is_empty() {
            job.append(&run, 0.0, TextFormat::simple(font.clone(), run_color));
            run.clear();
        }
        run_color = color;
        if c == '\t' {
            let n = TAB_WIDTH - disp % TAB_WIDTH;
            run.extend(std::iter::repeat_n(' ', n));
            disp += n;
        } else {
            run.push(c);
            disp += 1;
        }
    }
    cols.push(disp);
    if !run.is_empty() {
        job.append(&run, 0.0, TextFormat::simple(font.clone(), run_color));
    }
    if job.sections.is_empty() {
        job.append("", 0.0, TextFormat::simple(font.clone(), pal.text));
    }
    job.wrap.max_width = f32::INFINITY;
    (ui.fonts_mut(|f| f.layout_job(job)), cols, shown)
}

/// The selected char columns of `line`: `(from, to, past_end)`.
pub fn selected_cols(
    doc: &Doc,
    block: Option<Block>,
    vim: bool,
    line: usize,
) -> Option<(usize, usize, bool)> {
    let b = &doc.core.buffer;
    let (a, h) = doc.core.sel;
    let start = b.line_start(line);
    let len = b.line_len(line);
    let mode = doc.mode();
    if vim && mode == Mode::VisualBlock {
        let k = block?;
        if line < k.top || line > k.bottom || k.left > len {
            return None;
        }
        let to = if k.to_end {
            len
        } else {
            (k.right + 1).min(len)
        };
        return Some((k.left, to.max(k.left), k.to_end));
    }
    if vim && mode == Mode::VisualLine {
        let (l1, l2) = (b.line_of(a.min(h)), b.line_of(a.max(h)));
        return (l1..=l2).contains(&line).then_some((0, len, true));
    }
    let (lo, hi) = if vim && mode == Mode::Visual {
        (a.min(h), a.max(h) + 1)
    } else if !vim && a != h {
        (a.min(h), a.max(h))
    } else {
        return None;
    };
    let end = start + len;
    if hi <= start || lo > end {
        return None;
    }
    let from = lo.max(start) - start;
    let past_end = hi > end;
    let to = hi.min(end) - start;
    Some((from, to.max(from), past_end))
}

/// The bracket matching the one at `pos`, if `pos` is on a bracket.
pub fn matching_bracket(r: &ropey::Rope, pos: usize) -> Option<usize> {
    if pos >= r.len_chars() {
        return None;
    }
    let c = r.char(pos);
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0usize;
    if forward {
        for (i, ch) in r.chars_at(pos).enumerate().take(BRACKET_SCAN) {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some(pos + i);
                }
            }
        }
    } else {
        let mut it = r.chars_at(pos + 1);
        let mut i = pos + 1;
        while let Some(ch) = it.prev() {
            i -= 1;
            if pos - i > BRACKET_SCAN {
                break;
            }
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// Search matches (char ranges) that start in `range`.
pub fn search_matches(doc: &Doc, range: Range<usize>) -> Vec<Range<usize>> {
    let Some(pattern) = &doc.core.search else {
        return Vec::new();
    };
    let q = void_editor::Query {
        pattern: pattern.clone(),
        regex: true,
        case: void_editor::CaseMode::Sensitive,
        whole_word: false,
    };
    void_editor::captures_in(doc.core.buffer.rope(), &q, range)
        .into_iter()
        .filter_map(|m| m.into_iter().next().flatten())
        .filter(|m| !m.is_empty())
        .collect()
}

/// The text of `lines` for screen readers (and tests), each cut like a drawn line.
pub fn accessible_text(r: &ropey::Rope, lines: impl Iterator<Item = usize>) -> String {
    let mut out = String::new();
    for (i, l) in lines.enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = r.line(l);
        out.extend(line.chars().take(MAX_DRAWN_CHARS).filter(|c| *c != '\n'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_match_both_ways() {
        let r = ropey::Rope::from_str("f(a[1], {b})");
        assert_eq!(matching_bracket(&r, 1), Some(11));
        assert_eq!(matching_bracket(&r, 11), Some(1));
        assert_eq!(matching_bracket(&r, 3), Some(5));
        assert_eq!(matching_bracket(&r, 8), Some(10));
        assert_eq!(matching_bracket(&r, 9), None);
        assert_eq!(matching_bracket(&r, 0), None);
    }
}

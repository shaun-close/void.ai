//! Marks and the jump list (`:help mark-motions`, `:help jump-motions`, `:help jumplist`).
//!
//! Marks are char positions that move with the text: the engine adjusts them for every edit it
//! makes (and for undo and redo); edits the host makes itself are reported with
//! [`Engine::adjust_marks`].

use std::ops::Range;

use ropey::Rope;

use crate::VimHost;
use crate::engine::Engine;
use crate::motion::Motion;
use crate::text::*;

/// Most entries the jump list keeps.
const MAX_JUMPS: usize = 100;

#[derive(Debug, Default)]
pub(crate) struct Marks {
    /// `a`–`z`.
    named: [Option<usize>; 26],
    /// `'` / `` ` ``: where the cursor was before the latest jump.
    context: Option<usize>,
    jumps: Vec<usize>,
    /// Where `<C-o>` / `<C-i>` stand in `jumps`; `jumps.len()` when past the newest entry.
    at: usize,
}

/// Where position `p` goes when `old` chars at `start` become `new` chars: positions after the
/// edit shift with it, positions inside it stay put but no further than its new end.
fn shift(p: usize, start: usize, old: usize, new: usize) -> usize {
    if p < start {
        p
    } else if p >= start + old {
        p - old + new
    } else {
        p.min(start + new)
    }
}

/// Whether removing `range` from `r` takes the whole of `p`'s line, with its line break.
fn line_removed(r: &Rope, p: usize, range: &Range<usize>) -> bool {
    if p < range.start || p > range.end {
        return false;
    }
    let line = line_of(r, p);
    let (start, end) = (line_start(r, line), line_end(r, line));
    if line < last_line(r) {
        range.start <= start && range.end > end
    } else {
        line > 0 && range.start < start && range.end >= end
    }
}

/// The chars that differ between `old` and `new`: `(start, old_len, new_len)`. Compares the
/// ropes' chunks as bytes, so an undo in a big file costs a memory compare, not a char walk.
fn changed_span(old: &Rope, new: &Rope) -> Option<(usize, usize, usize)> {
    let prefix = common_bytes(old, new, false, usize::MAX);
    let (ob, nb) = (old.len_bytes(), new.len_bytes());
    if prefix == ob && prefix == nb {
        return None;
    }
    let suffix = common_bytes(old, new, true, ob.min(nb) - prefix);
    // The first differing byte may be inside a char: start at that char. The common tail may
    // start inside one: leave that char out of it (UTF-8 boundaries are the same in both).
    let start = old.byte_to_char(prefix);
    let tail_chars = |r: &Rope| {
        let at = r.len_bytes() - suffix;
        let c = r.byte_to_char(at);
        let c = if r.char_to_byte(c) < at { c + 1 } else { c };
        r.len_chars() - c
    };
    let tail = tail_chars(old);
    Some((
        start,
        old.len_chars() - tail - start,
        new.len_chars() - tail - start,
    ))
}

/// How many bytes `a` and `b` share at their start (or, `from_end`, at their end), at most `most`.
fn common_bytes<'a>(a: &'a Rope, b: &'a Rope, from_end: bool, most: usize) -> usize {
    let chunks = |r: &'a Rope| r.chunks_at_byte(if from_end { r.len_bytes() } else { 0 }).0;
    let (mut ca, mut cb) = (chunks(a), chunks(b));
    fn step<'a>(c: &mut ropey::iter::Chunks<'a>, back: bool) -> Option<&'a str> {
        if back { c.prev() } else { c.next() }
    }
    let (mut sa, mut sb): (&[u8], &[u8]) = (&[], &[]);
    let mut n = 0;
    while n < most {
        if sa.is_empty() {
            let Some(c) = step(&mut ca, from_end) else {
                break;
            };
            sa = c.as_bytes();
            continue;
        }
        if sb.is_empty() {
            let Some(c) = step(&mut cb, from_end) else {
                break;
            };
            sb = c.as_bytes();
            continue;
        }
        let k = sa.len().min(sb.len()).min(most - n);
        let (xa, xb) = if from_end {
            (&sa[sa.len() - k..], &sb[sb.len() - k..])
        } else {
            (&sa[..k], &sb[..k])
        };
        if xa != xb {
            let same = if from_end {
                xa.iter()
                    .rev()
                    .zip(xb.iter().rev())
                    .take_while(|(x, y)| x == y)
                    .count()
            } else {
                xa.iter().zip(xb).take_while(|(x, y)| x == y).count()
            };
            return n + same;
        }
        n += k;
        if from_end {
            sa = &sa[..sa.len() - k];
            sb = &sb[..sb.len() - k];
        } else {
            sa = &sa[k..];
            sb = &sb[k..];
        }
    }
    n
}

impl Marks {
    fn is_empty(&self) -> bool {
        self.named.iter().all(Option::is_none) && self.context.is_none() && self.jumps.is_empty()
    }

    fn adjust(&mut self, start: usize, old: usize, new: usize) {
        if old == 0 && new == 0 {
            return;
        }
        let f = |p: &mut usize| *p = shift(*p, start, old, new);
        self.named.iter_mut().flatten().for_each(f);
        self.context.iter_mut().for_each(f);
        self.jumps.iter_mut().for_each(f);
    }

    /// `:help mark-motions`: "If you delete a line containing a mark, the mark is deleted."
    fn forget_removed(&mut self, r: &Rope, range: &Range<usize>) {
        if range.is_empty() {
            return;
        }
        for m in &mut self.named {
            if m.is_some_and(|p| line_removed(r, p, range)) {
                *m = None;
            }
        }
    }
}

/// Motions that are jumps: the cursor position before them goes in the jump list and the `'`
/// mark.
pub(crate) fn is_jump(m: &Motion) -> bool {
    matches!(
        m,
        Motion::FirstLine
            | Motion::LastLine
            | Motion::Match
            | Motion::ParagraphForward
            | Motion::ParagraphBack
            | Motion::ScreenTop
            | Motion::ScreenMiddle
            | Motion::ScreenBottom
            | Motion::SearchNext { .. }
            | Motion::Search { .. }
            | Motion::Word { .. }
            | Motion::Mark { .. }
    )
}

/// Whether `{name}` is a mark `m`, `'` and `` ` `` know.
pub(crate) fn is_mark(name: char) -> bool {
    name.is_ascii_lowercase() || name == '\'' || name == '`'
}

impl Engine {
    /// Tell the engine about an edit the host made itself (a paste from the UI, a reload): `old_len`
    /// chars at `edit_start` became `new_len` chars. Marks and the jump list move with the text.
    pub fn adjust_marks(&mut self, edit_start: usize, old_len: usize, new_len: usize) {
        self.marks.adjust(edit_start, old_len, new_len);
    }

    /// Replace `range` with `text` in the host, moving the marks with the text.
    pub(crate) fn edit(&mut self, h: &mut dyn VimHost, range: Range<usize>, text: &str) {
        self.marks.forget_removed(h.rope(), &range);
        self.marks
            .adjust(range.start, range.len(), text.chars().count());
        h.replace(range, text);
    }

    /// A host call that inserts text at the head (`newline`, `tab`).
    pub(crate) fn host_insert(&mut self, h: &mut dyn VimHost, f: fn(&mut dyn VimHost)) {
        let (at, before) = (h.selection().1, h.rope().len_chars());
        f(h);
        let added = h.rope().len_chars().saturating_sub(before);
        self.marks.adjust(at, 0, added);
    }

    /// `n` undo or redo steps. The host's history does not say what changed, so the marks move
    /// over the span between the first and last changed chars.
    pub(crate) fn history(&mut self, h: &mut dyn VimHost, n: usize, redo: bool) {
        for _ in 0..n {
            let old = h.rope().clone();
            if redo {
                h.redo();
            } else {
                h.undo();
            }
            if self.marks.is_empty() {
                continue;
            }
            if let Some((start, old_len, new_len)) = changed_span(&old, h.rope()) {
                self.marks.adjust(start, old_len, new_len);
            }
        }
    }

    /// `m{a-z}`, and `m'` / `` m` `` (the previous context mark, which is also a jump).
    pub(crate) fn set_mark(&mut self, h: &mut dyn VimHost, name: char) {
        let cur = h.selection().1;
        match name {
            'a'..='z' => self.marks.named[(name as u8 - b'a') as usize] = Some(cur),
            _ => self.push_jump(h.rope(), cur),
        }
    }

    /// Where mark `name` is, or `None` (with Vim's message) when it is not set.
    pub(crate) fn mark(&mut self, r: &Rope, name: char) -> Option<usize> {
        let p = match name {
            'a'..='z' => self.marks.named[(name as u8 - b'a') as usize],
            _ => self.marks.context,
        };
        if p.is_none() {
            self.state.message = "E20: Mark not set".into();
        }
        p.map(|p| p.min(r.len_chars()))
    }

    /// Remember `from` before a jump: at the end of the jump list (dropping an older entry on the
    /// same line) and as the `'` mark.
    pub(crate) fn push_jump(&mut self, r: &Rope, from: usize) {
        let line = line_of(r, from);
        let m = &mut self.marks;
        m.jumps.retain(|&p| line_of(r, p) != line);
        m.jumps.push(from);
        if m.jumps.len() > MAX_JUMPS {
            m.jumps.remove(0);
        }
        m.at = m.jumps.len();
        m.context = Some(from);
    }

    /// `<C-o>` (`older`) / `<C-i>`: `n` entries along the jump list.
    pub(crate) fn jump(&mut self, h: &mut dyn VimHost, n: usize, older: bool) -> bool {
        let cur = h.selection().1;
        let r = h.rope();
        if older && self.marks.at >= self.marks.jumps.len() {
            // Leaving the newest end: keep the cursor's place so `<C-i>` comes back to it.
            self.push_jump(r, cur);
            self.marks.at = self.marks.jumps.len() - 1;
        }
        let at = self.marks.at;
        let target = if older {
            at.checked_sub(n)
        } else {
            at.checked_add(n).filter(|&t| t < self.marks.jumps.len())
        };
        let Some(target) = target else {
            return false;
        };
        self.marks.at = target;
        self.marks.context = Some(cur);
        let pos = clamp_normal(r, self.marks.jumps[target].min(r.len_chars()));
        self.want = None;
        self.set_cursor(h, pos);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_span_finds_the_edit() {
        let span = |a: &str, b: &str| changed_span(&Rope::from_str(a), &Rope::from_str(b));
        assert_eq!(span("abc", "abc"), None);
        assert_eq!(span("abcd", "aXd"), Some((1, 2, 1)));
        assert_eq!(span("aaa", "aaaa"), Some((3, 0, 1)));
        assert_eq!(span("héllo", "hllo"), Some((1, 1, 0)));
        assert_eq!(
            span("aé", "aè"),
            Some((1, 1, 1)),
            "chars sharing a first byte"
        );
        assert_eq!(
            span("xé", "xɩ"),
            Some((1, 1, 1)),
            "chars sharing a last byte"
        );
        assert_eq!(span("", "x"), Some((0, 0, 1)));
        assert_eq!(span("ab", "b"), Some((0, 1, 0)));
        // Across chunk boundaries.
        let long: String = (0..5000).map(|i| format!("{i} ")).collect();
        let mut edited = long.clone();
        edited.insert(12_345, '🦀');
        let (a, b) = (Rope::from_str(&long), Rope::from_str(&edited));
        let at = a.byte_to_char(12_345);
        assert_eq!(changed_span(&a, &b), Some((at, 0, 1)));
        assert_eq!(changed_span(&b, &a), Some((at, 1, 0)));
    }
}

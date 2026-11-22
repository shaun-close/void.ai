//! Motions (`:help motion.txt`): where each one goes, and whether an operator over it is
//! exclusive, inclusive or linewise.

use ropey::Rope;

use crate::VimHost;
use crate::engine::{Curswant, Engine};
use crate::text::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Find {
    pub forward: bool,
    pub till: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Motion {
    Left,
    Right,
    /// `<BS>`: left, continuing on the previous line.
    BackWrap,
    /// `<Space>`: right, continuing on the next line.
    ForwardWrap,
    Down,
    Up,
    WordStart {
        big: bool,
    },
    WordBack {
        big: bool,
    },
    WordEnd {
        big: bool,
    },
    WordEndBack {
        big: bool,
    },
    LineStart,
    FirstNonBlank,
    LineEnd,
    LastNonBlank,
    Column,
    FirstLine,
    LastLine,
    Find(Find, char),
    RepeatFind {
        reverse: bool,
    },
    /// `%`: the matching bracket, or with a count that percentage of the file.
    Match,
    ParagraphForward,
    ParagraphBack,
    NextLine,
    PrevLine,
    CurrentLine,
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    SearchNext {
        reverse: bool,
    },
    /// `*`, `#`, `g*`, `g#`: the word under the cursor.
    Word {
        forward: bool,
        whole: bool,
    },
    Search {
        pattern: String,
        forward: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// What a motion does to the remembered column (`curswant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// Forget it: the next `j`/`k` uses the column the motion landed on.
    Col,
    /// Keep it (`j`, `k`).
    Keep,
    /// Stick to the end of the line (`$`).
    End,
}

/// Where a motion lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reach {
    pub pos: usize,
    pub kind: Kind,
    /// How the remembered column (`curswant`) changes.
    pub want: Want,
}

impl Reach {
    fn new(pos: usize, kind: Kind) -> Option<Reach> {
        Some(Reach {
            pos,
            kind,
            want: Want::Col,
        })
    }
}

impl Engine {
    /// Where motion `m` goes from the cursor, or `None` when it cannot move (Vim beeps, and a
    /// macro stops). `op` is set when an operator is waiting for the motion.
    pub(crate) fn reach(
        &mut self,
        h: &mut dyn VimHost,
        m: &Motion,
        count: Option<usize>,
        op: bool,
    ) -> Option<Reach> {
        let cur = h.selection().1;
        let n = count.unwrap_or(1);
        match m {
            Motion::SearchNext { reverse } => {
                let (pattern, forward) = self.last_search.clone().or_else(|| {
                    self.state.message = "E35: No previous regular expression".into();
                    None
                })?;
                let pos = self.find(h, &pattern, forward != *reverse, cur, n)?;
                return Reach::new(pos, Kind::Exclusive);
            }
            Motion::Search { pattern, forward } => {
                let pos = self.find(h, pattern, *forward, cur, n)?;
                return Reach::new(pos, Kind::Exclusive);
            }
            Motion::Word { forward, whole } => {
                let (start, pattern) = word_pattern(h.rope(), cur, *whole)?;
                self.last_search = Some((pattern.clone(), *forward));
                let pos = self.find(h, &pattern, *forward, start, n)?;
                return Reach::new(pos, Kind::Exclusive);
            }
            Motion::RepeatFind { reverse } => {
                let (f, c) = self.last_find?;
                let f = Find {
                    forward: f.forward != *reverse,
                    till: f.till,
                };
                let pos = find_in_line(h.rope(), cur, c, f, n, f.till)?;
                let kind = if f.forward {
                    Kind::Inclusive
                } else {
                    Kind::Exclusive
                };
                return Reach::new(pos, kind);
            }
            Motion::Find(f, c) => {
                self.last_find = Some((*f, *c));
            }
            _ => {}
        }
        let visual = self.state.mode.is_visual();
        let r = h.rope();
        let line = line_of(r, cur);
        let last = last_line(r);
        let lines = |pos: usize| Reach::new(pos, Kind::Linewise);
        match m {
            Motion::Left => {
                let start = line_start(r, line);
                (cur > start).then_some(())?;
                Reach::new(cur.saturating_sub(n).max(start), Kind::Exclusive)
            }
            Motion::Right => {
                let end = if op {
                    line_end(r, line)
                } else {
                    line_last(r, line)
                };
                (cur < end).then_some(())?;
                Reach::new((cur + n).min(end), Kind::Exclusive)
            }
            Motion::BackWrap | Motion::ForwardWrap => {
                let mut p = cur;
                for _ in 0..n {
                    let l = line_of(r, p);
                    p = if *m == Motion::BackWrap {
                        if p > line_start(r, l) {
                            p - 1
                        } else if l > 0 {
                            line_last(r, l - 1)
                        } else {
                            break;
                        }
                    } else if p < line_last(r, l) {
                        p + 1
                    } else if l < last {
                        line_start(r, l + 1)
                    } else {
                        break;
                    };
                }
                (p != cur).then_some(())?;
                Reach::new(p, Kind::Exclusive)
            }
            Motion::Down | Motion::Up => {
                let target = if *m == Motion::Down {
                    (line < last).then_some(())?;
                    (line + n).min(last)
                } else {
                    (line > 0).then_some(())?;
                    line.saturating_sub(n)
                };
                Some(Reach {
                    pos: self.col_in(r, target, cur),
                    kind: Kind::Linewise,
                    want: Want::Keep,
                })
            }
            Motion::WordStart { big } => {
                let (mut p, mut prev) = (cur, cur);
                for _ in 0..n {
                    let next = next_word_start(r, p, *big);
                    if next == p {
                        break;
                    }
                    (prev, p) = (p, next);
                }
                // `:help word`: with an operator, a last word at the end of a line stops there.
                let pl = line_of(r, prev);
                if op && line_of(r, p) > pl && line_len(r, pl) > 0 {
                    p = line_end(r, pl).max(cur);
                }
                if !op {
                    p = clamp_normal(r, p);
                }
                (p != cur).then_some(())?;
                Reach::new(p, Kind::Exclusive)
            }
            Motion::WordBack { big } => {
                let p = (0..n).fold(cur, |p, _| prev_word_start(r, p, *big));
                (p != cur).then_some(())?;
                Reach::new(p, Kind::Exclusive)
            }
            Motion::WordEnd { big } => {
                let p = (0..n).fold(cur, |p, _| word_end(r, p, *big));
                (p != cur).then_some(())?;
                Reach::new(p, Kind::Inclusive)
            }
            Motion::WordEndBack { big } => {
                let p = (0..n).fold(cur, |p, _| prev_word_end(r, p, *big));
                (p != cur).then_some(())?;
                Reach::new(p, Kind::Inclusive)
            }
            Motion::LineStart => Reach::new(line_start(r, line), Kind::Exclusive),
            Motion::FirstNonBlank => Reach::new(first_non_blank(r, line), Kind::Exclusive),
            Motion::LineEnd => {
                let l = (line + n - 1).min(last);
                let pos = if visual {
                    line_end(r, l)
                } else {
                    line_last(r, l)
                };
                Some(Reach {
                    pos,
                    kind: Kind::Inclusive,
                    want: Want::End,
                })
            }
            Motion::LastNonBlank => {
                let l = (line + n - 1).min(last);
                let start = line_start(r, l);
                let mut p = line_last(r, l);
                while p > start && is_blank(r.char(p)) {
                    p -= 1;
                }
                Reach::new(p, Kind::Inclusive)
            }
            Motion::Column => {
                let end = if op {
                    line_end(r, line)
                } else {
                    line_last(r, line)
                };
                Reach::new((line_start(r, line) + n - 1).min(end), Kind::Exclusive)
            }
            Motion::FirstLine | Motion::LastLine => {
                let default = if *m == Motion::FirstLine { 0 } else { last };
                let l = count.map_or(default, |c| (c - 1).min(last));
                lines(first_non_blank(r, l))
            }
            Motion::Find(f, c) => {
                let pos = find_in_line(r, cur, *c, *f, n, false)?;
                let kind = if f.forward {
                    Kind::Inclusive
                } else {
                    Kind::Exclusive
                };
                Reach::new(pos, kind)
            }
            Motion::Match => match count {
                Some(c) if c <= 100 => {
                    let l = ((c * r.len_lines()).div_ceil(100))
                        .saturating_sub(1)
                        .min(last);
                    lines(first_non_blank(r, l))
                }
                Some(_) => None,
                None => Reach::new(match_bracket(r, cur)?, Kind::Inclusive),
            },
            Motion::ParagraphForward => {
                let mut l = line;
                for _ in 0..n {
                    while l < last && line_len(r, l) == 0 {
                        l += 1;
                    }
                    while l < last && line_len(r, l) > 0 {
                        l += 1;
                    }
                }
                let pos = if line_len(r, l) == 0 {
                    line_start(r, l)
                } else if op {
                    r.len_chars()
                } else {
                    line_last(r, l)
                };
                (pos != cur).then_some(())?;
                Reach::new(pos, Kind::Exclusive)
            }
            Motion::ParagraphBack => {
                let mut l = line;
                for _ in 0..n {
                    while l > 0 && line_len(r, l) == 0 {
                        l -= 1;
                    }
                    while l > 0 && line_len(r, l) > 0 {
                        l -= 1;
                    }
                }
                let pos = line_start(r, l);
                (pos != cur).then_some(())?;
                Reach::new(pos, Kind::Exclusive)
            }
            Motion::NextLine => {
                (line + n <= last).then_some(())?;
                lines(first_non_blank(r, line + n))
            }
            Motion::PrevLine => {
                (line >= n).then_some(())?;
                lines(first_non_blank(r, line - n))
            }
            Motion::CurrentLine => lines(first_non_blank(r, (line + n - 1).min(last))),
            Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
                let (top, bottom) = h.visible_lines();
                let bottom = bottom.min(last);
                let top = top.min(bottom);
                let l = match m {
                    Motion::ScreenTop => (top + n - 1).min(bottom),
                    Motion::ScreenBottom => bottom.saturating_sub(n - 1).max(top),
                    _ => (top + bottom) / 2,
                };
                lines(first_non_blank(r, l))
            }
            Motion::SearchNext { .. }
            | Motion::Search { .. }
            | Motion::Word { .. }
            | Motion::RepeatFind { .. } => unreachable!("handled above"),
        }
    }

    /// The position on `line` at the remembered column.
    fn col_in(&self, r: &Rope, line: usize, cur: usize) -> usize {
        let want = match self.want {
            Some((w, at)) if at == cur => w,
            _ => Curswant::Column(col(r, cur)),
        };
        match want {
            Curswant::End if self.state.mode.is_visual() => line_end(r, line),
            Curswant::End => line_last(r, line),
            Curswant::Column(c) => (line_start(r, line) + c).min(line_last(r, line)),
        }
    }
}

/// `w`: the start of the next word, an empty line, or the end of the text.
pub(crate) fn next_word_start(r: &Rope, p: usize, big: bool) -> usize {
    let len = r.len_chars();
    if p >= len {
        return p;
    }
    let mut q = p;
    if is_empty_line_at(r, q) {
        q += 1;
    } else {
        let k = class(r.char(q), big);
        if k != 0 {
            while q < len && r.char(q) != '\n' && class(r.char(q), big) == k {
                q += 1;
            }
        }
    }
    loop {
        if q >= len {
            return len;
        }
        if q != p && is_empty_line_at(r, q) {
            return q;
        }
        if class(r.char(q), big) == 0 {
            q += 1;
            continue;
        }
        return q;
    }
}

/// `b`: the start of this or the previous word (or an empty line).
pub(crate) fn prev_word_start(r: &Rope, p: usize, big: bool) -> usize {
    if p == 0 {
        return 0;
    }
    let mut q = p - 1;
    loop {
        if is_empty_line_at(r, q) {
            return q;
        }
        if class(r.char(q), big) != 0 {
            break;
        }
        if q == 0 {
            return 0;
        }
        q -= 1;
    }
    let k = class(r.char(q), big);
    while q > 0 && r.char(q - 1) != '\n' && class(r.char(q - 1), big) == k {
        q -= 1;
    }
    q
}

/// `e`: the end of this or the next word (empty lines are skipped).
pub(crate) fn word_end(r: &Rope, p: usize, big: bool) -> usize {
    let len = r.len_chars();
    if p + 1 >= len {
        return p;
    }
    let mut q = p + 1;
    while q < len && class(r.char(q), big) == 0 {
        q += 1;
    }
    if q >= len {
        return len - 1;
    }
    let k = class(r.char(q), big);
    while q + 1 < len && r.char(q + 1) != '\n' && class(r.char(q + 1), big) == k {
        q += 1;
    }
    q
}

/// `ge`: the end of the previous word (or an empty line).
pub(crate) fn prev_word_end(r: &Rope, p: usize, big: bool) -> usize {
    let mut q = p;
    if let Some(c) = char_at(r, q)
        && class(c, big) != 0
    {
        let k = class(c, big);
        while q > 0 && r.char(q - 1) != '\n' && class(r.char(q - 1), big) == k {
            q -= 1;
        }
    }
    if q == 0 {
        return 0;
    }
    q -= 1;
    loop {
        if is_empty_line_at(r, q) || class(r.char(q), big) != 0 {
            return q;
        }
        if q == 0 {
            return 0;
        }
        q -= 1;
    }
}

/// `f`/`t`/`F`/`T`: the `n`th `c` in the cursor's line. `skip_adjacent` is for `;` after `t`,
/// which does not stop right before a `c` next to the cursor.
fn find_in_line(r: &Rope, cur: usize, c: char, f: Find, n: usize, skip: bool) -> Option<usize> {
    let line = line_of(r, cur);
    let (start, end) = (line_start(r, line), line_end(r, line));
    let skip = usize::from(skip && f.till);
    let mut seen = 0;
    if f.forward {
        let from = cur + 1 + skip;
        if from >= end {
            return None;
        }
        for (i, ch) in r.slice(from..end).chars().enumerate() {
            if ch == c {
                seen += 1;
                if seen == n {
                    let p = from + i;
                    return Some(if f.till { p - 1 } else { p });
                }
            }
        }
    } else {
        let to = cur.checked_sub(skip)?;
        if to <= start {
            return None;
        }
        let mut it = r.slice(start..to).chars_at(to - start);
        let mut p = to;
        while let Some(ch) = it.prev() {
            p -= 1;
            if ch == c {
                seen += 1;
                if seen == n {
                    return Some(if f.till { p + 1 } else { p });
                }
            }
        }
    }
    None
}

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// `%`: the bracket matching the first one at or after the cursor on its line.
fn match_bracket(r: &Rope, cur: usize) -> Option<usize> {
    let end = line_end(r, line_of(r, cur));
    let (p, c) = r
        .slice(cur..end)
        .chars()
        .enumerate()
        .find(|(_, c)| "()[]{}".contains(*c))
        .map(|(i, c)| (cur + i, c))?;
    let (open, close) = *PAIRS.iter().find(|(o, cl)| *o == c || *cl == c)?;
    if c == open {
        find_close(r, p + 1, open, close)
    } else {
        find_open(r, p, open, close)
    }
}

/// The `close` that balances an `open` just before `from`.
pub(crate) fn find_close(r: &Rope, from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    for (i, ch) in r.chars_at(from).enumerate() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(from + i);
            }
        }
    }
    None
}

/// The unbalanced `open` before `before`.
pub(crate) fn find_open(r: &Rope, before: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1;
    let mut it = r.chars_at(before);
    let mut p = before;
    while let Some(ch) = it.prev() {
        p -= 1;
        if ch == close {
            depth += 1;
        } else if ch == open {
            depth -= 1;
            if depth == 0 {
                return Some(p);
            }
        }
    }
    None
}

/// The word under or after the cursor for `*` / `#`: where it starts, and the pattern.
fn word_pattern(r: &Rope, cur: usize, whole: bool) -> Option<(usize, String)> {
    let end = line_end(r, line_of(r, cur));
    let start = line_start(r, line_of(r, cur));
    let first = (cur..end)
        .find(|&q| is_keyword(r.char(q)))
        .or_else(|| (cur..end).find(|&q| !r.char(q).is_whitespace()))?;
    let keyword = is_keyword(r.char(first));
    let same = |c: char| {
        if keyword {
            is_keyword(c)
        } else {
            !c.is_whitespace() && !is_keyword(c)
        }
    };
    let mut s = first;
    while s > start && same(r.char(s - 1)) {
        s -= 1;
    }
    let mut e = first;
    while e < end && same(r.char(e)) {
        e += 1;
    }
    let word = escape(&r.slice(s..e).to_string());
    let pattern = if whole && keyword {
        format!("\\<{word}\\>")
    } else {
        word
    };
    Some((s, pattern))
}

/// Escape regex metacharacters.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

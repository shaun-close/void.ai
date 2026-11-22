//! Operators and the other changes made from Normal and Visual mode (`:help change.txt`):
//! delete, change, yank, shift, case, put, join, replace.

use std::ops::Range;

use ropey::Rope;

use crate::engine::Engine;
use crate::motion::{Kind, Reach};
use crate::parse::{Cmd, Extent, Op};
use crate::registers::{Register, Write};
use crate::text::*;
use crate::{Mode, VimHost};

/// The text an operator works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Region {
    Chars(Range<usize>),
    /// First and last line, inclusive.
    Lines(usize, usize),
}

/// The region from `from` to where a motion lands, with Vim's rules for exclusive motions that
/// end in column 0 (`:help exclusive`).
pub(crate) fn motion_region(r: &Rope, from: usize, reach: Reach) -> Region {
    let (a, b) = (from.min(reach.pos), from.max(reach.pos));
    match reach.kind {
        Kind::Linewise => Region::Lines(line_of(r, a), line_of(r, b)),
        Kind::Inclusive => {
            let end = if matches!(char_at(r, b), Some(c) if c != '\n') {
                b + 1
            } else {
                b
            };
            Region::Chars(a..end)
        }
        Kind::Exclusive => {
            let (la, lb) = (line_of(r, a), line_of(r, b));
            if b > a && lb > la && b == line_start(r, lb) {
                if a <= line_start(r, la) + indent_len(r, la) {
                    return Region::Lines(la, lb - 1);
                }
                return Region::Chars(a..line_end(r, lb - 1));
            }
            Region::Chars(a..b)
        }
    }
}

/// The selection of a Visual mode, inclusive at both ends.
pub(crate) fn visual_region(r: &Rope, mode: Mode, anchor: usize, head: usize) -> Region {
    let (a, b) = (anchor.min(head), anchor.max(head));
    if mode == Mode::VisualLine {
        return Region::Lines(line_of(r, a), line_of(r, b));
    }
    Region::Chars(a..(b + 1).min(r.len_chars()))
}

pub(crate) fn visual_extent(r: &Rope, mode: Mode, anchor: usize, head: usize) -> Extent {
    let (a, b) = (anchor.min(head), anchor.max(head));
    let (la, lb) = (line_of(r, a), line_of(r, b));
    match mode {
        Mode::VisualLine => Extent::Lines(lb - la + 1),
        _ if la == lb => Extent::Chars(b - a + 1),
        _ => Extent::Span(lb - la, col(r, b)),
    }
}

/// The region a repeated Visual operator covers from `cur`.
pub(crate) fn extent_region(r: &Rope, cur: usize, extent: Extent) -> Region {
    let line = line_of(r, cur);
    let last = last_line(r);
    match extent {
        Extent::Lines(n) => Region::Lines(line, (line + n - 1).min(last)),
        Extent::Chars(n) => Region::Chars(cur..(cur + n).min(line_end(r, line).max(cur + 1))),
        Extent::Span(dl, c) => {
            let l = (line + dl).min(last);
            let end = (line_start(r, l) + c).min(line_end(r, l));
            Region::Chars(cur..(end + 1).min(r.len_chars()))
        }
    }
}

/// The chars a region covers (for lines: through the last line's break, if it has one), its
/// text as a register keeps it, and whether it is linewise.
fn region_text(r: &Rope, region: &Region) -> (Range<usize>, Register) {
    match *region {
        Region::Chars(ref range) => (
            range.clone(),
            Register {
                text: r.slice(range.clone()).to_string(),
                linewise: false,
            },
        ),
        Region::Lines(l1, l2) => {
            let start = line_start(r, l1);
            let end = if l2 < last_line(r) {
                line_start(r, l2 + 1)
            } else {
                r.len_chars()
            };
            let mut text = r.slice(start..end).to_string();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            (
                start..end,
                Register {
                    text,
                    linewise: true,
                },
            )
        }
    }
}

impl Engine {
    pub(crate) fn store(
        &mut self,
        h: &mut dyn VimHost,
        name: Option<char>,
        reg: Register,
        w: Write,
    ) {
        if matches!(name, Some('+' | '*')) {
            h.clipboard_set(&reg.text);
        }
        self.registers.store(name, reg, w);
    }

    pub(crate) fn read_register(
        &mut self,
        h: &dyn VimHost,
        name: Option<char>,
    ) -> Option<Register> {
        let name = name.unwrap_or('"');
        let reg = match name {
            '+' | '*' => h.clipboard_get().map(|text| Register {
                linewise: text.ends_with('\n'),
                text,
            }),
            c => self.registers.get(c).cloned(),
        };
        if reg.is_none() {
            self.state.message = format!("E353: Nothing in register {name}");
        }
        reg
    }

    /// Run operator `op` over `region`. `times` repeats a shift (`3>` in Visual mode). Leaves
    /// Insert mode open for `c`; every other operator is one undo step.
    pub(crate) fn apply(
        &mut self,
        h: &mut dyn VimHost,
        cmd: &Cmd,
        op: Op,
        region: Region,
        times: usize,
    ) -> bool {
        let r = h.rope();
        let cur = h.selection().1;
        // Only the operators that fill a register copy the text.
        let text = || region_text(r, &region);
        match op {
            Op::Yank => {
                let (_, reg) = text();
                let pos = match region {
                    Region::Chars(ref c) => c.start,
                    Region::Lines(l1, _) if line_of(r, cur) > l1 => {
                        (line_start(r, l1) + col(r, cur)).min(line_last(r, l1))
                    }
                    Region::Lines(..) => cur,
                };
                self.store(h, cmd.reg, reg, Write::Yank);
                self.set_cursor(h, pos);
            }
            Op::Delete => {
                let (range, reg) = text();
                if range.is_empty() {
                    return true;
                }
                self.store(h, cmd.reg, reg, Write::Delete);
                self.begin(h);
                match region {
                    Region::Lines(l1, l2) => {
                        self.delete_lines(h, l1, l2);
                        let r = h.rope();
                        let pos = first_non_blank(r, l1.min(last_line(r)));
                        self.set_cursor(h, pos);
                    }
                    Region::Chars(range) => {
                        h.replace(range.clone(), "");
                        self.set_cursor(h, range.start);
                    }
                }
                self.end(h);
            }
            Op::Change => {
                let (_, reg) = text();
                // Linewise: keep one line, with the first line's indent (autoindent).
                let (range, text) = match region {
                    Region::Lines(l1, l2) => (line_start(r, l1)..line_end(r, l2), indent(r, l1)),
                    Region::Chars(range) => (range, String::new()),
                };
                self.store(h, cmd.reg, reg, Write::Delete);
                self.begin(h);
                h.replace(range.clone(), &text);
                let pos = range.start + text.chars().count();
                self.set_cursor(h, pos);
                self.start_insert(Mode::Insert, Some(cmd.clone()), 1, false);
            }
            Op::Indent | Op::Outdent => {
                let (l1, l2) = match region {
                    Region::Lines(l1, l2) => (l1, l2),
                    Region::Chars(c) => {
                        (line_of(r, c.start), line_of(r, c.end.max(c.start + 1) - 1))
                    }
                };
                self.begin(h);
                self.shift(h, l1, l2, op == Op::Indent, times);
                let pos = first_non_blank(h.rope(), l1);
                self.set_cursor(h, pos);
                self.end(h);
            }
            Op::Toggle | Op::Lower | Op::Upper => {
                let range = match region {
                    Region::Lines(l1, l2) => line_start(r, l1)..line_end(r, l2),
                    Region::Chars(c) => c,
                };
                let old = r.slice(range.clone()).to_string();
                let new: String = old
                    .chars()
                    .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
                        let upper = match op {
                            Op::Upper => true,
                            Op::Lower => false,
                            _ => c.is_lowercase(),
                        };
                        if upper {
                            Box::new(c.to_uppercase())
                        } else {
                            Box::new(c.to_lowercase())
                        }
                    })
                    .collect();
                self.begin(h);
                if new != old {
                    h.replace(range.clone(), &new);
                }
                self.set_cursor(h, range.start);
                self.end(h);
            }
        }
        true
    }

    /// Remove whole lines, with the line break that joins them to their neighbours.
    pub(crate) fn delete_lines(&mut self, h: &mut dyn VimHost, l1: usize, l2: usize) {
        let r = h.rope();
        let range = if l2 < last_line(r) {
            line_start(r, l1)..line_start(r, l2 + 1)
        } else if l1 > 0 {
            line_end(r, l1 - 1)..r.len_chars()
        } else {
            0..r.len_chars()
        };
        h.replace(range, "");
    }

    fn shift(&mut self, h: &mut dyn VimHost, l1: usize, l2: usize, right: bool, times: usize) {
        let unit = h.indent_unit().to_string();
        let width = if unit == "\t" {
            4
        } else {
            unit.chars().count().max(1)
        };
        for l in l1..=l2 {
            let r = h.rope();
            let start = line_start(r, l);
            if right {
                if line_len(r, l) > 0 {
                    h.replace(start..start, &unit.repeat(times));
                }
                continue;
            }
            let mut chars = r.line(l).chars().peekable();
            let mut remove = 0;
            for _ in 0..times {
                if chars.peek() == Some(&'\t') {
                    chars.next();
                    remove += 1;
                    continue;
                }
                let mut n = 0;
                while n < width && chars.peek() == Some(&' ') {
                    chars.next();
                    n += 1;
                }
                remove += n;
            }
            if remove > 0 {
                h.replace(start..start + remove, "");
            }
        }
    }

    /// `p`, `P`, `gp`, `gP`.
    pub(crate) fn put(
        &mut self,
        h: &mut dyn VimHost,
        cmd: &Cmd,
        before: bool,
        after: bool,
    ) -> bool {
        let Some(reg) = self.read_register(h, cmd.reg) else {
            return false;
        };
        let n = cmd.count.unwrap_or(1);
        let cur = h.selection().1;
        self.begin(h);
        let r = h.rope();
        let line = line_of(r, cur);
        let pos = if reg.linewise {
            let mut text = reg.text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let text = text.repeat(n);
            let lines = text.matches('\n').count();
            let first = if before { line } else { line + 1 };
            if before || line < last_line(r) {
                let at = line_start(r, first);
                h.replace(at..at, &text);
            } else {
                let at = r.len_chars();
                h.replace(at..at, &format!("\n{}", &text[..text.len() - 1]));
            }
            let r = h.rope();
            if after {
                line_start(r, (first + lines).min(last_line(r)))
            } else {
                first_non_blank(r, first)
            }
        } else {
            let text = reg.text.repeat(n);
            let at = if before || line_len(r, line) == 0 {
                cur
            } else {
                cur + 1
            };
            h.replace(at..at, &text);
            let len = text.chars().count();
            if after {
                at + len
            } else if text.contains('\n') {
                at
            } else {
                at + len.saturating_sub(1)
            }
        };
        self.set_cursor(h, pos);
        self.end(h);
        true
    }

    /// Visual `p` / `P`: replace the selection with a register. `p` puts the replaced text in the
    /// unnamed register; `P` leaves the registers alone.
    pub(crate) fn visual_put(&mut self, h: &mut dyn VimHost, cmd: &Cmd, keep: bool) -> bool {
        let Some(reg) = self.read_register(h, cmd.reg) else {
            self.exit_visual(h);
            return false;
        };
        let (anchor, head) = h.selection();
        let r = h.rope();
        let region = visual_region(r, self.state.mode, anchor, head);
        let (_, old) = region_text(r, &region);
        let at_end = matches!(region, Region::Lines(_, l2) if l2 == last_line(r));
        let n = cmd.count.unwrap_or(1);
        self.exit_visual(h);
        self.begin(h);
        let pos = match region {
            Region::Lines(l1, l2) => {
                let mut text = reg.text.clone();
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                let text = text.repeat(n);
                self.delete_lines(h, l1, l2);
                let len = h.rope().len_chars();
                if at_end && l1 > 0 {
                    h.replace(len..len, &format!("\n{}", &text[..text.len() - 1]));
                } else if at_end {
                    h.replace(0..len, &text[..text.len() - 1]);
                } else {
                    let at = line_start(h.rope(), l1);
                    h.replace(at..at, &text);
                }
                first_non_blank(h.rope(), l1)
            }
            Region::Chars(range) => {
                let text = if reg.linewise {
                    format!("\n{}", reg.text)
                } else {
                    reg.text.repeat(n)
                };
                h.replace(range.clone(), &text);
                if reg.linewise {
                    range.start + 1
                } else {
                    range.start + text.chars().count().saturating_sub(1)
                }
            }
        };
        if !keep {
            self.store(h, None, old, Write::Delete);
        }
        self.set_cursor(h, pos);
        self.end(h);
        true
    }

    /// `J` / `gJ`: join `count` lines (at least two) starting at `line`.
    pub(crate) fn join(
        &mut self,
        h: &mut dyn VimHost,
        line: usize,
        count: usize,
        spaces: bool,
    ) -> bool {
        let last = last_line(h.rope());
        if line >= last {
            return false;
        }
        let joins = (count.max(2) - 1).min(last - line);
        self.begin(h);
        let mut pos = h.selection().1;
        for _ in 0..joins {
            let r = h.rope();
            let start = line_start(r, line);
            let end = line_end(r, line);
            let next = line_start(r, line + 1);
            let (to, insert) = if spaces {
                let skip = indent_len(r, line + 1);
                let first = char_at(r, next + skip).filter(|c| *c != '\n');
                let ends_blank = end > start && is_blank(r.char(end - 1));
                let space = !(first.is_none() || first == Some(')') || ends_blank || end == start);
                (next + skip, if space { " " } else { "" })
            } else {
                (next, "")
            };
            h.replace(end..to, insert);
            pos = end;
        }
        self.set_cursor(h, pos);
        self.end(h);
        true
    }

    /// `r{char}`: replace `n` chars under the cursor; a line break replaces them with one break.
    pub(crate) fn replace_chars(&mut self, h: &mut dyn VimHost, c: char, n: usize) -> bool {
        let r = h.rope();
        let cur = h.selection().1;
        if cur + n > line_end(r, line_of(r, cur)) {
            return false;
        }
        self.begin(h);
        let pos = if c == '\n' {
            h.replace(cur..cur + n, "\n");
            cur + 1
        } else {
            h.replace(cur..cur + n, &c.to_string().repeat(n));
            cur + n - 1
        };
        self.set_cursor(h, pos);
        self.end(h);
        true
    }

    /// Visual `r{char}`: every char of the selection except line breaks.
    pub(crate) fn visual_replace(&mut self, h: &mut dyn VimHost, c: char) -> bool {
        let (anchor, head) = h.selection();
        let r = h.rope();
        let range = match visual_region(r, self.state.mode, anchor, head) {
            Region::Lines(l1, l2) => line_start(r, l1)..line_end(r, l2),
            Region::Chars(range) => range,
        };
        let new: String = r
            .slice(range.clone())
            .chars()
            .map(|ch| if ch == '\n' { ch } else { c })
            .collect();
        self.exit_visual(h);
        self.begin(h);
        h.replace(range.clone(), &new);
        self.set_cursor(h, range.start);
        self.end(h);
        true
    }

    /// `~`: switch the case of `n` chars and move past them.
    pub(crate) fn toggle_chars(&mut self, h: &mut dyn VimHost, n: usize) -> bool {
        let r = h.rope();
        let cur = h.selection().1;
        let end = (cur + n).min(line_end(r, line_of(r, cur)));
        if end == cur {
            return false;
        }
        let new: String = r
            .slice(cur..end)
            .chars()
            .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
                if c.is_lowercase() {
                    Box::new(c.to_uppercase())
                } else {
                    Box::new(c.to_lowercase())
                }
            })
            .collect();
        self.begin(h);
        h.replace(cur..end, &new);
        let pos = cur + new.chars().count();
        self.set_cursor(h, pos);
        self.end(h);
        true
    }
}

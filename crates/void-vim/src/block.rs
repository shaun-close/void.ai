//! Visual-block mode (`:help blockwise-operators`, `v_b_I`, `v_b_A`, `v_b_$`, `v_b_r`, `v_b_<`,
//! `v_b_>`, `v_b_c`, `v_b_D`): commands over a rectangle of lines and columns.
//!
//! Columns are char columns, not display columns: a tab or a double-width char counts as one.

use std::ops::Range;

use ropey::Rope;

use crate::engine::{Curswant, Engine};
use crate::operator::{outdent_len, recase};
use crate::parse::{Act, Cmd, Op};
use crate::registers::{RegKind, Register, Write};
use crate::text::*;
use crate::{Mode, VimHost};

/// A Visual-block selection: lines `top..=bottom` and char columns `left..=right`, or every line
/// to its end when `to_end` (after `$`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
    pub to_end: bool,
}

/// What a block command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockOp {
    Op(Op),
    /// `I`.
    Insert,
    /// `A`.
    Append,
    /// `r{char}`.
    Replace(char),
}

/// A block's shape without its place, so `.` covers the same amount from the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockSize {
    lines: usize,
    width: usize,
    to_end: bool,
}

/// A block `I`, `A` or `c` in progress: on `<Esc>` the text typed on the top line is copied to
/// `lines`.
#[derive(Debug)]
pub(crate) struct BlockInsert {
    lines: Vec<usize>,
    /// The column the text goes in (`None`: each line's end).
    col: Option<usize>,
    /// Pad lines shorter than `col` with spaces (`A`) rather than skip them.
    pad: bool,
    /// Where typing started.
    start: usize,
    /// The text's line count then: a line break typed in the insert stops the copy.
    line_count: usize,
}

impl Block {
    /// The chars of `line` inside the block (empty when the line ends before it).
    pub(crate) fn span(&self, r: &Rope, line: usize) -> Range<usize> {
        let start = line_start(r, line);
        let len = line_len(r, line);
        let from = start + self.left.min(len);
        let to = if self.to_end {
            start + len
        } else {
            start + (self.right + 1).min(len)
        };
        from..to.max(from)
    }

    fn top_left(&self, r: &Rope) -> usize {
        clamp_normal(r, line_start(r, self.top) + self.left)
    }

    fn size(&self) -> BlockSize {
        BlockSize {
            lines: self.bottom - self.top,
            width: self.right - self.left + 1,
            to_end: self.to_end,
        }
    }
}

impl Engine {
    /// The Visual-block selection, for drawing; `None` outside Visual-block mode.
    pub fn visual_block(&self, host: &impl VimHost) -> Option<Block> {
        (self.state.mode == Mode::VisualBlock).then(|| {
            let (anchor, head) = host.selection();
            self.block_of(host.rope(), anchor, head)
        })
    }

    pub(crate) fn block_of(&self, r: &Rope, anchor: usize, head: usize) -> Block {
        let (la, lh) = (line_of(r, anchor), line_of(r, head));
        let (ca, ch) = (col(r, anchor), col(r, head));
        Block {
            top: la.min(lh),
            bottom: la.max(lh),
            left: ca.min(ch),
            right: ca.max(ch),
            to_end: matches!(self.want, Some((Curswant::End, at)) if at == head),
        }
    }

    /// Run a block command on the Visual-block selection; `to_end` makes it run to each line's
    /// end (`D`, `C`).
    pub(crate) fn block_op(
        &mut self,
        h: &mut dyn VimHost,
        cmd: &Cmd,
        op: BlockOp,
        to_end: bool,
    ) -> bool {
        let (anchor, head) = h.selection();
        let mut block = self.block_of(h.rope(), anchor, head);
        block.to_end |= to_end;
        self.exit_visual(h);
        self.run_block(h, cmd, op, block)
    }

    /// `.` after a block command: the same shape from the cursor.
    pub(crate) fn block_repeat(
        &mut self,
        h: &mut dyn VimHost,
        cmd: &Cmd,
        op: BlockOp,
        size: BlockSize,
    ) -> bool {
        let r = h.rope();
        let cur = h.selection().1;
        let top = line_of(r, cur);
        let left = col(r, cur);
        let block = Block {
            top,
            bottom: (top + size.lines).min(last_line(r)),
            left,
            right: left + size.width - 1,
            to_end: size.to_end,
        };
        self.run_block(h, cmd, op, block)
    }

    fn run_block(&mut self, h: &mut dyn VimHost, cmd: &Cmd, op: BlockOp, b: Block) -> bool {
        // `r<CR>` would split every line of the block; it fails instead of guessing.
        if op == BlockOp::Replace('\n') {
            return false;
        }
        let repeat = Cmd {
            reg: cmd.reg,
            count: cmd.count,
            act: Act::BlockRepeat(op, b.size()),
        };
        let top_left = b.top_left(h.rope());
        h.select(top_left, top_left);
        self.want = None;
        if let BlockOp::Op(Op::Yank | Op::Delete | Op::Change) = op {
            let r = h.rope();
            let text: Vec<String> = (b.top..=b.bottom)
                .map(|l| r.slice(b.span(r, l)).to_string())
                .collect();
            let reg = Register {
                text: text.join("\n"),
                kind: RegKind::Block,
            };
            let w = if op == BlockOp::Op(Op::Yank) {
                Write::Yank
            } else {
                Write::Delete
            };
            self.store(h, cmd.reg, reg, w);
            if op == BlockOp::Op(Op::Yank) {
                return true;
            }
        }
        self.begin(h);
        let r = h.rope();
        match op {
            BlockOp::Op(Op::Change) | BlockOp::Insert => {
                let lines = self.lines_reaching(r, &b);
                if op != BlockOp::Insert {
                    self.each_line(h, &b, |_| String::new());
                }
                let r = h.rope();
                let at = line_start(r, b.top) + b.left.min(line_len(r, b.top));
                h.select(at, at);
                self.start_block_insert(h, repeat, lines, Some(b.left), false);
                return true;
            }
            BlockOp::Append => {
                let lines = (b.top + 1..=b.bottom).collect();
                if b.to_end {
                    let end = line_end(r, b.top);
                    h.select(end, end);
                    self.start_block_insert(h, repeat, lines, None, false);
                } else {
                    let col = b.right + 1;
                    let (start, len) = (line_start(r, b.top), line_len(r, b.top));
                    if len < col {
                        self.edit(h, start + len..start + len, &" ".repeat(col - len));
                    }
                    h.select(start + col, start + col);
                    self.start_block_insert(h, repeat, lines, Some(col), true);
                }
                return true;
            }
            BlockOp::Op(Op::Delete) => self.each_line(h, &b, |_| String::new()),
            BlockOp::Replace(c) => self.each_line(h, &b, |s| s.chars().map(|_| c).collect()),
            BlockOp::Op(op @ (Op::Toggle | Op::Lower | Op::Upper)) => {
                self.each_line(h, &b, |s| recase(s.chars(), op))
            }
            BlockOp::Op(op @ (Op::Indent | Op::Outdent)) => {
                let unit = h.indent_unit().to_string();
                let times = cmd.count.unwrap_or(1);
                for l in b.top..=b.bottom {
                    let r = h.rope();
                    if line_len(r, l) <= b.left {
                        continue;
                    }
                    let at = line_start(r, l) + b.left;
                    if op == Op::Indent {
                        self.edit(h, at..at, &unit.repeat(times));
                    } else {
                        let n = outdent_len(r.chars_at(at), &unit, times);
                        if n > 0 {
                            self.edit(h, at..at + n, "");
                        }
                    }
                }
            }
            BlockOp::Op(Op::Yank) => unreachable!("handled above"),
        }
        let pos = b.top_left(h.rope());
        self.set_cursor(h, pos);
        self.end(h);
        self.changed(repeat);
        true
    }

    /// `p` / `P` of a block register (`:help blockwise-register`): its lines go in at one column
    /// (after the cursor's, or at it for `P`) on successive lines, padding short lines with
    /// spaces and adding lines at the end as needed. A count puts it side by side `n` times. The
    /// cursor goes to the block's top-left, or just after it with `gp` / `gP`.
    pub(crate) fn put_block(
        &mut self,
        h: &mut dyn VimHost,
        text: &str,
        before: bool,
        after: bool,
        n: usize,
    ) {
        let r = h.rope();
        let cur = h.selection().1;
        let top = line_of(r, cur);
        let col = col(r, cur) + usize::from(!before && line_len(r, top) > 0);
        let pieces: Vec<&str> = text.split('\n').collect();
        let width = pieces.iter().map(|p| p.chars().count()).max().unwrap_or(0);
        self.begin(h);
        for (i, piece) in pieces.iter().enumerate() {
            let line = top + i;
            if line > last_line(h.rope()) {
                let end = h.rope().len_chars();
                self.edit(h, end..end, "\n");
            }
            let r = h.rope();
            let (start, len) = (line_start(r, line), line_len(r, line));
            let pad = " ".repeat(width - piece.chars().count());
            let mut s = format!("{piece}{pad}").repeat(n - 1);
            s.push_str(piece);
            // Keep the text after the block in line with the other lines.
            if len > col {
                s.push_str(&pad);
            }
            if s.is_empty() {
                continue;
            }
            let lead = " ".repeat(col.saturating_sub(len));
            let at = start + col.min(len);
            self.edit(h, at..at, &format!("{lead}{s}"));
        }
        let r = h.rope();
        let pos = if after {
            let line = top + pieces.len() - 1;
            (line_start(r, line) + col + width * n).min(line_end(r, line))
        } else {
            line_start(r, top) + col
        };
        self.set_cursor(h, pos);
        self.end(h);
    }

    /// Replace each line's part of the block with `f` of it.
    fn each_line(&mut self, h: &mut dyn VimHost, b: &Block, f: impl Fn(&str) -> String) {
        for l in b.top..=b.bottom {
            let r = h.rope();
            let span = b.span(r, l);
            if span.is_empty() {
                continue;
            }
            let old = r.slice(span.clone()).to_string();
            let new = f(&old);
            if new != old {
                self.edit(h, span, &new);
            }
        }
    }

    /// The lines below the top one that reach into the block (`I` and `c` skip short lines).
    fn lines_reaching(&self, r: &Rope, b: &Block) -> Vec<usize> {
        (b.top + 1..=b.bottom)
            .filter(|&l| line_len(r, l) > b.left)
            .collect()
    }

    fn start_block_insert(
        &mut self,
        h: &mut dyn VimHost,
        repeat: Cmd,
        lines: Vec<usize>,
        col: Option<usize>,
        pad: bool,
    ) {
        let r = h.rope();
        self.block_insert = Some(BlockInsert {
            lines,
            col,
            pad,
            start: h.selection().1,
            line_count: r.len_lines(),
        });
        self.start_insert(Mode::Insert, Some(repeat), 1, false);
    }

    /// `<Esc>` after a block insert: copy what was typed on the top line (ending at `end`) to the
    /// other lines. Returns where the cursor goes.
    pub(crate) fn finish_block_insert(
        &mut self,
        h: &mut dyn VimHost,
        b: BlockInsert,
        end: usize,
    ) -> usize {
        let r = h.rope();
        if r.len_lines() != b.line_count || end < b.start || line_of(r, end) != line_of(r, b.start)
        {
            return end.saturating_sub(1).max(line_start(r, line_of(r, end)));
        }
        let text = r.slice(b.start..end).to_string();
        if !text.is_empty() {
            for l in b.lines {
                let r = h.rope();
                let (start, len) = (line_start(r, l), line_len(r, l));
                let (at, pad) = match b.col {
                    None => (start + len, 0),
                    Some(c) if b.pad => (start + len.min(c), c.saturating_sub(len)),
                    Some(c) => (start + c.min(len), 0),
                };
                self.edit(h, at..at, &format!("{}{text}", " ".repeat(pad)));
            }
        }
        b.start
    }
}

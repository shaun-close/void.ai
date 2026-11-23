//! A clean-room Vim engine, written from Vim's documented behaviour (`:help motion.txt`,
//! `change.txt`, `visual.txt`, `repeat.txt`, `pattern.txt`, `undo.txt`, `scroll.txt`,
//! `mark-motions`, `jump-motions`, `blockwise-operators`, `CTRL-A`).
//!
//! The engine owns modes, pending keys, registers, marks, the jump list, macros and dot-repeat;
//! the text, the selection, undo history, scrolling and prompts belong to a [`VimHost`]. Every
//! position is a char index into the host's rope.
//!
//! Marks and jump-list entries move with the text. The engine sees its own edits (and undo/redo);
//! a host that changes the text itself (a paste from the UI, a reload from disk) reports it with
//! [`Engine::adjust_marks`]. In Visual-block mode, [`Engine::visual_block`] gives the block to
//! draw.
//!
//! Keys are Vim key names: a single character (`"a"`, `"$"`), or a name in angle brackets
//! (`"<Esc>"`, `"<CR>"`, `"<C-r>"`). [`key_name`] builds them from a UI key event.

mod block;
mod engine;
mod ex;
mod insert;
mod keys;
mod marks;
mod motion;
mod number;
mod operator;
mod parse;
mod registers;
mod scroll;
mod text;
mod textobj;

#[cfg(test)]
mod tests;

use std::ops::Range;

pub use block::Block;
pub use engine::{Engine, Mode, State};
pub use keys::{KeyLike, key_name};
pub use registers::{RegKind, Register, Registers};

/// Where [`VimHost::scroll_to`] puts the line in the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollAt {
    Top,
    Center,
    Bottom,
}

/// What the engine needs from the editor it drives.
pub trait VimHost {
    fn rope(&self) -> &ropey::Rope;
    /// `(anchor, head)`. In Normal and Insert mode the engine keeps `anchor == head` (no
    /// selection); in Visual modes both ends are inclusive, as Vim draws them.
    fn selection(&self) -> (usize, usize);
    fn select(&mut self, anchor: usize, head: usize);
    /// Replace `range` with `text`. (Edits the host makes on its own, outside this call, should
    /// be reported with [`Engine::adjust_marks`].)
    fn replace(&mut self, range: Range<usize>, text: &str);
    /// Start one undo step; the host remembers the current head as the cursor to restore.
    fn begin_group(&mut self);
    /// Close the undo step; the current head is the cursor to restore on redo.
    fn end_group(&mut self);
    /// Undo one step and move the selection to the cursor the history restores.
    fn undo(&mut self);
    fn redo(&mut self);
    /// Insert a line break at the head, with the host's auto-indent, and move the head after it.
    fn newline(&mut self);
    /// Insert one indent at the head and move the head after it.
    fn tab(&mut self);
    /// First and last visible lines, inclusive.
    fn visible_lines(&self) -> (usize, usize);
    fn scroll_to(&mut self, line: usize, at: ScrollAt);
    /// Open the command line (`:`) or a search line (`/`, `?`). The host answers later with
    /// [`Engine::prompt_answer`].
    fn prompt(&mut self, prefix: char);
    /// Highlight every match of `pattern`, a Rust regex with its flags inline (`(?m)`, plus `i`
    /// when the search is case-insensitive); `None` clears the highlight.
    fn highlight(&mut self, pattern: Option<String>);
    fn save(&mut self);
    /// `:w!`: save even where a plain save would refuse (say, the file changed on disk since
    /// it was read). Hosts without such a check can leave this as [`VimHost::save`].
    fn save_force(&mut self) {
        self.save();
    }
    /// An ex command the engine does not handle itself (`e path`, `q`, `wa`, `qa`, …). Returns
    /// whether the host knew it.
    fn ex(&mut self, cmd: &str) -> bool;
    fn clipboard_get(&self) -> Option<String>;
    fn clipboard_set(&mut self, s: &str);
    /// One level of indentation (`"    "` or `"\t"`), used by `>` and `<`.
    fn indent_unit(&self) -> &str;
}

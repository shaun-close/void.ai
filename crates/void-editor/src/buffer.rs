use ropey::Rope;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

/// A position as tree-sitter sees it: zero-based row and byte column within the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub old_len: usize,
    pub new_len: usize,
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
}

/// One applied replacement, kept so it can be reversed and replayed.
struct Change {
    start: usize,
    old: String,
    new: String,
}

/// Changes undone or redone together, with the cursor to restore on each side.
struct Group {
    changes: Vec<Change>,
    cursor_before: usize,
    cursor_after: usize,
}

pub struct Buffer {
    rope: Rope,
    version: u64,
    saved_version: u64,
    pub line_ending: LineEnding,
    pub final_newline: bool,
    pub read_only: bool,
    pub notice: Option<String>,
    undo: Vec<Group>,
    redo: Vec<Group>,
    open_group: Option<Group>,
}

impl Buffer {
    /// Build a buffer from `text`. CRLF is normalised to LF (remembered in `line_ending`) and
    /// one trailing newline is stripped (remembered in `final_newline`).
    pub fn new(text: &str) -> Buffer {
        let line_ending = match text.find('\n') {
            Some(i) if text[..i].ends_with('\r') => LineEnding::CrLf,
            _ => LineEnding::Lf,
        };
        let text = if line_ending == LineEnding::CrLf {
            std::borrow::Cow::Owned(text.replace("\r\n", "\n"))
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        let (body, final_newline) = match text.strip_suffix('\n') {
            Some(body) => (body, true),
            None => (&*text, false),
        };
        Buffer {
            rope: Rope::from_str(body),
            version: 0,
            saved_version: 0,
            line_ending,
            final_newline,
            read_only: false,
            notice: None,
            undo: Vec::new(),
            redo: Vec::new(),
            open_group: None,
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// Bumps on every edit, undo and redo.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    /// Replace the char `range` with `text`, recording it for undo. Returns the change in the
    /// coordinates tree-sitter needs.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Edit {
        let old = self.rope.slice(range.clone()).to_string();
        let cursor = range.start;
        let edit = self.apply(range.start, old.chars().count(), text);
        let change = Change {
            start: range.start,
            old,
            new: text.to_string(),
        };
        let end = range.start + edit.new_len;
        match &mut self.open_group {
            Some(g) => g.changes.push(change),
            None => self.undo.push(Group {
                changes: vec![change],
                cursor_before: cursor,
                cursor_after: end,
            }),
        }
        self.redo.clear();
        edit
    }

    fn point(&self, char_idx: usize) -> Point {
        let row = self.rope.char_to_line(char_idx);
        let line_start = self.rope.line_to_byte(row);
        Point {
            row,
            column: self.rope.char_to_byte(char_idx) - line_start,
        }
    }

    /// Splice without recording history.
    fn apply(&mut self, start: usize, old_len: usize, text: &str) -> Edit {
        let new_len = text.chars().count();
        let start_byte = self.rope.char_to_byte(start);
        let start_point = self.point(start);
        let old_end_byte = self.rope.char_to_byte(start + old_len);
        let old_end_point = self.point(start + old_len);
        self.rope.remove(start..start + old_len);
        self.rope.insert(start, text);
        self.version += 1;
        Edit {
            start,
            old_len,
            new_len,
            start_byte,
            old_end_byte,
            new_end_byte: start_byte + text.len(),
            start_point,
            old_end_point,
            new_end_point: self.point(start + new_len),
        }
    }

    /// Start collecting edits into one undo step. `cursor` is restored by `undo`.
    pub fn begin_group(&mut self, cursor: usize) {
        self.end_group(cursor);
        self.open_group = Some(Group {
            changes: Vec::new(),
            cursor_before: cursor,
            cursor_after: cursor,
        });
    }

    /// Close the open group; `cursor` is restored by `redo`. Empty groups are dropped.
    pub fn end_group(&mut self, cursor: usize) {
        if let Some(mut g) = self.open_group.take()
            && !g.changes.is_empty()
        {
            g.cursor_after = cursor;
            self.undo.push(g);
        }
    }

    /// Undo the last group, returning the cursor from before it.
    pub fn undo(&mut self) -> Option<usize> {
        self.undo_edits().map(|(cursor, _)| cursor)
    }

    pub fn redo(&mut self) -> Option<usize> {
        self.redo_edits().map(|(cursor, _)| cursor)
    }

    /// Undo one step; returns the cursor to restore and the edits made, in order (feed them to
    /// [`crate::Syntax::edit`] to keep a tree in step).
    pub fn undo_edits(&mut self) -> Option<(usize, Vec<Edit>)> {
        self.end_group(0);
        let g = self.undo.pop()?;
        let edits = g
            .changes
            .iter()
            .rev()
            .map(|c| self.apply(c.start, c.new.chars().count(), &c.old))
            .collect();
        let cursor = g.cursor_before;
        self.redo.push(g);
        Some((cursor, edits))
    }

    /// Redo one step; like [`Buffer::undo_edits`].
    pub fn redo_edits(&mut self) -> Option<(usize, Vec<Edit>)> {
        let g = self.redo.pop()?;
        let edits = g
            .changes
            .iter()
            .map(|c| self.apply(c.start, c.old.chars().count(), &c.new))
            .collect();
        let cursor = g.cursor_after;
        self.undo.push(g);
        Some((cursor, edits))
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_of(&self, char: usize) -> usize {
        self.rope.char_to_line(char)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    /// Chars in `line`, excluding its newline.
    pub fn line_len(&self, line: usize) -> usize {
        let l = self.rope.line(line);
        let n = l.len_chars();
        if n > 0 && l.char(n - 1) == '\n' {
            n - 1
        } else {
            n
        }
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.slice(range).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_multibyte_edits() {
        let mut b = Buffer::new("héllo 🦀 世界");
        assert_eq!(b.slice(0..5), "héllo");
        assert_eq!(b.slice(6..7), "🦀");
        assert_eq!(b.slice(8..10), "世界");
        b.replace(6..7, "crab");
        assert_eq!(b.slice(0..b.rope().len_chars()), "héllo crab 世界");
        b.replace(1..2, "e");
        assert_eq!(b.slice(0..5), "hello");
        assert_eq!(b.line_len(0), 13);
    }

    #[test]
    fn undo_groups_insert_session() {
        let mut b = Buffer::new("ab");
        b.begin_group(1);
        for (i, c) in "hello".chars().enumerate() {
            b.replace(1 + i..1 + i, &c.to_string());
        }
        b.end_group(6);
        assert_eq!(b.slice(0..7), "ahellob");
        assert_eq!(b.undo(), Some(1));
        assert_eq!(b.slice(0..2), "ab");
        assert_eq!(b.rope().len_chars(), 2);
        assert_eq!(b.undo(), None);
        assert_eq!(b.redo(), Some(6));
        assert_eq!(b.slice(0..7), "ahellob");
        assert_eq!(b.redo(), None);
    }

    #[test]
    fn ungrouped_edits_are_separate_undo_steps() {
        let mut b = Buffer::new("");
        b.replace(0..0, "a");
        b.replace(1..1, "b");
        assert_eq!(b.undo(), Some(1));
        assert_eq!(b.slice(0..1), "a");
        b.replace(1..1, "z");
        assert_eq!(b.redo(), None, "a new edit clears redo");
    }

    #[test]
    fn dirty_follows_version_and_saved_mark() {
        let mut b = Buffer::new("x");
        assert!(!b.is_dirty());
        let v = b.version();
        b.replace(0..1, "y");
        assert!(b.version() > v && b.is_dirty());
        b.mark_saved();
        assert!(!b.is_dirty());
    }

    #[test]
    fn edit_reports_tree_sitter_coordinates() {
        // "é" is 2 bytes, "🦀" 4 bytes.
        let mut b = Buffer::new("aé\n🦀bc\nz");
        // Replace "🦀b" (chars 3..5) with "x\nyé".
        let e = b.replace(3..5, "x\nyé");
        assert_eq!((e.start, e.old_len, e.new_len), (3, 2, 4));
        assert_eq!(e.start_byte, 4);
        assert_eq!(e.old_end_byte, 9);
        assert_eq!(e.new_end_byte, 4 + "x\nyé".len());
        assert_eq!(e.start_point, Point { row: 1, column: 0 });
        assert_eq!(e.old_end_point, Point { row: 1, column: 5 });
        assert_eq!(e.new_end_point, Point { row: 2, column: 3 });
        assert_eq!(b.slice(0..b.rope().len_chars()), "aé\nx\nyéc\nz");
    }

    #[test]
    fn line_helpers() {
        let b = Buffer::new("ab\n\ncde");
        assert_eq!(b.line_count(), 3);
        assert_eq!(b.line_of(3), 1);
        assert_eq!(b.line_start(2), 4);
        assert_eq!(b.line_len(0), 2);
        assert_eq!(b.line_len(1), 0);
        assert_eq!(b.line_len(2), 3);
    }

    #[test]
    fn undo_reports_edits() {
        let mut b = Buffer::new("aé\nb");
        b.begin_group(0);
        b.replace(0..0, "x");
        b.replace(4..5, "🦀z");
        b.end_group(6);
        let (cursor, edits) = b.undo_edits().unwrap();
        assert_eq!(cursor, 0);
        assert_eq!(b.slice(0..b.rope().len_chars()), "aé\nb");
        // Undone in reverse order, each in the coordinates of the text it applied to.
        assert_eq!(edits.len(), 2);
        assert_eq!(
            (edits[0].start, edits[0].old_len, edits[0].new_len),
            (4, 2, 1)
        );
        assert_eq!(
            (edits[1].start, edits[1].old_len, edits[1].new_len),
            (0, 1, 0)
        );
        let (cursor, edits) = b.redo_edits().unwrap();
        assert_eq!(cursor, 6);
        assert_eq!(b.slice(0..b.rope().len_chars()), "xaé\n🦀z");
        assert_eq!(
            (edits[0].start, edits[0].old_len, edits[0].new_len),
            (0, 0, 1)
        );
        assert_eq!(
            (edits[1].start, edits[1].old_len, edits[1].new_len),
            (4, 1, 2)
        );
        assert!(b.redo_edits().is_none());
    }
}

//! Rope helpers in Vim's terms: lines without their line break, columns as char offsets, and
//! character classes for word motions.

use ropey::Rope;

pub(crate) fn last_line(r: &Rope) -> usize {
    r.len_lines() - 1
}

pub(crate) fn line_of(r: &Rope, pos: usize) -> usize {
    r.char_to_line(pos.min(r.len_chars()))
}

pub(crate) fn line_start(r: &Rope, line: usize) -> usize {
    r.line_to_char(line)
}

/// Chars in `line`, without its line break.
pub(crate) fn line_len(r: &Rope, line: usize) -> usize {
    let l = r.line(line);
    let n = l.len_chars();
    if n > 0 && l.char(n - 1) == '\n' {
        n - 1
    } else {
        n
    }
}

/// The position just past the last char of `line` (its line break, or the end of the text).
pub(crate) fn line_end(r: &Rope, line: usize) -> usize {
    line_start(r, line) + line_len(r, line)
}

/// The last char of `line`, or its start when it is empty: where Normal mode's cursor can go.
pub(crate) fn line_last(r: &Rope, line: usize) -> usize {
    line_start(r, line) + line_len(r, line).saturating_sub(1)
}

pub(crate) fn col(r: &Rope, pos: usize) -> usize {
    pos - line_start(r, line_of(r, pos))
}

pub(crate) fn char_at(r: &Rope, pos: usize) -> Option<char> {
    (pos < r.len_chars()).then(|| r.char(pos))
}

pub(crate) fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Leading blanks of `line`, in chars.
pub(crate) fn indent_len(r: &Rope, line: usize) -> usize {
    r.line(line).chars().take_while(|c| is_blank(*c)).count()
}

pub(crate) fn indent(r: &Rope, line: usize) -> String {
    r.line(line).chars().take_while(|c| is_blank(*c)).collect()
}

/// The first non-blank char of `line` (its end when the line is all blanks), as a position.
pub(crate) fn first_non_blank(r: &Rope, line: usize) -> usize {
    let start = line_start(r, line);
    (start + indent_len(r, line)).min(line_last(r, line).max(start))
}

/// Whether `line` is empty or only blanks.
pub(crate) fn is_blank_line(r: &Rope, line: usize) -> bool {
    indent_len(r, line) == line_len(r, line)
}

/// Clamp `pos` to where Normal mode's cursor may sit on its line.
pub(crate) fn clamp_normal(r: &Rope, pos: usize) -> usize {
    let line = line_of(r, pos);
    pos.clamp(line_start(r, line), line_last(r, line))
}

/// `pos` is the start of an empty line (Vim counts an empty line as a word).
pub(crate) fn is_empty_line_at(r: &Rope, pos: usize) -> bool {
    matches!(char_at(r, pos), None | Some('\n')) && (pos == 0 || r.char(pos - 1) == '\n')
}

/// Vim's character class: 0 for blanks and line breaks, 1 for punctuation, 2 for word chars
/// (letters, digits, `_`), and separate classes for emoji and each CJK script so that a change
/// of script starts a new word. With `big` (WORD motions) every non-blank is one class.
pub(crate) fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        return 0;
    }
    if big {
        return 1;
    }
    if c.is_ascii() {
        return if c.is_ascii_alphanumeric() || c == '_' {
            2
        } else {
            1
        };
    }
    match c as u32 {
        0x3000..=0x303F | 0xFF00..=0xFF0F | 0xFF1A..=0xFF20 => 1,
        0x3040..=0x309F => 4,
        0x30A0..=0x30FF => 5,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF => 6,
        0xAC00..=0xD7A3 => 7,
        0x1F000..=0x1FAFF | 0x2600..=0x27BF => 3,
        _ if c.is_alphanumeric() => 2,
        _ => 1,
    }
}

/// Whether `c` is a keyword char (`iskeyword`: what `*` and `\<` treat as a word).
pub(crate) fn is_keyword(c: char) -> bool {
    class(c, false) >= 2 && class(c, false) != 3
}

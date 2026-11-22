//! `<C-a>` / `<C-x>` (`:help CTRL-A`): add to the number at or after the cursor. Decimal numbers
//! may be negative; `0x` / `0X` numbers are hex (unsigned, 64-bit, wrapping). A number written
//! with leading zeros keeps its width, as does every hex number; hex keeps the case of its
//! letters. In Visual mode the first number in the selected part of each line changes
//! (`:help v_CTRL-A`).

use crate::engine::Engine;
use crate::text::*;
use crate::{Mode, VimHost};

/// A number in a line: its chars `start..end` (with the `-` or `0x`), and its digits.
#[derive(Debug, PartialEq, Eq)]
struct Number {
    start: usize,
    end: usize,
    hex: bool,
}

/// The first number on `line` that ends after column `cur`.
fn find(line: &[char], cur: usize) -> Option<Number> {
    let mut i = 0;
    while i < line.len() {
        let hex = line[i] == '0'
            && matches!(line.get(i + 1), Some('x' | 'X'))
            && line.get(i + 2).is_some_and(char::is_ascii_hexdigit);
        let (start, end) = if hex {
            let digits = line[i + 2..].iter().take_while(|c| c.is_ascii_hexdigit());
            (i, i + 2 + digits.count())
        } else if line[i].is_ascii_digit() {
            let digits = line[i..].iter().take_while(|c| c.is_ascii_digit());
            let start = if i > 0 && line[i - 1] == '-' {
                i - 1
            } else {
                i
            };
            (start, i + digits.count())
        } else {
            i += 1;
            continue;
        };
        if end > cur {
            return Some(Number { start, end, hex });
        }
        i = end;
    }
    None
}

/// `text` (a number [`find`] found) plus `delta`, written the same way; `None` when it is too
/// long to be a number.
fn added(text: &str, hex: bool, delta: i64) -> Option<String> {
    if hex {
        let (prefix, digits) = text.split_at(2);
        let value = u64::from_str_radix(digits, 16).ok()?;
        let new = value.wrapping_add_signed(delta);
        let width = digits.len();
        let upper = digits
            .chars()
            .rev()
            .find(char::is_ascii_alphabetic)
            .is_some_and(|c| c.is_ascii_uppercase());
        return Some(if upper {
            format!("{prefix}{new:0width$X}")
        } else {
            format!("{prefix}{new:0width$x}")
        });
    }
    let (negative, digits) = match text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, text),
    };
    let value: i128 = digits.parse().ok()?;
    let new = if negative { -value } else { value }.saturating_add(delta.into());
    let width = if digits.len() > 1 && digits.starts_with('0') {
        digits.len()
    } else {
        0
    };
    let sign = if new < 0 { "-" } else { "" };
    Some(format!("{sign}{:0width$}", new.unsigned_abs()))
}

impl Engine {
    /// Add `delta` to the number at or after the cursor; `false` when the line has none.
    pub(crate) fn add(&mut self, h: &mut dyn VimHost, delta: i64) -> bool {
        let r = h.rope();
        let cur = h.selection().1;
        let line = line_of(r, cur);
        let start = line_start(r, line);
        let chars: Vec<char> = r.slice(start..line_end(r, line)).chars().collect();
        let Some(n) = find(&chars, cur - start) else {
            return false;
        };
        let text: String = chars[n.start..n.end].iter().collect();
        let Some(new) = added(&text, n.hex, delta) else {
            return false;
        };
        let at = start + n.start;
        self.begin(h);
        self.edit(h, at..start + n.end, &new);
        self.set_cursor(h, at + new.chars().count() - 1);
        self.end(h);
        true
    }

    /// Visual `<C-a>` / `<C-x>`: add `delta` to the first number in the selected part of each
    /// line, as one undo step, and put the cursor at the start of the selection. `false` when no
    /// line had a number.
    pub(crate) fn visual_add(&mut self, h: &mut dyn VimHost, delta: i64) -> bool {
        let mode = self.state.mode;
        let (anchor, head) = h.selection();
        let r = h.rope();
        let block = self.block_of(r, anchor, head);
        let (a, b) = (anchor.min(head), anchor.max(head));
        let (l1, l2) = (line_of(r, a), line_of(r, b));
        let start = match mode {
            Mode::VisualLine => line_start(r, l1),
            Mode::VisualBlock => clamp_normal(r, line_start(r, l1) + block.left),
            _ => a,
        };
        self.exit_visual(h);
        self.begin(h);
        let mut any = false;
        for l in l1..=l2 {
            let r = h.rope();
            let (ls, le) = (line_start(r, l), line_end(r, l));
            let span = match mode {
                Mode::VisualLine => ls..le,
                Mode::VisualBlock => block.span(r, l),
                _ => {
                    let from = if l == l1 { a.min(le) } else { ls };
                    let to = if l == l2 { (b + 1).min(le) } else { le };
                    from..to.max(from)
                }
            };
            let chars: Vec<char> = r.slice(span.clone()).chars().collect();
            let Some(n) = find(&chars, 0) else {
                continue;
            };
            let text: String = chars[n.start..n.end].iter().collect();
            let Some(new) = added(&text, n.hex, delta) else {
                continue;
            };
            self.edit(h, span.start + n.start..span.start + n.end, &new);
            any = true;
        }
        self.set_cursor(h, start);
        self.end(h);
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_found_and_added() {
        let chars = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(
            find(&chars("a 0x1f b"), 4),
            Some(Number {
                start: 2,
                end: 6,
                hex: true
            })
        );
        assert_eq!(find(&chars("12 34"), 2).map(|n| n.start), Some(3));
        assert_eq!(find(&chars("x-5"), 0).map(|n| n.start), Some(1));
        assert_eq!(find(&chars("no digits"), 0), None);
        assert_eq!(added("0xff", true, 1).as_deref(), Some("0x100"));
        assert_eq!(
            added("0x0", true, -1).as_deref(),
            Some("0xffffffffffffffff")
        );
        assert_eq!(added("-3", false, 5).as_deref(), Some("2"));
        assert_eq!(added("010", false, -1).as_deref(), Some("009"));
        assert_eq!(added("10", false, -1).as_deref(), Some("9"));
        assert_eq!(
            added("99999999999999999999999999999999999999999", false, 1),
            None
        );
    }
}

//! Text objects (`:help text-objects`): words, blocks, quotes and paragraphs.

use ropey::Rope;

use crate::motion::{find_close, find_open};
use crate::operator::Region;
use crate::parse::{ObjKind, Object};
use crate::text::*;

/// The region of object `obj` around `cur`, `n` times (`2i(` is the second block out).
pub(crate) fn object(r: &Rope, cur: usize, obj: Object, n: usize) -> Option<Region> {
    match obj.kind {
        ObjKind::Word { big } => word(r, cur, n, obj.inner, big),
        ObjKind::Block { open, close } => block(r, cur, n, obj.inner, open, close),
        ObjKind::Quote(q) => quote(r, cur, q, obj.inner),
        ObjKind::Paragraph => paragraph(r, cur, n, obj.inner),
    }
}

/// The run of same-class chars (blanks count as one class) that `p` is in, within its line.
fn run(r: &Rope, p: usize, big: bool) -> (usize, usize) {
    let line = line_of(r, p);
    let (start, end) = (line_start(r, line), line_end(r, line));
    let k = class(r.char(p), big);
    let (mut s, mut e) = (p, p + 1);
    while s > start && class(r.char(s - 1), big) == k {
        s -= 1;
    }
    while e < end && class(r.char(e), big) == k {
        e += 1;
    }
    (s, e)
}

fn word(r: &Rope, cur: usize, n: usize, inner: bool, big: bool) -> Option<Region> {
    let line = line_of(r, cur);
    let end = line_end(r, line);
    if cur >= end {
        // An empty line: an empty object, so `ciw` still starts Insert mode.
        return Some(Region::Chars(cur..cur));
    }
    let blank_at = |p: usize| p < end && class(r.char(p), big) == 0;
    let (s, mut e) = run(r, cur, big);
    if inner {
        // `iw`: each word and each run of blanks counts as one.
        for _ in 1..n {
            if e >= end {
                break;
            }
            e = run(r, e, big).1;
        }
        return Some(Region::Chars(s..e));
    }
    let mut s = s;
    if blank_at(cur) {
        // `aw` on blanks: the blanks and the word after them.
        for i in 0..n {
            if i > 0 && blank_at(e) {
                e = run(r, e, big).1;
            }
            if e < end {
                e = run(r, e, big).1;
            }
        }
        return Some(Region::Chars(s..e));
    }
    // `aw` on a word: the word and the blanks after it, or the blanks before when there are none.
    let mut trailing = false;
    for i in 0..n {
        if i > 0 && e < end {
            e = run(r, e, big).1;
        }
        if blank_at(e) {
            e = run(r, e, big).1;
            trailing = true;
        } else {
            trailing = false;
        }
    }
    if !trailing && s > line_start(r, line) && blank_at(s - 1) {
        s = run(r, s - 1, big).0;
    }
    Some(Region::Chars(s..e))
}

fn block(r: &Rope, cur: usize, n: usize, inner: bool, open: char, close: char) -> Option<Region> {
    // On the closing bracket this finds its own opening one, as from inside the block.
    let mut start = if char_at(r, cur) == Some(open) {
        Some(cur)
    } else {
        find_open(r, cur, open, close)
    };
    for _ in 1..n {
        start = find_open(r, start?, open, close);
    }
    let start = match start {
        Some(s) => s,
        // Not inside a block: the next one on the line.
        None if n == 1 => {
            let end = line_end(r, line_of(r, cur));
            (cur..end).find(|&p| r.char(p) == open)?
        }
        None => return None,
    };
    let end = find_close(r, start + 1, open, close)?;
    if !inner {
        return Some(Region::Chars(start..end + 1));
    }
    let mut s = start + 1;
    if char_at(r, s) == Some('\n') {
        s += 1;
    }
    let close_line = line_of(r, end);
    let close_bol = line_start(r, close_line);
    if close_line > line_of(r, start)
        && s <= close_bol
        && (close_bol..end).all(|p| is_blank(r.char(p)))
    {
        let first = line_of(r, s);
        if first >= close_line {
            return Some(Region::Chars(s..s));
        }
        // The closing bracket's indent is left out. When the inner text starts at or before the
        // first non-blank of its line, it is whole lines (`:help i{`, like `:help exclusive`);
        // otherwise it ends at the end of the line before the closing bracket.
        if s <= line_start(r, first) + indent_len(r, first) {
            return Some(Region::Lines(first, close_line - 1));
        }
        return Some(Region::Chars(s..line_end(r, close_line - 1)));
    }
    Some(Region::Chars(s.min(end)..end))
}

fn quote(r: &Rope, cur: usize, q: char, inner: bool) -> Option<Region> {
    let line = line_of(r, cur);
    let (start, end) = (line_start(r, line), line_end(r, line));
    let mut quotes = Vec::new();
    let mut escaped = false;
    for (i, c) in r.slice(start..end).chars().enumerate() {
        if c == q && !escaped {
            quotes.push(start + i);
        }
        escaped = c == '\\' && !escaped;
    }
    let (a, b) = if let Some(i) = quotes.iter().position(|&p| p == cur) {
        // On a quote: pairs are counted from the start of the line.
        if i % 2 == 0 {
            (cur, *quotes.get(i + 1)?)
        } else {
            (quotes[i - 1], cur)
        }
    } else {
        let before = quotes.iter().rev().find(|&&p| p < cur);
        let after = quotes.iter().find(|&&p| p > cur);
        match (before, after) {
            (Some(&a), Some(&b)) => (a, b),
            (None, Some(&a)) => {
                let i = quotes.iter().position(|&p| p == a)?;
                (a, *quotes.get(i + 1)?)
            }
            _ => return None,
        }
    };
    if inner {
        return Some(Region::Chars(a + 1..b));
    }
    let blank = |p: usize| p < end && is_blank(r.char(p));
    let (mut s, mut e) = (a, b + 1);
    if blank(e) {
        while blank(e) {
            e += 1;
        }
    } else {
        while s > start && blank(s - 1) {
            s -= 1;
        }
    }
    Some(Region::Chars(s..e))
}

fn paragraph(r: &Rope, cur: usize, n: usize, inner: bool) -> Option<Region> {
    let last = last_line(r);
    let line = line_of(r, cur);
    let blank = |l: usize| is_blank_line(r, l);
    let mut first = line;
    while first > 0 && blank(first - 1) == blank(line) {
        first -= 1;
    }
    let run_end = |l: usize| {
        let mut e = l;
        while e < last && blank(e + 1) == blank(l) {
            e += 1;
        }
        e
    };
    let mut l2 = run_end(line);
    if inner {
        for _ in 1..n {
            if l2 >= last {
                break;
            }
            l2 = run_end(l2 + 1);
        }
        return Some(Region::Lines(first, l2));
    }
    let started_blank = blank(line);
    for i in 0..n {
        if i > 0 {
            if l2 >= last {
                break;
            }
            l2 = run_end(l2 + 1);
        }
        if l2 < last && blank(l2 + 1) != started_blank {
            l2 = run_end(l2 + 1);
        } else if i == 0 && !started_blank {
            // No blank lines after the paragraph: take the ones before it.
            while first > 0 && blank(first - 1) {
                first -= 1;
            }
        }
    }
    Some(Region::Lines(first, l2))
}

//! Search and replace over a rope buffer.
//!
//! Positions are char indices into the rope. Matching runs through `regex-cursor` directly over
//! the rope's chunks, so a search never copies the document; byte offsets exist only inside
//! this module.

use std::ops::Range;

use regex_cursor::{
    Input,
    engines::meta::Regex,
    regex_automata::util::{captures::Captures, interpolate, primitives::PatternID, syntax},
};
use ropey::Rope;

use crate::Buffer;

/// How letter case is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-insensitive unless the pattern contains an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// A search request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub pattern: String,
    /// Treat `pattern` as a regular expression; otherwise it is matched literally.
    pub regex: bool,
    pub case: CaseMode,
    /// Match only at word boundaries (`\b…\b` around the pattern). A pattern that starts or ends
    /// with a non-word character needs a word character next to it outside the match, so
    /// `(foo)` will not match in ` (foo) `.
    pub whole_word: bool,
}

/// Check that `query` is a usable pattern. Only an invalid regex is an error; an empty pattern is
/// valid (it matches nothing).
pub fn validate(query: &Query) -> Result<(), String> {
    if query.pattern.is_empty() {
        return Ok(());
    }
    build(query).map(drop)
}

/// First match starting at or after char `from`; with `wrap`, falls back to the first match in
/// the buffer. An invalid regex or an empty pattern matches nothing.
pub fn find_next(rope: &Rope, query: &Query, from: usize, wrap: bool) -> Option<Range<usize>> {
    let re = compile(query)?;
    let from = from.min(rope.len_chars());
    let mut cache = re.create_cache();
    let mut search = |start: usize| {
        let mut input = Input::new(rope);
        input.set_start(rope.char_to_byte(start));
        re.search_with(&mut cache, &mut input)
            .map(|m| to_chars(rope, m.range()))
    };
    search(from).or_else(|| if wrap && from > 0 { search(0) } else { None })
}

/// The closest match starting strictly before char `from`; with `wrap`, the last match in the
/// buffer. Scans forward from the start of the buffer (matches cannot be found backwards).
pub fn find_prev(rope: &Rope, query: &Query, from: usize, wrap: bool) -> Option<Range<usize>> {
    let re = compile(query)?;
    let (mut before, mut last) = (None, None);
    for m in re.find_iter(Input::new(rope)) {
        let range = to_chars(rope, m.range());
        if range.start < from {
            before = Some(range);
        } else if before.is_some() || !wrap {
            break;
        } else {
            last = Some(range);
        }
    }
    before.or(if wrap { last } else { None })
}

/// Up to `limit` non-overlapping matches, in order.
pub fn find_all(rope: &Rope, query: &Query, limit: usize) -> Vec<Range<usize>> {
    let Some(re) = compile(query) else {
        return Vec::new();
    };
    re.find_iter(Input::new(rope))
        .take(limit)
        .map(|m| to_chars(rope, m.range()))
        .collect()
}

/// Replace every match as one undo step and return how many were replaced. In regex mode `with`
/// may use `$1` / `${name}`; otherwise it is inserted literally.
pub fn replace_all(buffer: &mut Buffer, query: &Query, with: &str) -> usize {
    let Some(re) = compile(query) else {
        return 0;
    };
    let rope = buffer.rope();
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for caps in re.captures_iter(Input::new(rope)) {
        let Some(m) = caps.get_match() else { continue };
        let text = if query.regex {
            interpolate(rope, &caps, with)
        } else {
            with.to_string()
        };
        edits.push((to_chars(rope, m.range()), text));
    }
    let count = edits.len();
    if let Some((first, _)) = edits.first() {
        let cursor = first.start;
        buffer.begin_group(cursor);
        for (range, text) in edits.into_iter().rev() {
            buffer.replace(range, &text);
        }
        buffer.end_group(cursor);
    }
    count
}

fn interpolate(rope: &Rope, caps: &Captures, with: &str) -> String {
    let mut out = String::new();
    interpolate::string(
        with,
        |index, dst| {
            if let Some(span) = caps.get_group(index) {
                dst.extend(rope.byte_slice(span.range()).chunks());
            }
        },
        |name| caps.group_info().to_index(PatternID::ZERO, name),
        &mut out,
    );
    out
}

fn to_chars(rope: &Rope, bytes: Range<usize>) -> Range<usize> {
    rope.byte_to_char(bytes.start)..rope.byte_to_char(bytes.end)
}

/// Build the engine for `query`; `None` for an empty pattern or an invalid regex.
fn compile(query: &Query) -> Option<Regex> {
    if query.pattern.is_empty() {
        return None;
    }
    build(query).ok()
}

fn build(query: &Query) -> Result<Regex, String> {
    let mut pattern = if query.regex {
        query.pattern.clone()
    } else {
        regex_syntax::escape(&query.pattern)
    };
    if query.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let insensitive = match query.case {
        CaseMode::Insensitive => true,
        CaseMode::Sensitive => false,
        CaseMode::Smart => !has_uppercase(&query.pattern, query.regex),
    };
    Regex::builder()
        .syntax(
            syntax::Config::new()
                .case_insensitive(insensitive)
                .multi_line(true),
        )
        .build(&pattern)
        .map_err(|e| e.to_string())
}

/// Whether `pattern` has an uppercase letter the user typed; in regex mode escapes such as `\W`
/// or `\S` do not count.
fn has_uppercase(pattern: &str, regex: bool) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if regex && c == '\\' {
            chars.next();
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pattern: &str) -> Query {
        Query {
            pattern: pattern.into(),
            ..Query::default()
        }
    }

    #[test]
    fn find_next_wraps() {
        let rope = Rope::from_str("foo bar foo");
        assert_eq!(find_next(&rope, &q("foo"), 0, false), Some(0..3));
        assert_eq!(find_next(&rope, &q("foo"), 1, false), Some(8..11));
        assert_eq!(find_next(&rope, &q("foo"), 9, false), None);
        assert_eq!(find_next(&rope, &q("foo"), 9, true), Some(0..3));
        assert_eq!(find_next(&rope, &q("zzz"), 0, true), None);
    }

    #[test]
    fn find_prev_is_strictly_before() {
        let rope = Rope::from_str("foo bar foo");
        assert_eq!(find_prev(&rope, &q("foo"), 8, false), Some(0..3));
        assert_eq!(find_prev(&rope, &q("foo"), 9, false), Some(8..11));
        assert_eq!(find_prev(&rope, &q("foo"), 0, false), None);
        assert_eq!(find_prev(&rope, &q("foo"), 0, true), Some(8..11));
    }

    #[test]
    fn smart_case() {
        let rope = Rope::from_str("Foo foo FOO");
        assert_eq!(find_all(&rope, &q("foo"), 10).len(), 3);
        assert_eq!(find_all(&rope, &q("Foo"), 10), vec![0..3]);
        let sensitive = Query {
            case: CaseMode::Sensitive,
            ..q("foo")
        };
        assert_eq!(find_all(&rope, &sensitive, 10), vec![4..7]);
        let insensitive = Query {
            case: CaseMode::Insensitive,
            ..q("FOO")
        };
        assert_eq!(find_all(&rope, &insensitive, 10).len(), 3);
    }

    #[test]
    fn whole_word() {
        let rope = Rope::from_str("cat concat cat.");
        let words = Query {
            whole_word: true,
            ..q("cat")
        };
        assert_eq!(find_all(&rope, &words, 10), vec![0..3, 11..14]);
        assert_eq!(find_all(&rope, &q("cat"), 10).len(), 3);
    }

    #[test]
    fn regex_captures_in_replace() {
        let mut buf = Buffer::new("a=1 b=22");
        let re = Query {
            regex: true,
            ..q(r"(\w)=(?P<n>\d+)")
        };
        assert_eq!(replace_all(&mut buf, &re, "${n}:$1"), 2);
        assert_eq!(buf.rope().to_string(), "1:a 22:b");

        let mut buf = Buffer::new("x x");
        assert_eq!(replace_all(&mut buf, &q("x"), "$1"), 2);
        assert_eq!(buf.rope().to_string(), "$1 $1");
    }

    #[test]
    fn literal_pattern_is_escaped() {
        let rope = Rope::from_str("a.c abc");
        assert_eq!(find_all(&rope, &q("a.c"), 10), vec![0..3]);
    }

    #[test]
    fn search_multibyte_offsets() {
        let rope = Rope::from_str("héllo wörld ünï wörld");
        assert_eq!(find_all(&rope, &q("wörld"), 10), vec![6..11, 16..21]);
        assert_eq!(find_next(&rope, &q("ünï"), 0, false), Some(12..15));
        assert_eq!(find_prev(&rope, &q("wörld"), 20, false), Some(16..21));
        // Case-insensitive match of a non-ASCII capital.
        let rope = Rope::from_str("ÄÖ äö");
        assert_eq!(find_all(&rope, &q("äö"), 10), vec![0..2, 3..5]);
        let mut buf = Buffer::new("héllo wörld");
        assert_eq!(replace_all(&mut buf, &q("ö"), "😀"), 1);
        assert_eq!(buf.rope().to_string(), "héllo w😀rld");
    }

    #[test]
    fn replace_all_is_one_undo() {
        let mut buf = Buffer::new("one two one two one");
        assert_eq!(replace_all(&mut buf, &q("one"), "1"), 3);
        assert_eq!(buf.rope().to_string(), "1 two 1 two 1");
        assert!(buf.undo().is_some());
        assert_eq!(buf.rope().to_string(), "one two one two one");
        assert!(buf.redo().is_some());
        assert_eq!(buf.rope().to_string(), "1 two 1 two 1");
    }

    #[test]
    fn empty_matches_terminate() {
        let mut buf = Buffer::new("ab\ncd");
        let re = Query {
            regex: true,
            ..q("^")
        };
        assert_eq!(replace_all(&mut buf, &re, "> "), 2);
        assert_eq!(buf.rope().to_string(), "> ab\n> cd");
        assert!(find_all(&Rope::from_str("abc"), &q(""), 10).is_empty());
    }

    #[test]
    fn matches_across_rope_chunks() {
        let text = "xé ".repeat(50_000) + "needle " + &"yö ".repeat(50_000) + "needle";
        let rope = Rope::from_str(&text);
        let total = rope.len_chars();
        assert_eq!(
            find_all(&rope, &q("needle"), 10),
            vec![150_000..150_006, total - 6..total]
        );
        assert_eq!(
            find_prev(&rope, &q("needle"), total, false),
            Some(total - 6..total)
        );
    }

    #[test]
    fn find_all_honours_limit_and_bad_regex() {
        let rope = Rope::from_str("a a a a");
        assert_eq!(find_all(&rope, &q("a"), 2).len(), 2);
        let bad = Query {
            regex: true,
            ..q("(")
        };
        assert!(find_all(&rope, &bad, 10).is_empty());
        assert_eq!(find_next(&rope, &bad, 0, true), None);
    }

    #[test]
    fn validate_reports_bad_regex() {
        let bad = Query {
            regex: true,
            ..q("(")
        };
        assert!(validate(&bad).is_err_and(|e| !e.is_empty()));
        assert_eq!(validate(&q("(")), Ok(()));
        let ok = Query {
            regex: true,
            ..q("a+")
        };
        assert_eq!(validate(&ok), Ok(()));
    }
}

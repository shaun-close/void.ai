//! Searching (`:help pattern.txt`) and the command line (`:help cmdline.txt`): `:w`, `:q`, `:wq`,
//! `:x`, `:{n}`, `:s`, `:noh`; anything else goes to the host.
//!
//! Patterns are Rust regex syntax, matched with smart case; `\c` / `\C` force the case and
//! `\<` / `\>` are word boundaries, as in Vim.

use void_editor::{CaseMode, Query, captures_in, find_next, find_prev, validate};

use crate::VimHost;
use crate::engine::Engine;
use crate::registers::Register;
use crate::text::*;

/// A Vim search pattern as a query for `void_editor`'s search.
pub(crate) fn query(pattern: &str) -> Query {
    let mut case = CaseMode::Smart;
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('c') => case = CaseMode::Insensitive,
            Some('C') => case = CaseMode::Sensitive,
            Some(n) => {
                out.push('\\');
                out.push(n);
            }
            None => out.push('\\'),
        }
    }
    Query {
        pattern: out,
        regex: true,
        case,
        whole_word: false,
    }
}

/// The query as one self-contained Rust regex, flags inline, for the host's highlighting.
fn highlight_regex(q: &Query) -> String {
    let insensitive = match q.case {
        CaseMode::Insensitive => true,
        CaseMode::Sensitive => false,
        CaseMode::Smart => !has_uppercase(&q.pattern),
    };
    format!("(?m{}){}", if insensitive { "i" } else { "" }, q.pattern)
}

/// Smart case looks for an uppercase letter the user typed (`\S` and `\W` do not count).
fn has_uppercase(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

/// Split `text` at the first `delim` not escaped by a backslash; `\delim` becomes `delim`.
fn split_at_delim(text: &str, delim: char) -> (String, Option<&str>) {
    let mut out = String::new();
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == delim {
            return (out, Some(&text[i + c.len_utf8()..]));
        }
        if c == '\\' {
            match chars.next() {
                Some((_, n)) if n == delim => out.push(n),
                Some((_, n)) => {
                    out.push('\\');
                    out.push(n);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    (out, None)
}

/// A piece of a `:s` replacement (`:help sub-replace-special`).
enum Piece {
    Text(String),
    Group(usize),
}

fn replacement(rep: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut chars = rep.chars();
    let group = |pieces: &mut Vec<Piece>, text: &mut String, g| {
        if !text.is_empty() {
            pieces.push(Piece::Text(std::mem::take(text)));
        }
        pieces.push(Piece::Group(g));
    };
    while let Some(c) = chars.next() {
        match c {
            '&' => group(&mut pieces, &mut text, 0),
            '\\' => match chars.next() {
                Some(d @ '0'..='9') => group(&mut pieces, &mut text, d as usize - '0' as usize),
                Some('n' | 'r') => text.push('\n'),
                Some('t') => text.push('\t'),
                Some(other) => text.push(other),
                None => text.push('\\'),
            },
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

/// Where a line address in a range points: `.`, `$`, `{n}`, `'<`, `'>`, with `+n` / `-n`.
fn address(
    s: &str,
    cur: usize,
    last: usize,
    visual: Option<(usize, usize)>,
) -> Option<(Option<usize>, &str)> {
    let digits = |s: &str| s.chars().take_while(char::is_ascii_digit).count();
    let (mut line, mut rest) = match s.chars().next() {
        Some('.') => (Some(cur as i64), &s[1..]),
        Some('$') => (Some(last as i64), &s[1..]),
        Some('\'') if s[1..].starts_with('<') => (Some(visual?.0 as i64), &s[2..]),
        Some('\'') if s[1..].starts_with('>') => (Some(visual?.1 as i64), &s[2..]),
        Some(c) if c.is_ascii_digit() => {
            let n = digits(s);
            (Some(s[..n].parse::<i64>().ok()? - 1), &s[n..])
        }
        _ => (None, s),
    };
    while let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let n = digits(&rest[1..]);
        let off = if n == 0 {
            1
        } else {
            rest[1..1 + n].parse::<i64>().ok()?
        };
        let base = line.unwrap_or(cur as i64);
        line = Some(if sign == '+' { base + off } else { base - off });
        rest = &rest[1 + n..];
    }
    Some((line.map(|l| l.clamp(0, last as i64) as usize), rest))
}

impl Engine {
    /// The pattern a `/` or `?` line asks for (an offset after the closing delimiter is
    /// ignored); an empty one reuses the last pattern. Becomes the last search.
    pub(crate) fn search_pattern(&mut self, text: &str, forward: bool) -> Option<String> {
        let (pattern, _offset) = split_at_delim(text, if forward { '/' } else { '?' });
        let pattern = if pattern.is_empty() {
            match &self.last_search {
                Some((p, _)) => p.clone(),
                None => {
                    self.state.message = "E35: No previous regular expression".into();
                    return None;
                }
            }
        } else {
            pattern
        };
        self.remember_search(&pattern, forward);
        Some(pattern)
    }

    fn remember_search(&mut self, pattern: &str, forward: bool) {
        self.last_search = Some((pattern.to_string(), forward));
        self.registers.set(
            '/',
            Register {
                text: pattern.to_string(),
                linewise: false,
            },
        );
    }

    /// The start of the `n`th match of `pattern` from `from`, wrapping around the buffer.
    pub(crate) fn find(
        &mut self,
        h: &mut dyn VimHost,
        pattern: &str,
        forward: bool,
        from: usize,
        n: usize,
    ) -> Option<usize> {
        let q = query(pattern);
        if validate(&q).is_err() {
            self.state.message = format!("E383: Invalid search string: {pattern}");
            return None;
        }
        h.highlight(Some(highlight_regex(&q)));
        let r = h.rope();
        let (mut pos, mut wrapped) = (from, false);
        for _ in 0..n {
            let found = if forward {
                find_next(r, &q, pos + 1, true)
            } else {
                find_prev(r, &q, pos, true)
            };
            let Some(m) = found else {
                self.state.message = format!("E486: Pattern not found: {pattern}");
                return None;
            };
            wrapped |= if forward {
                m.start <= pos
            } else {
                m.start >= pos
            };
            pos = m.start;
        }
        self.state.message = match (wrapped, forward) {
            (true, true) => "search hit BOTTOM, continuing at TOP".into(),
            (true, false) => "search hit TOP, continuing at BOTTOM".into(),
            (false, true) => format!("/{pattern}"),
            (false, false) => format!("?{pattern}"),
        };
        Some(pos)
    }

    /// Run a command line. `visual` is the line range when it was opened from Visual mode.
    pub(crate) fn ex(&mut self, h: &mut dyn VimHost, line: &str, visual: Option<(usize, usize)>) {
        let line = line.trim_start_matches([':', ' ', '\t']).trim_end();
        let r = h.rope();
        let cur = line_of(r, h.selection().1);
        let last = last_line(r);
        let visual_marks = self.last_visual.map(|(_, a, b)| {
            let (a, b) = (line_of(r, a.min(b)), line_of(r, a.max(b)));
            (a, b)
        });
        let range = if let Some(rest) = line.strip_prefix('%') {
            Some(((0, last), rest))
        } else {
            let Some((a, rest)) = address(line, cur, last, visual_marks) else {
                self.state.message = format!("E16: Invalid range: {line}");
                return;
            };
            match (a, rest.strip_prefix([',', ';'])) {
                (a, Some(rest2)) => {
                    let Some((b, rest2)) = address(rest2, cur, last, visual_marks) else {
                        self.state.message = format!("E16: Invalid range: {line}");
                        return;
                    };
                    let (a, b) = (a.unwrap_or(cur), b.unwrap_or(cur));
                    Some(((a.min(b), a.max(b)), rest2))
                }
                (Some(a), None) => Some(((a, a), rest)),
                (None, None) => None,
            }
        };
        let (range, cmd) = match range {
            Some((range, cmd)) => (Some(range), cmd.trim()),
            None => (visual, line),
        };
        let (name, args) = match cmd.find(|c: char| !c.is_ascii_alphabetic()) {
            Some(0) => ("", cmd),
            Some(i) => (&cmd[..i], &cmd[i..]),
            None => (cmd, ""),
        };
        match (name, args.trim()) {
            ("", "") => {
                if let Some((_, l)) = range {
                    let pos = first_non_blank(h.rope(), l);
                    self.set_cursor(h, pos);
                }
            }
            ("w" | "write", "" | "!") => h.save(),
            ("wq" | "x" | "xit" | "exit", "" | "!") => {
                h.save();
                h.ex("q");
            }
            ("noh" | "nohl" | "nohlsearch", "") => h.highlight(None),
            ("s" | "substitute", _)
                if !args.is_empty() && !args.starts_with(char::is_whitespace) =>
            {
                let range = range.unwrap_or((cur, cur));
                self.substitute(h, range, args);
            }
            _ => {
                if !h.ex(line) {
                    self.state.message = format!("E492: Not an editor command: {line}");
                }
            }
        }
    }

    /// `:s/pattern/replacement/flags` over lines `l1..=l2`, as one undo step.
    fn substitute(&mut self, h: &mut dyn VimHost, (l1, l2): (usize, usize), args: &str) {
        let Some(delim) = args.chars().next() else {
            return;
        };
        let (pattern, rest) = split_at_delim(&args[delim.len_utf8()..], delim);
        let (rep, flags) = match rest {
            Some(rest) => {
                let (rep, flags) = split_at_delim(rest, delim);
                (rep, flags.unwrap_or(""))
            }
            None => (String::new(), ""),
        };
        let pattern = if pattern.is_empty() {
            match &self.last_search {
                Some((p, _)) => p.clone(),
                None => {
                    self.state.message = "E35: No previous regular expression".into();
                    return;
                }
            }
        } else {
            pattern
        };
        self.remember_search(&pattern, true);
        let mut q = query(&pattern);
        let (mut global, mut quiet) = (false, false);
        for f in flags.trim().chars() {
            match f {
                'g' => global = true,
                'i' => q.case = CaseMode::Insensitive,
                'I' => q.case = CaseMode::Sensitive,
                'e' => quiet = true,
                '&' | ' ' => {}
                other => {
                    self.state.message = format!("E488: Trailing characters: {other}");
                    return;
                }
            }
        }
        if validate(&q).is_err() {
            self.state.message = format!("E383: Invalid search string: {pattern}");
            return;
        }
        h.highlight(Some(highlight_regex(&q)));
        let pieces = replacement(&rep);
        let r = h.rope();
        let (start, end) = (line_start(r, l1), line_end(r, l2));
        let mut edits = Vec::new();
        let mut last_line_done = None;
        for groups in captures_in(r, &q, start..end) {
            let Some(m) = groups[0].clone() else { continue };
            let line = line_of(r, m.start);
            if !global && last_line_done == Some(line) {
                continue;
            }
            last_line_done = Some(line);
            let mut text = String::new();
            for p in &pieces {
                match p {
                    Piece::Text(t) => text.push_str(t),
                    Piece::Group(g) => {
                        if let Some(Some(span)) = groups.get(*g) {
                            text.extend(r.slice(span.clone()).chars());
                        }
                    }
                }
            }
            edits.push((m, text));
        }
        let Some((last_match, _)) = edits.last().cloned() else {
            if !quiet {
                self.state.message = format!("E486: Pattern not found: {pattern}");
            }
            return;
        };
        let lines = {
            let mut seen: Vec<usize> = edits.iter().map(|(m, _)| line_of(r, m.start)).collect();
            seen.dedup();
            seen.len()
        };
        // The last match's start, after the edits before it change the text in front of it.
        let shift: isize = edits[..edits.len() - 1]
            .iter()
            .map(|(m, t)| t.chars().count() as isize - m.len() as isize)
            .sum();
        let count = edits.len();
        self.begin(h);
        for (m, text) in edits.into_iter().rev() {
            h.replace(m, &text);
        }
        let r = h.rope();
        let at = (last_match.start as isize + shift).max(0) as usize;
        let pos = first_non_blank(r, line_of(r, at));
        self.set_cursor(h, pos);
        self.end(h);
        if count > 1 {
            let s = |n: usize| if n == 1 { "" } else { "s" };
            self.state.message = format!(
                "{count} substitution{} on {lines} line{}",
                s(count),
                s(lines)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vim_word_boundaries_are_valid_patterns() {
        assert!(validate(&query(r"\<foo\>")).is_ok());
        assert_eq!(query(r"foo\c").case, CaseMode::Insensitive);
        assert_eq!(query(r"\CFoo").pattern, "Foo");
        assert_eq!(highlight_regex(&query("foo")), "(?mi)foo");
        assert_eq!(highlight_regex(&query("Foo")), "(?m)Foo");
    }
}

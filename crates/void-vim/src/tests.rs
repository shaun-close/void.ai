//! Engine tests against a host backed by a real `void_editor::Buffer`.

use std::ops::Range;

use ropey::Rope;
use void_editor::Buffer;

use crate::{Engine, KeyLike, Mode, ScrollAt, VimHost, key_name};

mod extensions;

struct Host {
    buf: Buffer,
    sel: (usize, usize),
    top: usize,
    height: usize,
    saves: usize,
    scrolls: Vec<(usize, ScrollAt)>,
    clipboard: Option<String>,
    clip_sets: Vec<String>,
    exs: Vec<String>,
    highlights: Vec<Option<String>>,
    prompts: Vec<char>,
}

impl Host {
    fn new(text: &str) -> Host {
        Host {
            buf: Buffer::new(text),
            sel: (0, 0),
            top: 0,
            height: 20,
            saves: 0,
            scrolls: Vec::new(),
            clipboard: None,
            clip_sets: Vec::new(),
            exs: Vec::new(),
            highlights: Vec::new(),
            prompts: Vec::new(),
        }
    }

    fn text(&self) -> String {
        self.buf.rope().to_string()
    }

    /// The head as (line, column).
    fn cursor(&self) -> (usize, usize) {
        let head = self.sel.1;
        let line = self.buf.line_of(head);
        (line, head - self.buf.line_start(line))
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        let p = self.buf.line_start(line) + col;
        self.sel = (p, p);
    }
}

impl VimHost for Host {
    fn rope(&self) -> &Rope {
        self.buf.rope()
    }
    fn selection(&self) -> (usize, usize) {
        self.sel
    }
    fn select(&mut self, anchor: usize, head: usize) {
        self.sel = (anchor, head);
    }
    fn replace(&mut self, range: Range<usize>, text: &str) {
        self.buf.replace(range, text);
    }
    fn begin_group(&mut self) {
        self.buf.begin_group(self.sel.1);
    }
    fn end_group(&mut self) {
        self.buf.end_group(self.sel.1);
    }
    fn undo(&mut self) {
        if let Some(c) = self.buf.undo() {
            self.sel = (c, c);
        }
    }
    fn redo(&mut self) {
        if let Some(c) = self.buf.redo() {
            self.sel = (c, c);
        }
    }
    fn newline(&mut self) {
        let head = self.sel.1;
        let line = self.buf.line_of(head);
        let indent: String = self
            .buf
            .rope()
            .line(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let text = format!("\n{indent}");
        self.buf.replace(head..head, &text);
        let p = head + text.chars().count();
        self.sel = (p, p);
    }
    fn tab(&mut self) {
        let head = self.sel.1;
        self.buf.replace(head..head, "    ");
        self.sel = (head + 4, head + 4);
    }
    fn visible_lines(&self) -> (usize, usize) {
        let last = self.buf.line_count() - 1;
        (self.top, (self.top + self.height - 1).min(last))
    }
    fn scroll_to(&mut self, line: usize, at: ScrollAt) {
        self.scrolls.push((line, at));
        self.top = match at {
            ScrollAt::Top => line,
            ScrollAt::Center => line.saturating_sub(self.height / 2),
            ScrollAt::Bottom => (line + 1).saturating_sub(self.height),
        };
    }
    fn prompt(&mut self, prefix: char) {
        self.prompts.push(prefix);
    }
    fn highlight(&mut self, pattern: Option<String>) {
        self.highlights.push(pattern);
    }
    fn save(&mut self) {
        self.saves += 1;
    }
    fn ex(&mut self, cmd: &str) -> bool {
        self.exs.push(cmd.to_string());
        matches!(cmd, "q" | "q!" | "e foo.rs" | "wa" | "qa")
    }
    fn clipboard_get(&self) -> Option<String> {
        self.clipboard.clone()
    }
    fn clipboard_set(&mut self, s: &str) {
        self.clip_sets.push(s.to_string());
        self.clipboard = Some(s.to_string());
    }
    fn indent_unit(&self) -> &str {
        "    "
    }
}

/// Split Vim key notation (`"dw<Esc>"`) into key names.
fn keys(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(len) = chars[i + 1..].iter().position(|c| *c == '>')
        {
            let name: String = chars[i + 1..i + 1 + len].iter().collect();
            if name.len() >= 2 && name.chars().next().unwrap().is_ascii_alphabetic() {
                out.push(format!("<{name}>"));
                i += len + 2;
                continue;
            }
        }
        out.push(chars[i].to_string());
        i += 1;
    }
    out
}

fn feed(e: &mut Engine, h: &mut Host, s: &str) {
    for k in keys(s) {
        e.key(h, &k);
    }
}

fn vim(text: &str) -> (Engine, Host) {
    (Engine::new(), Host::new(text))
}

fn answer(e: &mut Engine, h: &mut Host, s: &str) {
    e.prompt_answer(h, Some(s.to_string()));
}

fn reg(e: &Engine, name: char) -> Option<(String, bool)> {
    e.registers()
        .get(name)
        .map(|r| (r.text.clone(), r.linewise()))
}

#[test]
fn vim_basic_motions() {
    let (mut e, mut h) = vim("hello world foo\n  second line\nthird");
    let at = |e: &mut Engine, h: &mut Host, k: &str, want: (usize, usize)| {
        feed(e, h, k);
        assert_eq!(h.cursor(), want, "after {k}");
    };
    at(&mut e, &mut h, "l", (0, 1));
    at(&mut e, &mut h, "h", (0, 0));
    at(&mut e, &mut h, "h", (0, 0));
    at(&mut e, &mut h, "w", (0, 6));
    at(&mut e, &mut h, "e", (0, 10));
    at(&mut e, &mut h, "b", (0, 6));
    at(&mut e, &mut h, "2w", (1, 2));
    at(&mut e, &mut h, "gg", (0, 0));
    at(&mut e, &mut h, "2w", (0, 12));
    at(&mut e, &mut h, "$", (0, 14));
    at(&mut e, &mut h, "j", (1, 12));
    at(&mut e, &mut h, "j", (2, 4));
    at(&mut e, &mut h, "k", (1, 12));
    at(&mut e, &mut h, "0", (1, 0));
    at(&mut e, &mut h, "^", (1, 2));
    at(&mut e, &mut h, "k", (0, 2));
    at(&mut e, &mut h, "5l", (0, 7));
    at(&mut e, &mut h, "j", (1, 7));
    at(&mut e, &mut h, "G", (2, 0));
    at(&mut e, &mut h, "gg", (0, 0));
    at(&mut e, &mut h, "2G", (1, 2));
    at(&mut e, &mut h, "3gg", (2, 0));
    at(&mut e, &mut h, "j", (2, 0));
    at(&mut e, &mut h, "100k", (0, 0));
    at(&mut e, &mut h, "ge", (0, 0));
    at(&mut e, &mut h, "$ge", (0, 10));
    at(&mut e, &mut h, "3|", (0, 2));
    at(&mut e, &mut h, "+", (1, 2));
    at(&mut e, &mut h, "-", (0, 0));
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(h.sel.0, h.sel.1, "no selection in Normal mode");
}

#[test]
fn vim_insert_append_open() {
    let (mut e, mut h) = vim("abc\ndef");
    feed(&mut e, &mut h, "iX");
    assert_eq!(e.state().mode, Mode::Insert);
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "<Esc>");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(h.text(), "Xabc\ndef");
    assert_eq!(h.cursor(), (0, 0), "Esc steps left");

    feed(&mut e, &mut h, "aY<Esc>");
    assert_eq!(h.text(), "XYabc\ndef");
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "AZ<Esc>");
    assert_eq!(h.text(), "XYabcZ\ndef");
    assert_eq!(h.cursor(), (0, 5));
    feed(&mut e, &mut h, "I-<Esc>");
    assert_eq!(h.text(), "-XYabcZ\ndef");
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "onew<Esc>");
    assert_eq!(h.text(), "-XYabcZ\nnew\ndef");
    assert_eq!(h.cursor(), (1, 2));
    feed(&mut e, &mut h, "Oup<Esc>");
    assert_eq!(h.text(), "-XYabcZ\nup\nnew\ndef");
    assert_eq!(h.cursor(), (1, 1));
    feed(&mut e, &mut h, "0i<Esc>");
    assert_eq!(h.cursor(), (1, 0), "Esc does not step past the line start");

    // typed text (IME / paste) goes in as-is.
    feed(&mut e, &mut h, "gg$a");
    e.typed(&mut h, " héllo");
    feed(&mut e, &mut h, "<Esc>");
    assert_eq!(h.buf.slice(0..13), "-XYabcZ héllo");

    // o keeps the indentation (host auto-indent); O copies it too.
    let (mut e, mut h) = vim("    foo");
    feed(&mut e, &mut h, "obar<Esc>");
    assert_eq!(h.text(), "    foo\n    bar");
    feed(&mut e, &mut h, "Obaz<Esc>");
    assert_eq!(h.text(), "    foo\n    baz\n    bar");

    // Insert-mode editing keys.
    let (mut e, mut h) = vim("one");
    feed(&mut e, &mut h, "A two<BS><BS>o<CR>x<Esc>");
    assert_eq!(h.text(), "one to\nx");
    feed(&mut e, &mut h, "A foo bar<C-w><C-w>baz<Esc>");
    assert_eq!(h.text(), "one to\nx baz");

    // A count repeats the insertion.
    let (mut e, mut h) = vim("");
    feed(&mut e, &mut h, "3ia<Esc>");
    assert_eq!(h.text(), "aaa");
    assert_eq!(h.cursor(), (0, 2));
    feed(&mut e, &mut h, "2ox<Esc>");
    assert_eq!(h.text(), "aaa\nx\nx");
}

#[test]
fn vim_visual_and_visual_line() {
    let (mut e, mut h) = vim("one two three\nfour five\nsix");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "v");
    assert_eq!(e.state().mode, Mode::Visual);
    feed(&mut e, &mut h, "e");
    assert_eq!(h.sel, (4, 6), "anchor and head, inclusive");
    feed(&mut e, &mut h, "y");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(reg(&e, '"'), Some(("two".into(), false)));
    assert_eq!(h.cursor(), (0, 4));

    feed(&mut e, &mut h, "ved");
    assert_eq!(h.text(), "one  three\nfour five\nsix");
    assert_eq!(h.cursor(), (0, 4));

    // Charwise across lines includes the char under the head.
    feed(&mut e, &mut h, "vjd");
    assert_eq!(h.text(), "one five\nsix");

    // o swaps the ends; gv reselects.
    let (mut e, mut h) = vim("abcdef");
    h.set_cursor(0, 2);
    feed(&mut e, &mut h, "vlloh");
    assert_eq!(h.sel, (4, 1));
    feed(&mut e, &mut h, "<Esc>");
    assert_eq!(e.state().mode, Mode::Normal);
    feed(&mut e, &mut h, "gvd");
    assert_eq!(h.text(), "af");

    // Visual line.
    let (mut e, mut h) = vim("one\ntwo\nthree\nfour");
    h.set_cursor(1, 1);
    feed(&mut e, &mut h, "Vjy");
    assert_eq!(reg(&e, '"'), Some(("two\nthree\n".into(), true)));
    assert_eq!(h.cursor(), (1, 1));
    feed(&mut e, &mut h, "Vj>");
    assert_eq!(h.text(), "one\n    two\n    three\nfour");
    assert_eq!(h.cursor(), (1, 4));
    feed(&mut e, &mut h, "V<");
    assert_eq!(h.text(), "one\ntwo\n    three\nfour");
    feed(&mut e, &mut h, "Vjd");
    assert_eq!(h.text(), "one\nfour");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "Vkd");
    assert_eq!(h.text(), "");

    // v$ includes the line break; vU upper-cases.
    let (mut e, mut h) = vim("ab\ncd");
    h.set_cursor(0, 1);
    feed(&mut e, &mut h, "v$d");
    assert_eq!(h.text(), "acd");
    feed(&mut e, &mut h, "0vlU");
    assert_eq!(h.text(), "ACd");
    // V then c replaces the lines with one empty line.
    feed(&mut e, &mut h, "Vcnew<Esc>");
    assert_eq!(h.text(), "new");
}

#[test]
fn vim_replace_mode() {
    let (mut e, mut h) = vim("abcdef");
    feed(&mut e, &mut h, "RXY");
    assert_eq!(e.state().mode, Mode::Replace);
    assert_eq!(h.text(), "XYcdef");
    feed(&mut e, &mut h, "<BS>");
    assert_eq!(h.text(), "Xbcdef");
    feed(&mut e, &mut h, "<BS>");
    assert_eq!(h.text(), "abcdef");
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "Z<Esc>");
    assert_eq!(h.text(), "Zbcdef");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(h.cursor(), (0, 0));

    // Past the end of the line, R appends.
    feed(&mut e, &mut h, "$R123<Esc>");
    assert_eq!(h.text(), "Zbcde123");
    assert_eq!(h.cursor(), (0, 7));
    feed(&mut e, &mut h, "<BS>");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "Zbcdef", "one undo step for the session");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "abcdef");
}

#[test]
fn vim_ex_write_and_substitute() {
    let (mut e, mut h) = vim("aaa\nbab\nccc");
    feed(&mut e, &mut h, ":");
    assert_eq!(h.prompts, vec![':']);
    answer(&mut e, &mut h, "w");
    assert_eq!(h.saves, 1);

    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "%s/a/b/g");
    assert_eq!(h.text(), "bbb\nbbb\nccc");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "aaa\nbab\nccc", "one undo step for the whole :s");

    feed(&mut e, &mut h, "gg:");
    answer(&mut e, &mut h, "s/a/X/");
    assert_eq!(h.text(), "Xaa\nbab\nccc", "without g only the first match");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "2,3s/[bc]/-/g");
    assert_eq!(h.text(), "Xaa\n-a-\n---");
    assert_eq!(h.cursor(), (2, 0), "cursor on the last substituted line");

    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "%s/(\\w)a(\\w)/\\2a\\1/");
    assert_eq!(h.text(), "aaX\n-a-\n---", "capture groups");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "1s/a/[&]/g");
    assert_eq!(h.text(), "[a][a]X\n-a-\n---", "& is the match");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "s/X/Q/i");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "1s#q#x#I");
    assert_eq!(h.text(), "[a][a]Q\n-a-\n---", "I is case-sensitive");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "1s/\\]/\\r/");
    assert_eq!(h.text(), "[a\n[a]Q\n-a-\n---", "\\r splits the line");

    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "%s/nothing/x/");
    assert_eq!(e.state().message, "E486: Pattern not found: nothing");

    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "3");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "$");
    assert_eq!(h.cursor(), (3, 0));
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "noh");
    assert_eq!(h.highlights.last(), Some(&None));
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "q");
    assert_eq!(h.exs, vec!["q"]);
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "wq");
    assert_eq!(h.saves, 2);
    assert_eq!(h.exs, vec!["q", "q"]);
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "x");
    assert_eq!(h.saves, 3);
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "e foo.rs");
    assert_eq!(h.exs.last().map(String::as_str), Some("e foo.rs"));
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "bogus");
    assert_eq!(e.state().message, "E492: Not an editor command: bogus");

    // `:` from Visual mode works on the selected lines.
    let (mut e, mut h) = vim("a\na\na\na");
    h.set_cursor(1, 0);
    feed(&mut e, &mut h, "Vj:");
    assert_eq!(e.state().mode, Mode::Normal);
    answer(&mut e, &mut h, "s/a/b/");
    assert_eq!(h.text(), "a\nb\nb\na");
    feed(&mut e, &mut h, "gg:");
    answer(&mut e, &mut h, "'<,'>s/b/c/");
    assert_eq!(h.text(), "a\nc\nc\na");
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, ".,+1s/^/> /");
    assert_eq!(
        h.text(),
        "a\nc\n> c\n> a",
        "from the cursor, left on the last :s line"
    );
}

#[test]
fn vim_word_motions_unicode() {
    // héllo wörld 🦀🦀 世界abc, ok
    // 0     6     12 15 17 20 22
    let (mut e, mut h) = vim("héllo wörld 🦀🦀 世界abc, ok");
    let mut stops = Vec::new();
    for _ in 0..7 {
        feed(&mut e, &mut h, "w");
        stops.push(h.cursor().1);
    }
    assert_eq!(stops, vec![6, 12, 15, 17, 20, 22, 23]);
    let mut stops = Vec::new();
    for _ in 0..7 {
        feed(&mut e, &mut h, "b");
        stops.push(h.cursor().1);
    }
    assert_eq!(stops, vec![22, 20, 17, 15, 12, 6, 0]);
    let mut stops = Vec::new();
    for _ in 0..7 {
        feed(&mut e, &mut h, "e");
        stops.push(h.cursor().1);
    }
    assert_eq!(stops, vec![4, 10, 13, 16, 19, 20, 23]);
    feed(&mut e, &mut h, "0");
    let mut stops = Vec::new();
    for _ in 0..4 {
        feed(&mut e, &mut h, "W");
        stops.push(h.cursor().1);
    }
    assert_eq!(stops, vec![6, 12, 15, 22]);

    feed(&mut e, &mut h, "0dw");
    assert_eq!(h.text(), "wörld 🦀🦀 世界abc, ok");
    feed(&mut e, &mut h, "wx");
    assert_eq!(h.text(), "wörld 🦀 世界abc, ok", "x deletes a whole emoji");
    feed(&mut e, &mut h, "wcwX<Esc>");
    assert_eq!(h.text(), "wörld 🦀 Xabc, ok");

    // An empty line is a word; w stops on it.
    let (mut e, mut h) = vim("a\n\n  b");
    feed(&mut e, &mut h, "w");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "w");
    assert_eq!(h.cursor(), (2, 2));
    feed(&mut e, &mut h, "b");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "e");
    assert_eq!(h.cursor(), (2, 2), "e skips empty lines");
}

#[test]
fn vim_counts_with_operators() {
    let (mut e, mut h) = vim("one two three four\nl2\nl3\nl4");
    feed(&mut e, &mut h, "3dw");
    assert_eq!(h.text(), "four\nl2\nl3\nl4");
    assert_eq!(reg(&e, '"'), Some(("one two three ".into(), false)));
    feed(&mut e, &mut h, "d2j");
    assert_eq!(h.text(), "l4");
    assert_eq!(reg(&e, '"'), Some(("four\nl2\nl3\n".into(), true)));

    let (mut e, mut h) = vim("one two\nnext");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "c$X<Esc>");
    assert_eq!(h.text(), "one X\nnext");
    feed(&mut e, &mut h, "0d$");
    assert_eq!(h.text(), "\nnext");

    let (mut e, mut h) = vim("a b c d e f g");
    feed(&mut e, &mut h, "2d2w");
    assert_eq!(h.text(), "e f g", "counts multiply");
    feed(&mut e, &mut h, "10x");
    assert_eq!(h.text(), "", "x stops at the end of the line");

    let (mut e, mut h) = vim("1\n2\n3\n4\n5");
    h.set_cursor(1, 0);
    feed(&mut e, &mut h, "2dd");
    assert_eq!(h.text(), "1\n4\n5");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "G");
    feed(&mut e, &mut h, "dd");
    assert_eq!(h.text(), "1\n4");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "5dd");
    assert_eq!(h.text(), "1", "a count past the end deletes to the end");
    feed(&mut e, &mut h, "dj");
    assert_eq!(h.text(), "1", "dj on the last line does nothing");

    let (mut e, mut h) = vim("one\ntwo\nthree");
    feed(&mut e, &mut h, "y2jGp");
    assert_eq!(h.text(), "one\ntwo\nthree\none\ntwo\nthree");
    assert_eq!(h.cursor(), (3, 0));
    feed(&mut e, &mut h, "3J");
    assert_eq!(h.text(), "one\ntwo\nthree\none two three");
    feed(&mut e, &mut h, "gg2yy2P");
    assert_eq!(
        h.text(),
        "one\ntwo\none\ntwo\none\ntwo\nthree\none two three"
    );
}

#[test]
fn vim_text_objects() {
    let (mut e, mut h) = vim("foo(a, b) bar");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "ci(x<Esc>");
    assert_eq!(h.text(), "foo(x) bar");
    feed(&mut e, &mut h, "da(");
    assert_eq!(h.text(), "foo bar");

    let (mut e, mut h) = vim("f(a(b)c)");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "di(");
    assert_eq!(h.text(), "f(a()c)");
    feed(&mut e, &mut h, "2di)");
    assert_eq!(h.text(), "f()");

    let (mut e, mut h) = vim("say \"hi there\" now");
    h.set_cursor(0, 6);
    feed(&mut e, &mut h, "da\"");
    assert_eq!(h.text(), "say now");
    let (mut e, mut h) = vim("x = \"abc\"");
    feed(&mut e, &mut h, "ci\"Q<Esc>");
    assert_eq!(h.text(), "x = \"Q\"", "a quote after the cursor");
    let (mut e, mut h) = vim("a 'b' `c` [d] {e} <f>");
    feed(&mut e, &mut h, "fbdi'fcdi`fddi[fedi{ffdi<");
    assert_eq!(h.text(), "a '' `` [] {} <>");

    let (mut e, mut h) = vim("hello world");
    h.set_cursor(0, 7);
    feed(&mut e, &mut h, "yiw");
    assert_eq!(reg(&e, '"'), Some(("world".into(), false)));
    assert_eq!(h.cursor(), (0, 6));

    let (mut e, mut h) = vim("foo bar baz");
    h.set_cursor(0, 5);
    feed(&mut e, &mut h, "daw");
    assert_eq!(h.text(), "foo baz");
    feed(&mut e, &mut h, "wdaw");
    assert_eq!(h.text(), "foo", "no trailing blank: takes the leading one");
    let (mut e, mut h) = vim("a.b c.d");
    feed(&mut e, &mut h, "diW");
    assert_eq!(h.text(), " c.d");
    feed(&mut e, &mut h, "diw");
    assert_eq!(h.text(), "c.d", "iw on blanks takes the blanks");

    // Multi-line braces: the inner lines, linewise.
    let (mut e, mut h) = vim("fn x() {\n    a;\n    b;\n}");
    h.set_cursor(1, 4);
    feed(&mut e, &mut h, "yi{");
    assert_eq!(reg(&e, '"'), Some(("    a;\n    b;\n".into(), true)));
    feed(&mut e, &mut h, "ci{z<Esc>");
    assert_eq!(h.text(), "fn x() {\n    z\n}");
    feed(&mut e, &mut h, "di{");
    assert_eq!(h.text(), "fn x() {\n}");

    let (mut e, mut h) = vim("a\nb\n\nc\n\nd");
    feed(&mut e, &mut h, "dap");
    assert_eq!(h.text(), "c\n\nd");
    feed(&mut e, &mut h, "yip");
    assert_eq!(reg(&e, '"'), Some(("c\n".into(), true)));

    // Visual mode text objects set the selection.
    let (mut e, mut h) = vim("x (abc) y");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "vi(");
    assert_eq!(h.sel, (3, 5));
    feed(&mut e, &mut h, "a(");
    assert_eq!(h.sel, (2, 6));
}

#[test]
fn vim_dot_and_undo() {
    let (mut e, mut h) = vim("a b c d e");
    feed(&mut e, &mut h, "dw");
    assert_eq!(h.text(), "b c d e");
    feed(&mut e, &mut h, ".");
    assert_eq!(h.text(), "c d e");
    feed(&mut e, &mut h, ".");
    assert_eq!(h.text(), "d e");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "c d e", "u undoes one repeat");
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "b c d e");
    feed(&mut e, &mut h, "2.");
    assert_eq!(h.text(), "d e", "a count replaces the original count");

    // A change with an insertion repeats the insertion too.
    let (mut e, mut h) = vim("foo bar baz");
    feed(&mut e, &mut h, "ciwX<Esc>w.");
    assert_eq!(h.text(), "X X baz");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "X bar baz");
    let (mut e, mut h) = vim("a\nb");
    feed(&mut e, &mut h, "A;<Esc>j.");
    assert_eq!(h.text(), "a;\nb;");
    let (mut e, mut h) = vim("x");
    feed(&mut e, &mut h, "3ia<Esc>.");
    assert_eq!(h.text(), "aaaaaax");
    let (mut e, mut h) = vim("1\n2\n3\n4");
    feed(&mut e, &mut h, ">>j.j3.");
    assert_eq!(h.text(), "    1\n    2\n    3\n    4");
    let (mut e, mut h) = vim("abcdef");
    feed(&mut e, &mut h, "rx2l.");
    assert_eq!(h.text(), "xbxdef");
    feed(&mut e, &mut h, "ipre<Esc>u");
    assert_eq!(h.text(), "xbxdef", "an insert session is one undo step");

    // Visual operators repeat on the same amount of text.
    let (mut e, mut h) = vim("abcdefgh");
    feed(&mut e, &mut h, "vld.");
    assert_eq!(h.text(), "efgh");
    let (mut e, mut h) = vim("1\n2\n3\n4\n5");
    feed(&mut e, &mut h, "Vjd.");
    assert_eq!(h.text(), "5");
}

#[test]
fn vim_macro_multi_line() {
    let (mut e, mut h) = vim("a\nb\nc\nd\ne");
    feed(&mut e, &mut h, "qa");
    assert_eq!(e.state().recording, Some('a'));
    feed(&mut e, &mut h, "0iX<Esc>jq");
    assert_eq!(e.state().recording, None);
    assert_eq!(h.text(), "Xa\nb\nc\nd\ne");
    assert_eq!(h.cursor(), (1, 0));
    assert_eq!(reg(&e, 'a'), Some(("0iX\u{1b}j".into(), false)));
    feed(&mut e, &mut h, "3@a");
    assert_eq!(h.text(), "Xa\nXb\nXc\nXd\ne");
    assert_eq!(h.cursor(), (4, 0));
    feed(&mut e, &mut h, "@@");
    assert_eq!(h.text(), "Xa\nXb\nXc\nXd\nXe");

    // A failing motion stops the macro: 100@a runs to the end, once per line.
    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "qa0iX<Esc>jq100@a");
    assert_eq!(h.text(), "Xa\nXb\nXc");

    // A recursive macro runs until it fails.
    let (mut e, mut h) = vim("1\n2\n3\n4");
    feed(&mut e, &mut h, "qbqqbA!<Esc>j@bq");
    assert_eq!(h.text(), "1!\n2\n3\n4");
    feed(&mut e, &mut h, "@b");
    assert_eq!(h.text(), "1!\n2!\n3!\n4!");

    // Command-line input is recorded and replayed without asking the host again.
    let (mut e, mut h) = vim("x1\nx2\nx3");
    feed(&mut e, &mut h, "qc:");
    answer(&mut e, &mut h, "s/x/y/");
    feed(&mut e, &mut h, "jq");
    assert_eq!(h.prompts.len(), 1);
    feed(&mut e, &mut h, "2@c");
    assert_eq!(h.text(), "y1\ny2\ny3");
    assert_eq!(h.prompts.len(), 1);

    // Uppercase appends; @: repeats the last command line.
    let (mut e, mut h) = vim("a\na\na");
    feed(&mut e, &mut h, "qdxqqDjq");
    assert_eq!(reg(&e, 'd'), Some(("xj".into(), false)));
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "s/a/b/");
    feed(&mut e, &mut h, "j@:");
    assert_eq!(h.text(), "\nb\nb");
}

#[test]
fn vim_search_n_n() {
    let (mut e, mut h) = vim("foo bar\nbaz foo\nfoo end");
    feed(&mut e, &mut h, "/");
    assert_eq!(h.prompts, vec!['/']);
    answer(&mut e, &mut h, "foo");
    assert_eq!(h.cursor(), (1, 4));
    assert!(h.highlights.last().unwrap().is_some());
    feed(&mut e, &mut h, "n");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, "n");
    assert_eq!(h.cursor(), (0, 0));
    assert!(
        e.state().message.contains("BOTTOM"),
        "{}",
        e.state().message
    );
    feed(&mut e, &mut h, "N");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, "?");
    answer(&mut e, &mut h, "ba");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "n");
    assert_eq!(h.cursor(), (0, 4), "n keeps the ? direction");
    feed(&mut e, &mut h, "N");
    assert_eq!(h.cursor(), (1, 0));

    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "zzz");
    assert_eq!(e.state().message, "E486: Pattern not found: zzz");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "(");
    assert!(
        e.state().message.starts_with("E383"),
        "{}",
        e.state().message
    );

    // * and # search the word under the cursor, whole word.
    let (mut e, mut h) = vim("foo foobar foo\nfoo");
    feed(&mut e, &mut h, "*");
    assert_eq!(h.cursor(), (0, 11));
    feed(&mut e, &mut h, "n");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "#");
    assert_eq!(h.cursor(), (0, 11));
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "");
    assert_eq!(h.cursor(), (1, 0), "an empty pattern reuses the last one");
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "\\<bar");
    assert_eq!(e.state().message, "E486: Pattern not found: \\<bar");

    // Smart case.
    let (mut e, mut h) = vim("Foo foo FOO");
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "foo");
    assert_eq!(h.cursor(), (0, 4));
    feed(&mut e, &mut h, "n");
    assert_eq!(h.cursor(), (0, 8));
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "Foo");
    assert_eq!(h.cursor(), (0, 0));

    // Search as an operator target (exclusive; from column 0 to column 0 it is linewise).
    let (mut e, mut h) = vim("foo bar\nbaz foo\nend");
    feed(&mut e, &mut h, "d/");
    answer(&mut e, &mut h, "baz");
    assert_eq!(h.text(), "baz foo\nend");
    feed(&mut e, &mut h, "dn");
    assert_eq!(h.text(), "baz foo\nend", "no other match: nothing deleted");
    feed(&mut e, &mut h, "d/");
    answer(&mut e, &mut h, "foo");
    assert_eq!(h.text(), "foo\nend");
}

#[test]
fn vim_ctrl_r_redo() {
    let (mut e, mut h) = vim("abc");
    feed(&mut e, &mut h, "x");
    assert_eq!(h.text(), "bc");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "abc");
    feed(&mut e, &mut h, "<C-r>");
    assert_eq!(h.text(), "bc");
    feed(&mut e, &mut h, "ifoo bar<Esc>dd");
    assert_eq!(h.text(), "");
    feed(&mut e, &mut h, "2u");
    assert_eq!(h.text(), "bc");
    feed(&mut e, &mut h, "2<C-r>");
    assert_eq!(h.text(), "");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "foo barbc");
}

#[test]
fn vim_scroll_ctrl_d_moves_view() {
    let text: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    let (mut e, mut h) = vim(&text.join("\n"));
    feed(&mut e, &mut h, "<C-d>");
    assert_eq!(h.scrolls, vec![(10, ScrollAt::Top)]);
    assert_eq!(h.cursor(), (10, 0));
    feed(&mut e, &mut h, "<C-u>");
    assert_eq!(h.scrolls.last(), Some(&(0, ScrollAt::Top)));
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "<C-e>");
    assert_eq!(h.scrolls.last(), Some(&(1, ScrollAt::Top)));
    assert_eq!(h.cursor(), (1, 0), "the cursor stays in view");
    feed(&mut e, &mut h, "<C-f>");
    assert_eq!(h.scrolls.last(), Some(&(19, ScrollAt::Top)));
    assert_eq!(h.cursor(), (19, 0));
    feed(&mut e, &mut h, "<C-b>");
    assert_eq!(h.scrolls.last(), Some(&(1, ScrollAt::Top)));
    assert_eq!(h.cursor(), (19, 0));
    feed(&mut e, &mut h, "<C-y>");
    assert_eq!(h.scrolls.last(), Some(&(0, ScrollAt::Top)));
    assert_eq!(h.cursor(), (19, 0));
    feed(&mut e, &mut h, "<C-y>");
    assert_eq!(h.cursor(), (19, 0), "nothing above line 0");
    feed(&mut e, &mut h, "zz");
    assert_eq!(h.scrolls.last(), Some(&(19, ScrollAt::Center)));
    feed(&mut e, &mut h, "zt");
    assert_eq!(h.scrolls.last(), Some(&(19, ScrollAt::Top)));
    feed(&mut e, &mut h, "zb");
    assert_eq!(h.scrolls.last(), Some(&(19, ScrollAt::Bottom)));
    feed(&mut e, &mut h, "50z<CR>");
    assert_eq!(h.scrolls.last(), Some(&(49, ScrollAt::Top)));
    assert_eq!(h.cursor(), (49, 0));
    feed(&mut e, &mut h, "5<C-d>");
    assert_eq!(h.cursor(), (54, 0), "a count sets the scroll amount");
    feed(&mut e, &mut h, "<C-d>");
    assert_eq!(h.cursor(), (59, 0), "and it sticks");
    feed(&mut e, &mut h, "H");
    assert_eq!(h.cursor(), (h.top, 0));
    feed(&mut e, &mut h, "L");
    assert_eq!(h.cursor(), (h.top + 19, 0));
}

#[test]
fn vim_registers_clipboard() {
    let (mut e, mut h) = vim("hello\nworld");
    feed(&mut e, &mut h, "\"+yy");
    assert_eq!(h.clip_sets, vec!["hello\n"]);
    feed(&mut e, &mut h, "\"*yw");
    assert_eq!(h.clip_sets.last().map(String::as_str), Some("hello"));

    feed(&mut e, &mut h, "yy\"ayw\"Ayy");
    assert_eq!(reg(&e, 'a'), Some(("hello\nhello\n".into(), true)));
    feed(&mut e, &mut h, "j\"_dd");
    assert_eq!(h.text(), "hello");
    assert_eq!(
        reg(&e, '"'),
        Some(("hello\nhello\n".into(), true)),
        "_ is a black hole"
    );
    assert_eq!(
        reg(&e, '0'),
        Some(("hello\n".into(), true)),
        "0 keeps the last yank"
    );

    h.clipboard = Some("XYZ".into());
    feed(&mut e, &mut h, "\"+p");
    assert_eq!(h.text(), "hXYZello");
    assert_eq!(h.cursor(), (0, 3));
    h.clipboard = Some("line\n".into());
    feed(&mut e, &mut h, "\"+P");
    assert_eq!(h.text(), "line\nhXYZello");

    // Deletes: small ones to "-", lines to "1 (shifting the others).
    let (mut e, mut h) = vim("one\ntwo\nthree");
    feed(&mut e, &mut h, "dwddyy");
    assert_eq!(reg(&e, '-'), Some(("one".into(), false)));
    assert_eq!(reg(&e, '1'), Some(("\n".into(), true)));
    feed(&mut e, &mut h, "dd");
    assert_eq!(reg(&e, '1'), Some(("two\n".into(), true)));
    assert_eq!(reg(&e, '2'), Some(("\n".into(), true)));
    assert_eq!(reg(&e, '0'), Some(("two\n".into(), true)));
    feed(&mut e, &mut h, "\"2p");
    assert_eq!(h.text(), "three\n");
}

#[test]
fn vim_find_char_motions() {
    let (mut e, mut h) = vim("a(b, c(d), e)");
    feed(&mut e, &mut h, "f,");
    assert_eq!(h.cursor(), (0, 3));
    feed(&mut e, &mut h, ";");
    assert_eq!(h.cursor(), (0, 9));
    feed(&mut e, &mut h, ",");
    assert_eq!(h.cursor(), (0, 3));
    feed(&mut e, &mut h, "0t,");
    assert_eq!(h.cursor(), (0, 2));
    feed(&mut e, &mut h, ";");
    assert_eq!(h.cursor(), (0, 8), "; after t skips the adjacent match");
    feed(&mut e, &mut h, "F(");
    assert_eq!(h.cursor(), (0, 6));
    feed(&mut e, &mut h, "T(");
    assert_eq!(h.cursor(), (0, 2));
    feed(&mut e, &mut h, "fz");
    assert_eq!(h.cursor(), (0, 2), "not found: no move");
    feed(&mut e, &mut h, "2f(");
    assert_eq!(h.cursor(), (0, 2), "only one ( after the cursor");
    feed(&mut e, &mut h, "dt)");
    assert_eq!(h.text(), "a(), e)");
    feed(&mut e, &mut h, "0df)");
    assert_eq!(h.text(), ", e)");

    let (mut e, mut h) = vim("if (a[1] == b) { c }");
    h.set_cursor(0, 1);
    feed(&mut e, &mut h, "%");
    assert_eq!(h.cursor(), (0, 13), "% finds the next bracket on the line");
    feed(&mut e, &mut h, "%");
    assert_eq!(h.cursor(), (0, 3));
    feed(&mut e, &mut h, "f[d%");
    assert_eq!(h.text(), "if (a == b) { c }");
}

#[test]
fn vim_paragraph_and_exclusive_rules() {
    let (mut e, mut h) = vim("a\nb\n\nc\nd\n\ne");
    feed(&mut e, &mut h, "}");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, "}");
    assert_eq!(h.cursor(), (5, 0));
    feed(&mut e, &mut h, "}");
    assert_eq!(h.cursor(), (6, 0));
    feed(&mut e, &mut h, "2{");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, "gg");
    feed(&mut e, &mut h, "d}");
    assert_eq!(h.text(), "\nc\nd\n\ne", "from column 0 d}} is linewise");
    let (mut e, mut h) = vim("ab\ncd\n\nx");
    h.set_cursor(0, 1);
    feed(&mut e, &mut h, "d}");
    assert_eq!(
        h.text(),
        "a\n\nx",
        "mid-line: ends at the previous line's end"
    );

    // dw on the last word of a line stops at the line end; cw changes to the word end.
    let (mut e, mut h) = vim("foo bar\nbaz");
    h.set_cursor(0, 4);
    feed(&mut e, &mut h, "dw");
    assert_eq!(h.text(), "foo \nbaz");
    let (mut e, mut h) = vim("foo bar");
    feed(&mut e, &mut h, "cwx<Esc>");
    assert_eq!(h.text(), "x bar");
    feed(&mut e, &mut h, "cwz<Esc>");
    assert_eq!(h.text(), "z bar", "cw on a one-letter word");
    feed(&mut e, &mut h, "wcwy<Esc>");
    assert_eq!(h.text(), "z y");
    let (mut e, mut h) = vim("one\ntwo");
    feed(&mut e, &mut h, "dw");
    assert_eq!(h.text(), "\ntwo");
    feed(&mut e, &mut h, "de");
    assert_eq!(
        h.text(),
        "",
        "de from an empty line deletes up to the next word end"
    );
}

#[test]
fn vim_case_shift_paste_join() {
    let (mut e, mut h) = vim("hello World");
    feed(&mut e, &mut h, "g~w");
    assert_eq!(h.text(), "HELLO World");
    feed(&mut e, &mut h, "wgUiw");
    assert_eq!(h.text(), "HELLO WORLD");
    feed(&mut e, &mut h, "guu");
    assert_eq!(h.text(), "hello world");
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "3~");
    assert_eq!(h.text(), "HELlo world");
    assert_eq!(h.cursor(), (0, 3));
    feed(&mut e, &mut h, "gUU");
    assert_eq!(h.text(), "HELLO WORLD");
    feed(&mut e, &mut h, "2>>");
    assert_eq!(h.text(), "    HELLO WORLD");
    feed(&mut e, &mut h, "<<<<");
    assert_eq!(h.text(), "HELLO WORLD");

    let (mut e, mut h) = vim("ab\ncd");
    feed(&mut e, &mut h, "ylp");
    assert_eq!(h.text(), "aab\ncd");
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "3P");
    assert_eq!(h.text(), "aaaaab\ncd");
    assert_eq!(h.cursor(), (0, 3));
    feed(&mut e, &mut h, "yyjp");
    assert_eq!(h.text(), "aaaaab\ncd\naaaaab");
    assert_eq!(h.cursor(), (2, 0));
    feed(&mut e, &mut h, "kP");
    assert_eq!(h.text(), "aaaaab\naaaaab\ncd\naaaaab");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "ggdGixy<CR>z<Esc>0yyP");
    assert_eq!(h.text(), "xy\nz\nz");

    // Visual p swaps the selection and the register.
    let (mut e, mut h) = vim("one two");
    feed(&mut e, &mut h, "yiwwviwp");
    assert_eq!(h.text(), "one one");
    assert_eq!(reg(&e, '"'), Some(("two".into(), false)));

    let (mut e, mut h) = vim("a\n    b\n\nc)\n)");
    feed(&mut e, &mut h, "J");
    assert_eq!(h.text(), "a b\n\nc)\n)");
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "J");
    assert_eq!(
        h.text(),
        "a b\nc)\n)",
        "joining an empty line adds no space"
    );
    feed(&mut e, &mut h, "jJ");
    assert_eq!(h.text(), "a b\nc))", "no space before )");
    feed(&mut e, &mut h, "kgJ");
    assert_eq!(h.text(), "a bc))");

    let (mut e, mut h) = vim("abc");
    feed(&mut e, &mut h, "2rx");
    assert_eq!(h.text(), "xxc");
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "5rz");
    assert_eq!(h.text(), "xxc", "r fails past the end of the line");
    feed(&mut e, &mut h, "r<CR>");
    assert_eq!(h.text(), "x\nc");
}

#[test]
fn vim_pending_keys_and_escape() {
    let (mut e, mut h) = vim("abc def");
    feed(&mut e, &mut h, "2d");
    assert_eq!(e.state().pending, "2d");
    feed(&mut e, &mut h, "<Esc>");
    assert_eq!(e.state().pending, "");
    feed(&mut e, &mut h, "w");
    assert_eq!(h.text(), "abc def", "Esc cancelled the operator");
    assert_eq!(h.cursor(), (0, 4));
    feed(&mut e, &mut h, "\"a");
    assert_eq!(e.state().pending, "\"a");
    feed(&mut e, &mut h, "<C-c>");
    assert_eq!(e.state().pending, "");

    // Keys the engine does not use are left to the app.
    assert!(!e.key(&mut h, "<C-s>"));
    assert!(!e.key(&mut h, "<F5>"));
    assert!(e.key(&mut h, "<C-r>"));
    assert!(e.key(&mut h, "z"));
    assert!(e.key(&mut h, "<Esc>"));
    // Ctrl+[ is Esc.
    feed(&mut e, &mut h, "ix<C-[>");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(h.text(), "abc xdef");
    // An unknown command is swallowed whole.
    feed(&mut e, &mut h, "dq");
    assert_eq!(h.text(), "abc xdef");
    assert_eq!(e.state().pending, "");
}

#[test]
fn vim_key_names() {
    let k = |key, ctrl, alt, shift| {
        key_name(KeyLike {
            key,
            ctrl,
            alt,
            shift,
        })
    };
    assert_eq!(k("a", false, false, false).as_deref(), Some("a"));
    assert_eq!(k("A", false, false, false).as_deref(), Some("a"));
    assert_eq!(k("a", false, false, true).as_deref(), Some("A"));
    assert_eq!(k("?", false, false, true).as_deref(), Some("?"));
    assert_eq!(k("V", true, false, false).as_deref(), Some("<C-v>"));
    assert_eq!(k("v", true, false, true).as_deref(), Some("<C-S-v>"));
    assert_eq!(k("[", true, false, false).as_deref(), Some("<C-[>"));
    assert_eq!(k("x", false, true, false).as_deref(), Some("<M-x>"));
    assert_eq!(k("Escape", false, false, false).as_deref(), Some("<Esc>"));
    assert_eq!(k("Enter", false, false, false).as_deref(), Some("<CR>"));
    assert_eq!(k("Backspace", false, false, false).as_deref(), Some("<BS>"));
    assert_eq!(k("Tab", false, false, true).as_deref(), Some("<S-Tab>"));
    assert_eq!(
        k("ArrowLeft", false, false, false).as_deref(),
        Some("<Left>")
    );
    assert_eq!(k("Delete", true, false, false).as_deref(), Some("<C-Del>"));
    assert_eq!(k("Space", false, false, false).as_deref(), Some("<Space>"));
    assert_eq!(k(" ", false, false, false).as_deref(), Some("<Space>"));
    assert_eq!(k("F5", false, false, false).as_deref(), Some("<F5>"));
    assert_eq!(k("<", false, false, true).as_deref(), Some("<lt>"));
    assert_eq!(k("Shift", false, false, true), None);
    assert_eq!(k("Control", true, false, false), None);
}

#[test]
fn vim_insert_registers_arrows_and_counts() {
    // <C-r>{reg} inserts a register; an arrow key starts a new undo step and a new repeat.
    let (mut e, mut h) = vim("word");
    feed(&mut e, &mut h, "yiwA <C-r>\"<Esc>");
    assert_eq!(h.text(), "word word");
    let (mut e, mut h) = vim("");
    feed(&mut e, &mut h, "ifoo<Left>X<Esc>");
    assert_eq!(h.text(), "foXo");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "foo", "the arrow split the undo steps");
    feed(&mut e, &mut h, "0.");
    assert_eq!(h.text(), "Xfoo", ". repeats the insert after the arrow");

    let (mut e, mut h) = vim("x");
    feed(&mut e, &mut h, "2A!<Esc>");
    assert_eq!(h.text(), "x!!");
    feed(&mut e, &mut h, "3I-<Esc>");
    assert_eq!(h.text(), "---x!!");

    // cc and S keep the indent; s with a count changes that many chars.
    let (mut e, mut h) = vim("    foo bar\nnext");
    feed(&mut e, &mut h, "ccx<Esc>");
    assert_eq!(h.text(), "    x\nnext");
    feed(&mut e, &mut h, "jSy<Esc>");
    assert_eq!(h.text(), "    x\ny");
    feed(&mut e, &mut h, "k0w2sab<Esc>");
    assert_eq!(h.text(), "    ab\ny");
    feed(&mut e, &mut h, "j.");
    assert_eq!(h.text(), "    ab\nab", ". repeats with the same count");
}

#[test]
fn vim_visual_change_put_and_repeat() {
    let (mut e, mut h) = vim("abc abc abc");
    feed(&mut e, &mut h, "vecX<Esc>");
    assert_eq!(h.text(), "X abc abc");
    feed(&mut e, &mut h, "w.");
    assert_eq!(
        h.text(),
        "X X abc",
        "a Visual change repeats over as many chars"
    );

    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "yyjVp");
    assert_eq!(h.text(), "a\na\nc");
    assert_eq!(reg(&e, '"'), Some(("b\n".into(), true)));
    feed(&mut e, &mut h, "GVp");
    assert_eq!(h.text(), "a\na\nb");

    let (mut e, mut h) = vim("ab");
    feed(&mut e, &mut h, "ylgp");
    assert_eq!(h.text(), "aab");
    assert_eq!(h.cursor(), (0, 2), "gp leaves the cursor after the text");
    feed(&mut e, &mut h, "vr-");
    assert_eq!(h.text(), "aa-");

    // Visual-block operators act on the block (more in tests/extensions.rs).
    let (mut e, mut h) = vim("abc\ndef");
    feed(&mut e, &mut h, "<C-v>jl");
    assert_eq!(e.state().mode, Mode::VisualBlock);
    feed(&mut e, &mut h, "d");
    assert_eq!(h.text(), "c\nf");
    assert_eq!(e.state().mode, Mode::Normal);
}

#[test]
fn vim_macro_with_search_and_put_register() {
    let (mut e, mut h) = vim("x a x b x c");
    feed(&mut e, &mut h, "qa/");
    answer(&mut e, &mut h, "x");
    feed(&mut e, &mut h, "rYq");
    assert_eq!(h.text(), "x a Y b x c");
    feed(&mut e, &mut h, "@a");
    assert_eq!(h.text(), "x a Y b Y c");
    assert_eq!(
        h.prompts,
        vec!['/'],
        "replayed search keys stay inside the engine"
    );
    // The macro is ordinary register text.
    feed(&mut e, &mut h, "o<Esc>\"ap");
    assert_eq!(h.text(), "x a Y b Y c\n/x\rrY");

    // Operators with backwards and inclusive motions.
    let (mut e, mut h) = vim("one two three");
    h.set_cursor(0, 9);
    feed(&mut e, &mut h, "dge");
    assert_eq!(h.text(), "one twree", "dge includes the cursor char");
    feed(&mut e, &mut h, "db");
    assert_eq!(h.text(), "one ree");
    feed(&mut e, &mut h, "$dF ");
    assert_eq!(h.text(), "onee");
    feed(&mut e, &mut h, "0dT ");
    assert_eq!(h.text(), "onee", "T to nothing fails");
}

#[test]
fn vim_inner_block_keeps_opening_line() {
    let (mut e, mut h) = vim("let o = { a: 1,\n  b: 2,\n};");
    h.set_cursor(1, 2);
    feed(&mut e, &mut h, "di{");
    assert_eq!(
        h.text(),
        "let o = {\n};",
        "text after {{ stays outside, the opening line stays"
    );
    let (mut e, mut h) = vim("f(a,\n  b\n)");
    h.set_cursor(1, 2);
    feed(&mut e, &mut h, "ci(x<Esc>");
    assert_eq!(h.text(), "f(x\n)");
    // A brace that ends its line still makes the inner lines linewise.
    let (mut e, mut h) = vim("if x {\n  a;\n}");
    h.set_cursor(1, 2);
    feed(&mut e, &mut h, "di{");
    assert_eq!(h.text(), "if x {\n}");
}

#[test]
fn vim_empty_line_operators() {
    for keys in ["s", "cl", "ciw"] {
        let (mut e, mut h) = vim("ab\n\ncd");
        h.set_cursor(1, 0);
        feed(&mut e, &mut h, keys);
        assert_eq!(e.state().mode, Mode::Insert, "{keys} on an empty line");
        feed(&mut e, &mut h, "x<Esc>");
        assert_eq!(h.text(), "ab\nx\ncd", "{keys}");
    }
    for keys in ["x", "dl", "diw"] {
        let (mut e, mut h) = vim("ab\n\ncd");
        h.set_cursor(1, 0);
        feed(&mut e, &mut h, keys);
        assert_eq!(h.text(), "ab\n\ncd", "{keys} on an empty line does nothing");
        assert_eq!(e.state().mode, Mode::Normal);
    }
}

#[test]
fn vim_macro_survives_empty_line() {
    let (mut e, mut h) = vim("ab\n\ncd\nef\ngh");
    feed(&mut e, &mut h, "qaxjq5@a");
    assert_eq!(h.text(), "b\n\nd\nf\nh");
}

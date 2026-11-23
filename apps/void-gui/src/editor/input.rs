//! Keys and text into an editor: through the Vim engine, or plain editing when Vim is off.

use egui::{Event, Key, Modifiers};
use void_vim::{KeyLike, Mode, key_name};

use super::host::{Ctx, Host};
use super::state::{Doc, Msg};

/// The Vim name of an egui key event (`"a"`, `"A"`, `"<C-v>"`, `"<Esc>"`). eframe turns Ctrl+V,
/// Ctrl+C and Ctrl+X into clipboard events, so those map back to the keys. `None` for
/// releases and keys Vim has no name for.
pub fn vim_key(ev: &Event) -> Option<String> {
    match ev {
        Event::Paste(_) => Some("<C-v>".into()),
        Event::Copy => Some("<C-c>".into()),
        Event::Cut => Some("<C-x>".into()),
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_name(KeyLike {
            key: key_str(*key),
            ctrl: modifiers.ctrl || modifiers.mac_cmd,
            alt: modifiers.alt,
            shift: modifiers.shift,
        }),
        _ => None,
    }
}

/// egui's name for a key in the form `key_name` expects: the character for printable keys,
/// the name for the rest.
fn key_str(key: Key) -> &'static str {
    match key {
        Key::Minus => "-",
        Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp => key.name(),
        k => k.symbol_or_name(),
    }
}

/// Keys that also arrive as a `Text` event (letters, digits, punctuation, space) when no
/// Ctrl/Alt is held; the text event is the one to use (it respects the keyboard layout).
fn produces_text(key: Key, m: Modifiers) -> bool {
    !m.ctrl && !m.alt && !m.mac_cmd && (key == Key::Space || key_str(key).chars().count() == 1)
}

fn ctrl_shift(m: Modifiers, key: Key, want: Key) -> bool {
    m.ctrl && m.shift && key == want
}

/// Feed one frame's events to the doc. Returns whether anything was handled.
pub fn handle(events: &[Event], doc: &mut Doc, cx: &mut Ctx, vim: bool) -> bool {
    let mut any = false;
    for ev in events {
        any |= if vim {
            vim_event(ev, doc, cx)
        } else {
            plain_event(ev, doc, cx)
        };
    }
    any
}

fn run_key(doc: &mut Doc, cx: &mut Ctx, key: &str) -> bool {
    doc.core.message = None;
    let mut host = Host {
        core: &mut doc.core,
        cx,
    };
    doc.vim.key(&mut host, key)
}

fn run_keys(doc: &mut Doc, cx: &mut Ctx, keys: &[&str]) {
    for k in keys {
        run_key(doc, cx, k);
    }
}

fn typed(doc: &mut Doc, cx: &mut Ctx, text: &str) {
    doc.core.message = None;
    let mut host = Host {
        core: &mut doc.core,
        cx,
    };
    doc.vim.typed(&mut host, text);
}

fn answer(doc: &mut Doc, cx: &mut Ctx, line: Option<String>) {
    doc.core.prompt = None;
    doc.core.message = None;
    let mut host = Host {
        core: &mut doc.core,
        cx,
    };
    doc.vim.prompt_answer(&mut host, line);
}

fn inserting(doc: &Doc) -> bool {
    matches!(doc.mode(), Mode::Insert | Mode::Replace)
}

fn vim_event(ev: &Event, doc: &mut Doc, cx: &mut Ctx) -> bool {
    match ev {
        Event::Text(t) => vim_text(doc, cx, t),
        Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            if doc.core.prompt.is_some() {
                prompt_key(doc, cx, *key, *modifiers);
            } else if ctrl_shift(*modifiers, *key, Key::C) {
                app_copy(doc, cx);
            } else if ctrl_shift(*modifiers, *key, Key::V) {
                app_paste(doc, cx);
            } else if produces_text(*key, *modifiers) {
                return false;
            } else if let Some(name) = vim_key(ev) {
                run_key(doc, cx, &name);
            } else {
                return false;
            }
        }
        Event::Paste(text) if doc.core.prompt.is_some() => {
            if let Some(p) = doc.core.prompt.as_mut() {
                p.text.push_str(text.lines().next().unwrap_or(""));
            }
        }
        Event::Copy if doc.core.prompt.is_some() => answer(doc, cx, None),
        Event::Copy | Event::Cut | Event::Paste(_) => {
            if let Some(name) = vim_key(ev) {
                run_key(doc, cx, &name);
            }
        }
        _ => return false,
    }
    true
}

/// Typed text: in Insert mode it goes in as it is; otherwise each character is a key, and once
/// a key opens Insert mode or a prompt the rest goes there.
fn vim_text(doc: &mut Doc, cx: &mut Ctx, text: &str) {
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(p) = doc.core.prompt.as_mut() {
            p.text.push_str(rest);
            return;
        }
        if inserting(doc) {
            typed(doc, cx, rest);
            return;
        }
        rest = &rest[c.len_utf8()..];
        let key = match c {
            '\n' => "<CR>".to_string(),
            '\t' => "<Tab>".to_string(),
            c => c.to_string(),
        };
        run_key(doc, cx, &key);
    }
}

/// Editing keys of the `:` / `/` line.
fn prompt_key(doc: &mut Doc, cx: &mut Ctx, key: Key, m: Modifiers) {
    let Some(p) = doc.core.prompt.as_mut() else {
        return;
    };
    match key {
        Key::Enter => {
            let line = p.text.clone();
            answer(doc, cx, Some(line));
        }
        Key::Escape => answer(doc, cx, None),
        Key::C | Key::OpenBracket if m.ctrl => answer(doc, cx, None),
        Key::Backspace => {
            if p.text.pop().is_none() {
                answer(doc, cx, None);
            }
        }
        Key::U if m.ctrl => p.text.clear(),
        Key::V if m.ctrl && m.shift => {
            if let Some(t) = cx.clipboard.get() {
                p.text.push_str(t.lines().next().unwrap_or(""));
            }
        }
        _ => {}
    }
}

/// Ctrl+Shift+C: copy the Visual selection to the system clipboard.
fn app_copy(doc: &mut Doc, cx: &mut Ctx) {
    if doc.mode().is_visual() {
        run_keys(doc, cx, &["\"", "+", "y"]);
    } else {
        doc.core.message = Some(Msg::info("Select text (v, V or Ctrl+V) to copy it"));
    }
}

/// Ctrl+Shift+V: paste the system clipboard (typed in Insert mode, `"+P` / `"+p` otherwise).
fn app_paste(doc: &mut Doc, cx: &mut Ctx) {
    if inserting(doc) {
        if let Some(t) = cx.clipboard.get() {
            typed(doc, cx, &t);
        }
    } else if doc.mode().is_visual() {
        run_keys(doc, cx, &["\"", "+", "p"]);
    } else {
        run_keys(doc, cx, &["\"", "+", "P"]);
    }
}

// ---- Plain editing (Vim off) ----

fn plain_event(ev: &Event, doc: &mut Doc, cx: &mut Ctx) -> bool {
    match ev {
        Event::Text(t) => plain_insert(doc, t, true),
        Event::Paste(t) => plain_insert(doc, t, false),
        Event::Copy => {
            if let Some(s) = selected_text(doc) {
                cx.clipboard.set(&s);
            }
        }
        Event::Cut => {
            if let Some(s) = selected_text(doc) {
                cx.clipboard.set(&s);
                plain_insert(doc, "", false);
            }
        }
        Event::Key {
            key,
            pressed: true,
            modifiers: m,
            ..
        } => return plain_key(doc, cx, *key, *m),
        _ => return false,
    }
    true
}

fn selection(doc: &Doc) -> Option<std::ops::Range<usize>> {
    let (a, h) = doc.core.sel;
    let len = doc.core.len();
    (a != h).then(|| a.min(h).min(len)..a.max(h).min(len))
}

fn selected_text(doc: &Doc) -> Option<String> {
    selection(doc).map(|r| doc.core.buffer.slice(r))
}

fn end_typing(doc: &mut Doc) {
    if doc.typing {
        let head = doc.core.head();
        doc.core.buffer.end_group(head);
        doc.typing = false;
    }
}

/// Replace the selection (or insert at the cursor). Typing runs merge into one undo step.
fn plain_insert(doc: &mut Doc, text: &str, typing: bool) {
    if !typing {
        end_typing(doc);
    }
    let range = selection(doc).unwrap_or(doc.core.head()..doc.core.head());
    if !typing || !doc.typing {
        doc.core.buffer.begin_group(doc.core.head());
        doc.typing = typing;
    }
    let e = doc.core.replace(range.clone(), text);
    doc.vim.adjust_marks(e.start, e.old_len, e.new_len);
    let p = range.start + e.new_len;
    doc.core.sel = (p, p);
    if !typing {
        doc.core.buffer.end_group(p);
    }
}

fn plain_move(doc: &mut Doc, to: usize, extend: bool) {
    end_typing(doc);
    let to = to.min(doc.core.len());
    doc.core.sel = if extend {
        (doc.core.sel.0, to)
    } else {
        (to, to)
    };
}

fn word_left(doc: &Doc, from: usize) -> usize {
    let r = doc.core.buffer.rope();
    let mut i = from;
    while i > 0 && r.char(i - 1).is_whitespace() {
        i -= 1;
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if i > 0 && word(r.char(i - 1)) {
        while i > 0 && word(r.char(i - 1)) {
            i -= 1;
        }
    } else {
        i = i.saturating_sub(1);
    }
    i
}

fn word_right(doc: &Doc, from: usize) -> usize {
    let r = doc.core.buffer.rope();
    let len = r.len_chars();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = from;
    if i < len && word(r.char(i)) {
        while i < len && word(r.char(i)) {
            i += 1;
        }
    } else if i < len {
        i += 1;
    }
    while i < len && r.char(i).is_whitespace() && r.char(i) != '\n' {
        i += 1;
    }
    i
}

/// The same char column on `line` (clamped to its length).
fn at_column(doc: &Doc, line: usize, col: usize) -> usize {
    let b = &doc.core.buffer;
    let line = line.min(b.line_count().saturating_sub(1));
    b.line_start(line) + col.min(b.line_len(line))
}

fn plain_key(doc: &mut Doc, cx: &mut Ctx, key: Key, m: Modifiers) -> bool {
    let head = doc.core.head();
    let (line, col) = doc.core.line_col();
    let ext = m.shift;
    let ctrl = m.ctrl || m.mac_cmd;
    let rows = doc.core.view.rows();
    let b = &doc.core.buffer;
    match key {
        Key::ArrowLeft => {
            let to = match selection(doc) {
                Some(r) if !ext => r.start,
                _ if ctrl => word_left(doc, head),
                _ => head.saturating_sub(1),
            };
            plain_move(doc, to, ext);
        }
        Key::ArrowRight => {
            let to = match selection(doc) {
                Some(r) if !ext => r.end,
                _ if ctrl => word_right(doc, head),
                _ => head + 1,
            };
            plain_move(doc, to, ext);
        }
        Key::ArrowUp if line > 0 => plain_move(doc, at_column(doc, line - 1, col), ext),
        Key::ArrowUp => plain_move(doc, 0, ext),
        Key::ArrowDown => {
            let to = if line + 1 < b.line_count() {
                at_column(doc, line + 1, col)
            } else {
                doc.core.len()
            };
            plain_move(doc, to, ext);
        }
        Key::PageUp => plain_move(doc, at_column(doc, line.saturating_sub(rows), col), ext),
        Key::PageDown => plain_move(doc, at_column(doc, line + rows, col), ext),
        Key::Home if ctrl => plain_move(doc, 0, ext),
        Key::End if ctrl => plain_move(doc, doc.core.len(), ext),
        Key::Home => {
            let start = b.line_start(line);
            let blank = b
                .rope()
                .line(line)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count()
                .min(b.line_len(line));
            let to = if col == blank { start } else { start + blank };
            plain_move(doc, to, ext);
        }
        Key::End => plain_move(doc, b.line_start(line) + b.line_len(line), ext),
        Key::Backspace | Key::Delete => {
            if selection(doc).is_none() {
                let range = if key == Key::Backspace {
                    let from = if ctrl {
                        word_left(doc, head)
                    } else {
                        head.saturating_sub(1)
                    };
                    from..head
                } else {
                    let to = if ctrl {
                        word_right(doc, head)
                    } else {
                        head + 1
                    };
                    head..to.min(doc.core.len())
                };
                if range.is_empty() {
                    return true;
                }
                doc.core.sel = (range.start, range.end);
            }
            plain_insert(doc, "", false);
        }
        Key::Enter => {
            let at = selection(doc).map_or(head, |r| r.start);
            let text = doc.core.newline_text(at);
            plain_insert(doc, &text, false);
        }
        Key::Tab if !ctrl => {
            let unit = doc.core.indent.clone();
            plain_insert(doc, &unit, false);
        }
        Key::Escape => plain_move(doc, head, false),
        Key::Z if ctrl && !m.shift => plain_history(doc, false),
        Key::Y if ctrl => plain_history(doc, true),
        Key::Z if ctrl && m.shift => plain_history(doc, true),
        Key::A if ctrl => {
            end_typing(doc);
            doc.core.sel = (0, doc.core.len());
        }
        Key::C if ctrl && m.shift => {
            if let Some(s) = selected_text(doc) {
                cx.clipboard.set(&s);
            }
        }
        Key::V if ctrl && m.shift => {
            if let Some(t) = cx.clipboard.get() {
                plain_insert(doc, &t, false);
            }
        }
        _ => return false,
    }
    true
}

/// Ctrl+Z / Ctrl+Y with Vim off. Marks follow every edit the history makes.
fn plain_history(doc: &mut Doc, redo: bool) {
    end_typing(doc);
    let done = if redo {
        doc.core.redo()
    } else {
        doc.core.undo()
    };
    if let Some((cursor, edits)) = done {
        for e in &edits {
            doc.vim.adjust_marks(e.start, e.old_len, e.new_len);
        }
        doc.core.sel = (cursor, cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{Clipboard, EditorAction};
    use crate::layout::EditorView;
    use std::path::Path;

    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn run(doc: &mut Doc, vim: bool, events: &[Event]) -> Vec<EditorAction> {
        let clip = Clipboard::memory();
        let mut actions = Vec::new();
        let mut cx = Ctx {
            path: Path::new("/nonexistent/x.txt"),
            view: &EditorView::Disk,
            tab: 1,
            clipboard: &clip,
            actions: &mut actions,
        };
        handle(events, doc, &mut cx, vim);
        actions
    }

    fn doc(text: &str) -> Doc {
        Doc::new(Path::new("x.txt"), void_editor::Buffer::new(text))
    }

    #[test]
    fn plain_typing_is_one_undo_step() {
        let mut d = doc("ab");
        let none = Modifiers::NONE;
        run(
            &mut d,
            false,
            &[
                press(Key::ArrowRight, none),
                Event::Text("x".into()),
                Event::Text("y".into()),
                press(Key::Enter, none),
                press(Key::ArrowLeft, Modifiers::SHIFT),
                press(Key::Backspace, none),
            ],
        );
        assert_eq!(d.text(), "axyb");
        run(&mut d, false, &[press(Key::Z, Modifiers::CTRL)]);
        assert_eq!(d.text(), "axy\nb");
        run(&mut d, false, &[press(Key::Z, Modifiers::CTRL)]);
        assert_eq!(d.text(), "axyb");
        run(&mut d, false, &[press(Key::Z, Modifiers::CTRL)]);
        assert_eq!(d.text(), "ab");
        run(&mut d, false, &[press(Key::Y, Modifiers::CTRL)]);
        assert_eq!(d.text(), "axyb");
    }

    #[test]
    fn prompt_collects_a_line_in_one_frame() {
        let mut d = doc("one\ntwo\nthree");
        let actions = run(
            &mut d,
            true,
            &[
                Event::Text(":".into()),
                Event::Text("3".into()),
                press(Key::Enter, Modifiers::NONE),
                Event::Text(":".into()),
                Event::Text("qx".into()),
                press(Key::Backspace, Modifiers::NONE),
                press(Key::Enter, Modifiers::NONE),
            ],
        );
        assert_eq!(d.core.line_col(), (2, 0));
        assert_eq!(
            actions,
            vec![EditorAction::Close {
                tab: 1,
                force: false
            }]
        );
        assert!(d.core.prompt.is_none());
    }

    #[test]
    fn ctrl_keys_go_to_vim_and_shift_c_v_to_the_clipboard() {
        let mut d = doc("abc\ndef");
        let clip = Clipboard::memory();
        let mut actions = Vec::new();
        let mut cx = Ctx {
            path: Path::new("/nonexistent/x.txt"),
            view: &EditorView::Disk,
            tab: 1,
            clipboard: &clip,
            actions: &mut actions,
        };
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        handle(
            &[
                Event::Paste("ignored".into()),
                Event::Text("j".into()),
                press(Key::C, ctrl_shift),
            ],
            &mut d,
            &mut cx,
            true,
        );
        assert_eq!(clip.get().as_deref(), Some("a\nd"), "<C-v>j block, copied");
        handle(&[press(Key::V, ctrl_shift)], &mut d, &mut cx, true);
        assert_eq!(d.mode(), Mode::Normal);
        assert!(d.text().starts_with("a\nd"), "{:?}", d.text());
        assert!(d.text().len() > "abc\ndef".len(), "pasted");
    }
}

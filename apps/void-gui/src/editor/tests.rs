//! Editor tabs driven headlessly through the real app (egui_kittest, offline).

use std::path::Path;
use std::time::{Duration, Instant};

use egui::accesskit::Role;
use egui::{Event, Key, Modifiers};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::draw::relative_line_numbers;
use super::input::{route_clipboard_keys, vim_key};
use super::paint::accessible_text;
use crate::app::VoidApp;
use crate::layout::{EditorView, Tab};

fn harness(root: &Path) -> Harness<'static, VoidApp> {
    let (cfg, data) = (root.join("config.toml"), root.join("data"));
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, None, cfg, data));
    // The engine opens Scratch and the app applies its layout; editor tabs go in after that.
    until(&mut h, "workspace", |a| a.state.workspace.name == "Scratch");
    h.step();
    h
}

fn until(h: &mut Harness<'_, VoidApp>, what: &str, done: impl Fn(&VoidApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.step();
        if done(h.state()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the editor widget currently shows (its visible lines).
fn shown(h: &Harness<'_, VoidApp>) -> String {
    h.get_by_role(Role::MultilineTextInput)
        .value()
        .unwrap_or_default()
}

fn editor_tabs(app: &VoidApp) -> Vec<u32> {
    app.dock
        .iter_all_tabs()
        .filter_map(|(_, t)| match t {
            Tab::Editor { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

/// Type `s` and let two frames pass (tab titles show what the frame before them did).
fn text(h: &mut Harness<'_, VoidApp>, s: &str) {
    h.event(Event::Text(s.into()));
    h.step();
    h.step();
}

fn close(h: &mut Harness<'_, VoidApp>, id: u32) {
    h.state_mut().close_editor_tab(id);
    h.step();
    h.step();
}

#[test]
fn editor_opens_file_and_shows_text() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rs");
    std::fs::write(&file, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    assert_eq!(editor_tabs(h.state()).len(), 1);
    assert!(shown(&h).contains("println!(\"hi\");"), "{:?}", shown(&h));
    // Tab title and status line.
    assert!(h.query_by_label_contains("main.rs").is_some());
    assert!(h.query_by_label("NORMAL").is_some());
    assert!(h.query_by_label("1:1").is_some());
    assert!(h.query_by_label("Rust").is_some());
    assert!(h.query_by_label("LF").is_some());
    // Opening the same file again focuses the tab instead of adding one.
    h.state_mut().open_editor(file, EditorView::Disk, Some(2));
    h.step();
    assert_eq!(editor_tabs(h.state()).len(), 1);
    assert!(h.query_by_label("2:5").is_some(), "line 2, first non-blank");
}

#[test]
fn editor_vim_edit_and_save() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "world\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "ihello ");
    assert!(h.query_by_label("INSERT").is_some());
    h.key_press(Key::Escape);
    h.step();
    assert!(h.query_by_label("NORMAL").is_some());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "world\n",
        "nothing written before :w"
    );
    assert!(
        h.query_by_label_contains("notes.txt ●").is_some(),
        "dirty dot"
    );
    text(&mut h, ":");
    text(&mut h, "w");
    h.key_press(Key::Enter);
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world\n");
    assert!(h.query_by_label_contains("notes.txt ●").is_none());
    assert!(h.query_by_label_contains("written").is_some());
    // Undo goes through Vim: `u` restores the text (the buffer is dirty again).
    text(&mut h, "u");
    assert!(shown(&h).starts_with("world"), "{:?}", shown(&h));
}

#[test]
fn editor_two_tabs_share_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    let second = h
        .state_mut()
        .open_editor_tab(file.clone(), EditorView::Disk);
    h.step();
    h.step();
    let tabs = editor_tabs(h.state());
    assert_eq!(tabs.len(), 2);
    assert!(tabs.contains(&second));
    assert_eq!(h.state().editors.len(), 1, "one buffer for both tabs");
    text(&mut h, "dd");
    assert!(shown(&h).starts_with("two"), "{:?}", shown(&h));
    // Both titles carry the dirty dot; the other tab shows the same text.
    assert_eq!(h.query_all_by_label_contains("a.txt ●").count(), 2);
    let first = tabs.into_iter().find(|t| *t != second).unwrap();
    h.state_mut().focus_editor_tab(first);
    h.step();
    h.step();
    assert!(shown(&h).starts_with("two"), "{:?}", shown(&h));
    // Closing one of two tabs on a dirty buffer doesn't ask: the other still holds it.
    close(&mut h, first);
    assert!(h.query_by_label_contains("Save changes").is_none());
    assert_eq!(editor_tabs(h.state()), vec![second]);
}

#[test]
fn editor_dirty_close_asks() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("b.txt");
    std::fs::write(&file, "keep\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "x");
    let id = editor_tabs(h.state())[0];

    // Cancel keeps the tab and the edit.
    close(&mut h, id);
    assert!(h.query_by_label_contains("Save changes to b.txt").is_some());
    h.get_by_label("Cancel").click();
    h.step();
    h.step();
    assert_eq!(editor_tabs(h.state()), vec![id]);
    assert!(h.query_by_label_contains("Save changes").is_none());

    // Discard closes without writing.
    close(&mut h, id);
    h.get_by_label("Discard").click();
    h.step();
    h.step();
    assert!(editor_tabs(h.state()).is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep\n");
    assert_eq!(h.state().editors.len(), 0, "discarded buffer dropped");

    // Save writes, then closes.
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "x");
    let id = editor_tabs(h.state())[0];
    close(&mut h, id);
    h.get_by_label("Save").click();
    h.step();
    h.step();
    assert!(editor_tabs(h.state()).is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "eep\n");

    // A clean tab closes straight away.
    h.state_mut().open_editor(file, EditorView::Disk, None);
    h.step();
    let id = editor_tabs(h.state())[0];
    close(&mut h, id);
    assert!(editor_tabs(h.state()).is_empty());
}

#[test]
fn editor_big_file_first_frame_fast() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.rs");
    let mut src = String::with_capacity(4_000_000);
    for i in 0..100_000 {
        src.push_str(&format!(
            "fn f{i}(x: u32) -> u32 {{ x + {i} }} // line {i}\n"
        ));
    }
    std::fs::write(&file, &src).unwrap();
    let mut h = harness(dir.path());
    let t = Instant::now();
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    let first = t.elapsed();
    let bar = if cfg!(debug_assertions) {
        Duration::from_millis(1500)
    } else {
        Duration::from_millis(200)
    };
    println!("100 000-line file: open + first frame in {first:?} (bar {bar:?})");
    assert!(shown(&h).contains("fn f0(x: u32)"));
    assert!(first < bar, "first frame took {first:?}");
    // Highlighting follows from the background parse.
    until(&mut h, "background parse", |a| {
        a.editors
            .doc(&(file.clone(), EditorView::Disk))
            .is_some_and(|d| d.highlighted())
    });
    // Jumping to the end stays quick too.
    let t = Instant::now();
    text(&mut h, "G");
    println!("G on 100 000 lines: {:?}", t.elapsed());
    assert!(shown(&h).contains("line 99999"));
}

#[test]
fn editor_restore_tolerates_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(dir.path().join("gone.rs"), EditorView::Disk, None);
    h.step();
    h.step();
    assert!(h.query_by_label_contains("Couldn't open").is_some());
    // The tab survives a layout round trip.
    let text = crate::layout::to_text(&h.state().dock);
    assert!(crate::layout::from_text(&text).is_some(), "{text}");
}

#[test]
fn relative_numbers_count_from_the_cursor() {
    assert_eq!(relative_line_numbers(5, 3..8), vec![2, 1, 6, 1, 2]);
    assert_eq!(relative_line_numbers(0, 0..3), vec![1, 1, 2]);
    assert_eq!(relative_line_numbers(9, 0..2), vec![9, 8]);
}

#[test]
fn key_name_from_egui_events() {
    let key = |key: Key, modifiers: Modifiers| Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    };
    assert_eq!(
        vim_key(&key(Key::V, Modifiers::CTRL)).as_deref(),
        Some("<C-v>")
    );
    // eframe turns Ctrl+V, Ctrl+C and Ctrl+X into clipboard events.
    assert_eq!(vim_key(&Event::Paste("x".into())).as_deref(), Some("<C-v>"));
    assert_eq!(vim_key(&Event::Copy).as_deref(), Some("<C-c>"));
    assert_eq!(vim_key(&Event::Cut).as_deref(), Some("<C-x>"));
    // An image on the clipboard still means Ctrl+V.
    let image = std::sync::Arc::new(egui::ColorImage::filled([1, 1], egui::Color32::BLACK));
    assert_eq!(vim_key(&Event::PasteImage(image)).as_deref(), Some("<C-v>"));
    assert_eq!(
        vim_key(&key(Key::A, Modifiers::SHIFT)).as_deref(),
        Some("A")
    );
    assert_eq!(vim_key(&key(Key::A, Modifiers::NONE)).as_deref(), Some("a"));
    assert_eq!(
        vim_key(&key(Key::Escape, Modifiers::NONE)).as_deref(),
        Some("<Esc>")
    );
    assert_eq!(
        vim_key(&key(Key::OpenBracket, Modifiers::CTRL)).as_deref(),
        Some("<C-[>")
    );
    assert_eq!(
        vim_key(&key(Key::ArrowLeft, Modifiers::NONE)).as_deref(),
        Some("<Left>")
    );
    assert_eq!(
        vim_key(&key(Key::Minus, Modifiers::CTRL)).as_deref(),
        Some("<C-->")
    );
    let released = Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: Modifiers::NONE,
    };
    assert_eq!(vim_key(&released), None);
}

#[test]
fn editor_quit_with_unsaved_asks() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("q.txt");
    std::fs::write(&file, "abc\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "x");
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    h.step();
    assert!(
        h.query_by_label_contains("Save changes before quitting")
            .is_some()
    );
    assert!(h.query_by_label_contains("Unsaved: q.txt").is_some());
    h.get_by_label("Save all").click();
    h.step();
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "bc\n");
    assert!(
        h.query_by_label_contains("Save changes before quitting")
            .is_none()
    );
}

fn ctrl(key: Key) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::CTRL,
    }
}

/// eframe's own clipboard shortcuts are off (they swallow Ctrl+V when the clipboard is empty):
/// in a focused editor Ctrl+C/V/X stay keys for Vim; anywhere else they become clipboard events.
#[test]
fn clipboard_keys_route_to_editor_or_widgets() {
    let clip = super::Clipboard::memory();
    let mut events = vec![ctrl(Key::V), ctrl(Key::C), ctrl(Key::X)];
    route_clipboard_keys(&mut events, true, &clip);
    assert_eq!(events, vec![ctrl(Key::V), ctrl(Key::C), ctrl(Key::X)]);

    clip.set("pasted\r\ntext");
    let mut events = vec![
        ctrl(Key::V),
        ctrl(Key::C),
        ctrl(Key::X),
        Event::Text("a".into()),
    ];
    route_clipboard_keys(&mut events, false, &clip);
    assert_eq!(
        events,
        vec![
            Event::Paste("pasted\ntext".into()),
            Event::Copy,
            Event::Cut,
            Event::Text("a".into())
        ]
    );
    // An empty clipboard pastes nothing outside the editor.
    let clip = super::Clipboard::memory();
    let mut events = vec![ctrl(Key::V)];
    route_clipboard_keys(&mut events, false, &clip);
    assert!(events.is_empty());
    // Ctrl+Shift+V is not a paste shortcut here.
    let shifted = Event::Key {
        key: Key::V,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::CTRL | Modifiers::SHIFT,
    };
    let mut events = vec![shifted.clone()];
    route_clipboard_keys(&mut events, false, &clip);
    assert_eq!(events, vec![shifted]);
}

/// Through the app's input hook: raw Ctrl+V in a focused editor enters Visual-block mode even
/// with nothing on the clipboard; in the chat input it pastes.
#[test]
fn ctrl_v_reaches_vim_and_still_pastes_in_chat() {
    use eframe::App as _;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.txt");
    std::fs::write(&file, "abc\ndef\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    let hook = |h: &mut Harness<'_, VoidApp>| {
        let ctx = h.ctx.clone();
        let mut raw = h.input_mut().clone();
        h.state_mut().raw_input_hook(&ctx, &mut raw);
        *h.input_mut() = raw;
        h.step();
        h.step();
    };
    h.input_mut().events.push(ctrl(Key::V));
    hook(&mut h);
    assert!(h.query_by_label("V-BLOCK").is_some());
    h.key_press(Key::Escape);
    h.step();

    // Chat tab in front, its input focused, something on the clipboard.
    let chat = h
        .state()
        .dock
        .find_tab_from(|t| matches!(t, Tab::Chat { .. }))
        .unwrap();
    let _ = h.state_mut().dock.set_active_tab(chat);
    h.state_mut()
        .dock
        .set_focused_node_and_surface(chat.node_path());
    h.step();
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.state().editors.clipboard.set("pasted");
    h.input_mut().events.push(ctrl(Key::V));
    hook(&mut h);
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some("pasted")
    );
}

/// While the Save / Discard / Cancel dialog is up, keys don't edit the buffer behind it; Enter
/// picks Save and Esc Cancel.
#[test]
fn editor_ignores_keys_while_dialog_open() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("m.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "x");
    let id = editor_tabs(h.state())[0];
    let key = (file.clone(), EditorView::Disk);
    close(&mut h, id);
    assert!(h.query_by_label_contains("Save changes").is_some());
    text(&mut h, "x");
    text(&mut h, "dd");
    assert_eq!(
        h.state().editors.doc(&key).unwrap().text(),
        "ne\ntwo",
        "buffer unchanged behind the dialog"
    );
    h.key_press(Key::Escape);
    h.step();
    h.step();
    assert!(h.query_by_label_contains("Save changes").is_none());
    assert_eq!(editor_tabs(h.state()), vec![id]);
    close(&mut h, id);
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert!(editor_tabs(h.state()).is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "ne\ntwo\n");
}

#[test]
fn accessible_text_is_bounded() {
    let line: String = "x".repeat(1_000_000);
    let rope = ropey::Rope::from_str(&format!("{line}\nshort"));
    let text = accessible_text(&rope, [0, 1].into_iter());
    assert!(text.len() < 20_100, "{} bytes", text.len());
    assert!(text.ends_with("\nshort"));
}

fn command(key: Key) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    }
}

/// Run the app's input hook on the queued input (egui_kittest doesn't call it), then step.
fn hooked_step(h: &mut Harness<'_, VoidApp>) {
    use eframe::App as _;
    let ctx = h.ctx.clone();
    let mut raw = h.input_mut().clone();
    h.state_mut().raw_input_hook(&ctx, &mut raw);
    *h.input_mut() = raw;
    h.step();
    h.step();
}

/// One clipboard for the whole app: Ctrl+C in one text field pastes with Ctrl+V in another, and
/// the editor's `"+y` pastes into a text field too. (The system clipboard can't be used
/// headless; tests run on the app's in-memory clipboard, which every copy and paste goes
/// through.)
#[test]
fn copy_paste_round_trips_through_one_clipboard() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(dir.path());
    // Ctrl+C in the chat search field...
    h.get_by_role(Role::TextInput).focus();
    h.step();
    h.get_by_role(Role::TextInput).type_text("borrow");
    h.step();
    h.input_mut().events.push(command(Key::A));
    h.step();
    h.input_mut().events.push(command(Key::C));
    hooked_step(&mut h);
    assert_eq!(h.state().editors.clipboard.get().as_deref(), Some("borrow"));
    // ...Ctrl+V in the chat input.
    h.get_by_role(Role::MultilineTextInput).focus();
    h.step();
    h.input_mut().events.push(command(Key::V));
    hooked_step(&mut h);
    assert_eq!(
        h.get_by_role(Role::MultilineTextInput).value().as_deref(),
        Some("borrow")
    );

    // The editor's "+y, then Ctrl+V in the search field.
    let file = dir.path().join("y.txt");
    std::fs::write(&file, "yanked\n").unwrap();
    h.state_mut().open_editor(file, EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "\"+yiw");
    assert_eq!(h.state().editors.clipboard.get().as_deref(), Some("yanked"));
    let chat = h
        .state()
        .dock
        .find_tab_from(|t| matches!(t, Tab::Chat { .. }))
        .unwrap();
    let _ = h.state_mut().dock.set_active_tab(chat);
    h.step();
    h.get_by_role(Role::TextInput).focus();
    h.step();
    h.input_mut().events.push(command(Key::A));
    h.step();
    h.input_mut().events.push(command(Key::V));
    hooked_step(&mut h);
    assert_eq!(
        h.get_by_role(Role::TextInput).value().as_deref(),
        Some("yanked")
    );
}

/// A folder workspace (the watcher covers it) holding `a.txt`, opened in an editor tab once the
/// watcher is up.
fn watched(
    text: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Harness<'static, VoidApp>,
) {
    let t = tempfile::tempdir().unwrap();
    let ws = t.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir(&ws).unwrap();
    let file = ws.join("a.txt");
    std::fs::write(&file, text).unwrap();
    let (cfg, data) = (t.path().join("config.toml"), t.path().join("data"));
    let open = Some(ws.clone());
    let mut h = Harness::builder()
        .with_size([1200.0, 800.0])
        .with_max_steps(10)
        .with_accessibility_check(false)
        .build_eframe(move |cc| VoidApp::new(cc, open, cfg, data));
    let want = ws.clone();
    until(&mut h, "workspace", |a| {
        a.state.workspace.root.as_deref() == Some(want.as_path())
    });
    until(&mut h, "watcher", |a| a.disk.ready());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, Some(3));
    h.step();
    h.step();
    (t, file, h)
}

/// Wait until the watcher has delivered an event for a file written after everything before
/// it, so earlier events (own saves) have been handled too.
fn settle(h: &mut Harness<'_, VoidApp>, dir: &Path) {
    let sentinel = dir.join(format!("sentinel-{}", rand_suffix()));
    std::fs::write(&sentinel, "s").unwrap();
    until(h, "sentinel event", |a| a.disk.saw(&sentinel));
    h.step();
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn external_change_reloads_clean_buffer() {
    let (_t, file, mut h) = watched("one\ntwo\nthree\n");
    let key = (file.clone(), EditorView::Disk);
    let dir = file.parent().unwrap().to_path_buf();

    // Our own save is not an outside change: no banner, no reload.
    let doc = h.state_mut().editors.doc_mut(&key).unwrap();
    let end = doc.core.len();
    doc.core.replace(end..end, "!");
    h.state_mut().editors.save(&key).unwrap();
    settle(&mut h, &dir);
    assert!(h.query_by_label_contains("Changed on disk").is_none());
    assert_eq!(
        h.state().editors.doc(&key).unwrap().text(),
        "one\ntwo\nthree!"
    );
    assert_eq!(h.state().editors.doc(&key).unwrap().core.line_col().0, 2);

    // Someone else rewrites it: the clean buffer reloads and keeps the cursor line.
    std::fs::write(&file, "1\n2\n3\n4\n").unwrap();
    until(&mut h, "reloaded", |a| {
        a.editors
            .doc(&key)
            .is_some_and(|d| d.text() == "1\n2\n3\n4")
    });
    let doc = h.state().editors.doc(&key).unwrap();
    assert!(!doc.core.buffer.is_dirty());
    assert_eq!(doc.core.line_col().0, 2, "still on line 3");
    h.step();
    assert!(h.query_by_label_contains("Changed on disk").is_none());
    assert!(shown(&h).starts_with("1\n2\n3\n4"), "{:?}", shown(&h));
}

#[test]
fn external_change_flags_dirty_buffer() {
    let (_t, file, mut h) = watched("one\n");
    let key = (file.clone(), EditorView::Disk);
    h.state_mut()
        .editors
        .doc_mut(&key)
        .unwrap()
        .core
        .replace(0..0, "x");

    // A dirty buffer is never overwritten: it says so and offers Reload / Keep mine.
    std::fs::write(&file, "theirs\n").unwrap();
    until(&mut h, "banner", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_changed())
    });
    h.step();
    assert!(h.query_by_label_contains("Changed on disk").is_some());
    assert_eq!(h.state().editors.doc(&key).unwrap().text(), "xone");
    assert!(h.state().editors.is_dirty(&key));

    // Keep mine: the banner goes, the text and the file stay as they are.
    h.get_by_label("Keep mine").click();
    h.step();
    h.step();
    assert!(h.query_by_label_contains("Changed on disk").is_none());
    assert_eq!(h.state().editors.doc(&key).unwrap().text(), "xone");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");

    // Another outside change asks again; Reload takes the file.
    std::fs::write(&file, "newer\n").unwrap();
    until(&mut h, "banner again", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_changed())
    });
    h.step();
    h.get_by_label("Reload").click();
    h.step();
    h.step();
    let doc = h.state().editors.doc(&key).unwrap();
    assert_eq!(doc.text(), "newer");
    assert!(!doc.core.buffer.is_dirty());
    assert!(h.query_by_label_contains("Changed on disk").is_none());

    // Deleted on disk: the text stays, a banner says so, and saving puts the file back.
    std::fs::remove_file(&file).unwrap();
    until(&mut h, "deleted banner", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_deleted())
    });
    h.step();
    assert!(h.query_by_label_contains("Deleted on disk").is_some());
    assert_eq!(h.state().editors.doc(&key).unwrap().text(), "newer");
    // `:w`, through Vim.
    text(&mut h, ":");
    text(&mut h, "w");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "newer\n");
    assert!(h.query_by_label_contains("Deleted on disk").is_none());
}

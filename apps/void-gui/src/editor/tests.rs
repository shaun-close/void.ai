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

pub(super) fn until(h: &mut Harness<'_, VoidApp>, what: &str, done: impl Fn(&VoidApp) -> bool) {
    until_within(h, what, Duration::from_secs(10), done);
}

/// A full parse of a 100k-line file in a debug build on a loaded machine.
const PARSE_WAIT: Duration = Duration::from_secs(120);

fn until_within(
    h: &mut Harness<'_, VoidApp>,
    what: &str,
    limit: Duration,
    done: impl Fn(&VoidApp) -> bool,
) {
    let deadline = Instant::now() + limit;
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
pub(super) fn shown(h: &Harness<'_, VoidApp>) -> String {
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
pub(super) fn text(h: &mut Harness<'_, VoidApp>, s: &str) {
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

/// Opening a 100k-line file shows its first frame about as fast as a small file does, and
/// parses in the background. Timed against a small-file baseline in the same harness (a loaded
/// machine slows both alike); the parse being off the UI thread is checked without timing.
#[test]
fn editor_big_file_first_frame_fast() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, lines: usize| {
        let mut src = String::with_capacity(lines * 45);
        for i in 0..lines {
            src.push_str(&format!(
                "fn f{i}(x: u32) -> u32 {{ x + {i} }} // line {i}\n"
            ));
        }
        let file = dir.path().join(name);
        std::fs::write(&file, &src).unwrap();
        file
    };
    let small = write("small.rs", 1_000);
    let file = write("big.rs", 100_000);
    let mut h = harness(dir.path());
    let highlighted = |key: &std::path::PathBuf| {
        let key = (key.clone(), EditorView::Disk);
        move |a: &VoidApp| a.editors.doc(&key).is_some_and(|d| d.highlighted())
    };
    let open = |h: &mut Harness<'_, VoidApp>, f: &std::path::PathBuf| {
        let t = Instant::now();
        h.state_mut().open_editor(f.clone(), EditorView::Disk, None);
        h.step();
        t.elapsed()
    };
    // The baseline also pays the one-off costs (fonts, the Rust grammar).
    let base = open(&mut h, &small);
    until_within(&mut h, "small file parse", PARSE_WAIT, highlighted(&small));
    let first = open(&mut h, &file);
    // Unloaded (debug): ~70 ms against ~90 ms; a full parse on open would add ~1.3 s.
    let bar = base * 5
        + if cfg!(debug_assertions) {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(50)
        };
    println!("first frame: 100 000-line file {first:?}, 1 000-line file {base:?} (bar {bar:?})");
    assert!(shown(&h).contains("fn f0(x: u32)"));
    assert!(
        first < bar,
        "first frame took {first:?} (a 1 000-line file: {base:?})"
    );
    // A finished background parse is only installed by a later frame, so after one frame
    // the file can't be highlighted yet unless it was parsed on the UI thread.
    assert!(
        !highlighted(&file)(h.state()),
        "the 100 000-line file was parsed while opening it"
    );
    let t = Instant::now();
    until_within(&mut h, "background parse", PARSE_WAIT, highlighted(&file));
    println!("background parse: {:?} more", t.elapsed());
    // Jumping to the end. (The view, before and after, goes in the failure message: under a
    // loaded gate the view has been seen left at line ~83 200 with the cursor on the last line.)
    let key = (file.clone(), EditorView::Disk);
    let view = |h: &Harness<'_, VoidApp>| h.state().editors.doc(&key).map(|d| d.core.view.clone());
    let before = view(&h);
    let t = Instant::now();
    text(&mut h, "G");
    println!("G on 100 000 lines: {:?}", t.elapsed());
    let doc = h.state().editors.doc(&key).unwrap();
    assert!(
        shown(&h).contains("line 99999"),
        "G didn't show the last line: cursor {:?}, view before {before:?}, after {:?}, \
         first line shown {:?}",
        doc.core.line_col(),
        view(&h),
        shown(&h).lines().next()
    );
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

/// Saving checks the disk itself (no watcher event needed): a file changed since it was read
/// isn't overwritten; `:w!` or the banner's Overwrite does it on purpose.
#[test]
fn save_refuses_when_disk_changed_since_load() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s.txt");
    std::fs::write(&file, "one\n").unwrap();
    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    let key = (file.clone(), EditorView::Disk);
    h.state_mut()
        .editors
        .doc_mut(&key)
        .unwrap()
        .core
        .replace(0..0, "x");
    // Written behind our back, and saved before any watcher event can have arrived.
    std::fs::write(&file, "theirs\n").unwrap();
    assert!(h.state_mut().editors.save(&key).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs\n");
    assert!(h.state().editors.doc(&key).unwrap().disk_changed());
    h.step();
    h.step();
    assert!(h.query_by_label_contains("Changed on disk").is_some());
    assert!(
        h.query_by_label_contains("changed on disk since").is_some(),
        "says why"
    );

    // `:w!` overwrites on purpose.
    text(&mut h, ":");
    text(&mut h, "w!");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "xone\n");
    assert!(h.query_by_label_contains("Changed on disk").is_none());

    // So does the banner's Overwrite.
    std::fs::write(&file, "again\n").unwrap();
    h.state_mut()
        .editors
        .doc_mut(&key)
        .unwrap()
        .core
        .replace(0..0, "y");
    assert!(h.state_mut().editors.save(&key).is_err());
    h.step();
    h.step();
    h.get_by_label("Overwrite").click();
    h.step();
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "yxone\n");
    assert!(!h.state().editors.is_dirty(&key));
}

/// Closing the last tab on a buffer whose file was deleted asks first: the buffer is the only
/// copy left. Save puts the file back.
#[test]
fn closing_deleted_on_disk_buffer_asks() {
    let (_t, file, mut h) = watched("keep me\n");
    let key = (file.clone(), EditorView::Disk);
    std::fs::remove_file(&file).unwrap();
    until(&mut h, "deleted", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_deleted())
    });
    assert!(!h.state().editors.is_dirty(&key));
    let id = editor_tabs(h.state())[0];
    close(&mut h, id);
    assert_eq!(editor_tabs(h.state()), vec![id], "still open");
    assert!(h.query_by_label_contains("deleted on disk").is_some());
    h.get_by_label("Save").click();
    h.step();
    h.step();
    assert!(editor_tabs(h.state()).is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me\n");
}

/// `:qa` counts a clean buffer whose file was deleted on disk as unsaved (it holds the only
/// copy) and refuses; `:qa!` drops it.
#[test]
fn qa_keeps_deleted_on_disk_buffer() {
    let (_t, file, mut h) = watched("keep me\n");
    let key = (file.clone(), EditorView::Disk);
    std::fs::remove_file(&file).unwrap();
    until(&mut h, "deleted", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_deleted())
    });
    assert!(!h.state().editors.is_dirty(&key));
    let id = editor_tabs(h.state())[0];
    text(&mut h, ":qa");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert_eq!(editor_tabs(h.state()), vec![id], "still open");
    let msg = h.state().editors.doc(&key).unwrap().core.message.clone();
    assert!(format!("{msg:?}").contains("E162"), "{msg:?}");
    text(&mut h, ":qa!");
    h.key_press(Key::Enter);
    h.step();
    h.step();
    assert!(editor_tabs(h.state()).is_empty());
    assert!(!file.exists());
}

/// Quitting asks about a clean buffer whose file was deleted on disk; Save all puts it back.
#[test]
fn quit_asks_about_deleted_on_disk_buffer() {
    let (_t, file, mut h) = watched("keep me\n");
    let key = (file.clone(), EditorView::Disk);
    std::fs::remove_file(&file).unwrap();
    until(&mut h, "deleted", |a| {
        a.editors.doc(&key).is_some_and(|d| d.disk_deleted())
    });
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
    assert!(
        h.query_by_label_contains("a.txt (deleted on disk)")
            .is_some()
    );
    h.get_by_label("Save all").click();
    h.step();
    h.step();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me\n");
    assert!(
        h.query_by_label_contains("Save changes before quitting")
            .is_none()
    );
}

/// End to end on a real source file: search, change a word, write, undo, write again.
#[test]
fn e2e_editor_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("model.rs");
    let original = include_bytes!("../../../../crates/void-core/src/model.rs");
    std::fs::write(&file, original).unwrap();
    let before = String::from_utf8(original.to_vec()).unwrap();
    // `ciw` replaces the whole word around the first match (`ConversationId` here).
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let hit = before.find("Conversation").expect("a match");
    let start = before[..hit].rfind(|c| !word(c)).map_or(0, |n| n + 1);
    let end = hit
        + before[hit..]
            .find(|c| !word(c))
            .unwrap_or(before.len() - hit);
    let expected = format!("{}Thread{}", &before[..start], &before[end..]);

    let mut h = harness(dir.path());
    h.state_mut()
        .open_editor(file.clone(), EditorView::Disk, None);
    h.step();
    h.step();
    text(&mut h, "/Conversation");
    h.key_press(Key::Enter);
    h.step();
    text(&mut h, "ciwThread");
    h.key_press(Key::Escape);
    h.step();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "nothing written before :w"
    );
    text(&mut h, ":w");
    h.key_press(Key::Enter);
    h.step();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        expected.as_bytes(),
        "the first match changed, every other byte untouched"
    );

    text(&mut h, "u");
    text(&mut h, ":w");
    h.key_press(Key::Enter);
    h.step();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        original,
        "undo and :w restore the exact bytes"
    );
}

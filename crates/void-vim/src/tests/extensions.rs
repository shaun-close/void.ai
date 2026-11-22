//! Marks, the jump list, Visual-block operators, `<C-a>` / `<C-x>` and the host's ex commands.

use super::*;

#[test]
fn marks_survive_edits() {
    let (mut e, mut h) = vim("one\ntwo\n  three\nfour");
    h.set_cursor(2, 4);
    feed(&mut e, &mut h, "magg");
    feed(&mut e, &mut h, "'a");
    assert_eq!(
        h.cursor(),
        (2, 2),
        "' goes to the first non-blank of the mark's line"
    );
    feed(&mut e, &mut h, "gg`a");
    assert_eq!(h.cursor(), (2, 4), "` goes to the exact position");

    // Lines added above move the mark down with its line.
    feed(&mut e, &mut h, "ggOzero<Esc>");
    feed(&mut e, &mut h, "gg`a");
    assert_eq!(h.cursor(), (3, 4));
    // Text inserted before it on its line moves it along.
    feed(&mut e, &mut h, "0iXY<Esc>gg`a");
    assert_eq!(h.cursor(), (3, 6));
    assert_eq!(h.text(), "zero\none\ntwo\nXY  three\nfour");
    // A deleted line above moves it up; undo moves it back.
    feed(&mut e, &mut h, "ggddG`a");
    assert_eq!(h.cursor(), (2, 6));
    feed(&mut e, &mut h, "uG`a");
    assert_eq!(h.cursor(), (3, 6));
    // Edits after it leave it alone; edits from the host are reported with adjust_marks.
    feed(&mut e, &mut h, "Gdd`a");
    assert_eq!(h.cursor(), (3, 6));
    h.buf.replace(0..0, "ab\n");
    e.adjust_marks(0, 0, 3);
    feed(&mut e, &mut h, "gg`a");
    assert_eq!(h.cursor(), (4, 6));

    // Marks are operator targets: d'a deletes linewise, y`a exclusively.
    let (mut e, mut h) = vim("a\nb\nc\nd");
    feed(&mut e, &mut h, "jmajjd'a");
    assert_eq!(h.text(), "a");
    let (mut e, mut h) = vim("hello world");
    feed(&mut e, &mut h, "wmb0y`b");
    assert_eq!(reg(&e, '"'), Some(("hello ".into(), false)));

    // Deleting the mark's line deletes the mark: jumping to it fails and stops a macro.
    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "jmcgg");
    feed(&mut e, &mut h, "qq'cxq");
    assert_eq!(h.text(), "a\n\nc");
    feed(&mut e, &mut h, "dd");
    assert_eq!(h.text(), "a\nc");
    feed(&mut e, &mut h, "gg'c");
    assert_eq!(h.cursor(), (0, 0));
    assert!(e.state().message.contains("E20"), "{}", e.state().message);
    feed(&mut e, &mut h, "@q");
    assert_eq!(h.text(), "a\nc", "the macro stopped at the missing mark");

    // Marks work inside a macro, and over multi-byte text.
    let (mut e, mut h) = vim("héllo 🦀 wörld\nnext");
    feed(&mut e, &mut h, "fwmz0");
    feed(&mut e, &mut h, "qm`zx0q");
    assert_eq!(h.text(), "héllo 🦀 örld\nnext");
    feed(&mut e, &mut h, "@m");
    assert_eq!(h.text(), "héllo 🦀 rld\nnext");
}

#[test]
fn jumplist_back_forward() {
    let text: String = (0..200).map(|i| format!("line {i}\n")).collect();
    let (mut e, mut h) = vim(&text);
    feed(&mut e, &mut h, "G");
    assert_eq!(h.cursor().0, 199);
    feed(&mut e, &mut h, "3G");
    feed(&mut e, &mut h, "/");
    answer(&mut e, &mut h, "line 7");
    assert_eq!(h.cursor().0, 7);
    // Plain motions are not jumps.
    feed(&mut e, &mut h, "jk");

    let line = |e: &mut Engine, h: &mut Host, k: &str| {
        feed(e, h, k);
        h.cursor().0
    };
    assert_eq!(line(&mut e, &mut h, "<C-o>"), 2);
    assert_eq!(line(&mut e, &mut h, "<C-o>"), 199);
    assert_eq!(line(&mut e, &mut h, "<C-o>"), 0);
    assert_eq!(line(&mut e, &mut h, "<C-o>"), 0, "nothing older");
    assert_eq!(line(&mut e, &mut h, "<Tab>"), 199, "Tab is <C-i>");
    assert_eq!(line(&mut e, &mut h, "<C-i>"), 2);
    assert_eq!(line(&mut e, &mut h, "<C-i>"), 7);
    assert_eq!(line(&mut e, &mut h, "<C-i>"), 7, "nothing newer");
    assert_eq!(
        line(&mut e, &mut h, "2<C-o>"),
        199,
        "a count goes further back"
    );

    // n, N, %, H/M/L and marks are jumps too; '' and `` go back to before the last jump.
    let (mut e, mut h) = vim("a (x) b\nfoo\nbar\nfoo\nbaz");
    feed(&mut e, &mut h, "f(%");
    assert_eq!(h.cursor(), (0, 4));
    feed(&mut e, &mut h, "``");
    assert_eq!(h.cursor(), (0, 2));
    feed(&mut e, &mut h, "``");
    assert_eq!(h.cursor(), (0, 4));
    feed(&mut e, &mut h, "j*");
    assert_eq!(h.cursor(), (3, 0));
    feed(&mut e, &mut h, "''");
    assert_eq!(h.cursor(), (1, 0));
    feed(&mut e, &mut h, "nN");
    feed(&mut e, &mut h, "L");
    assert_eq!(h.cursor(), (4, 0));
    let back: Vec<usize> = (0..4).map(|_| line(&mut e, &mut h, "<C-o>")).collect();
    assert_eq!(
        back,
        [1, 3, 0, 0],
        "L, N, n and '' were recorded, each line once"
    );

    // At most 100 entries.
    let (mut e, mut h) = vim(&text);
    for n in 2..=151 {
        feed(&mut e, &mut h, &format!("{n}G"));
    }
    assert_eq!(h.cursor().0, 150);
    assert_eq!(line(&mut e, &mut h, "99<C-o>"), 51);
    assert_eq!(
        line(&mut e, &mut h, "<C-o>"),
        51,
        "older entries were dropped"
    );

    // A failing <C-o> stops a macro; jumps work when replayed.
    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "qqGx<C-o>q");
    assert_eq!(h.text(), "a\nb\n");
    assert_eq!(h.cursor().0, 0);
    feed(&mut e, &mut h, "j@q");
    assert_eq!(
        h.text(),
        "a\nb\n",
        "G then x on the empty last line, then back"
    );
    assert_eq!(h.cursor().0, 1);
}

#[test]
fn visual_block_insert_and_delete() {
    let (mut e, mut h) = vim("abcd\nefgh\nijkl");
    feed(&mut e, &mut h, "<C-v>");
    assert_eq!(e.state().mode, Mode::VisualBlock);
    feed(&mut e, &mut h, "jjIx<Esc>");
    assert_eq!(h.text(), "xabcd\nxefgh\nxijkl");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "abcd\nefgh\nijkl", "one undo step");

    feed(&mut e, &mut h, "gg0<C-v>jjld");
    assert_eq!(h.text(), "cd\ngh\nkl", "a 2×3 block");
    assert_eq!(reg(&e, '"').map(|r| r.0), Some("ab\nef\nij".into()));
    assert_eq!(h.cursor(), (0, 0));
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "abcd\nefgh\nijkl", "one undo step");

    // Columns are chars: multi-byte text stays whole; short lines are skipped by I.
    let (mut e, mut h) = vim("héllo\nx\nwörld");
    feed(&mut e, &mut h, "l<C-v>jjlI🦀<Esc>");
    assert_eq!(h.text(), "h🦀éllo\nx\nw🦀örld");
    feed(&mut e, &mut h, "u0l<C-v>jjld");
    assert_eq!(h.text(), "hlo\nx\nwld");

    // . repeats a block delete from the cursor, the same size.
    let (mut e, mut h) = vim("abcd\nefgh\nijkl");
    feed(&mut e, &mut h, "<C-v>jd.");
    assert_eq!(h.text(), "cd\ngh\nijkl");
}

#[test]
fn visual_block_change_yank_replace_shift() {
    let (mut e, mut h) = vim("abcd\nefgh\nijkl");
    feed(&mut e, &mut h, "l<C-v>jlcXY<Esc>");
    assert_eq!(h.text(), "aXYd\neXYh\nijkl");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "abcd\nefgh\nijkl", "c is one undo step");

    feed(&mut e, &mut h, "gg0l<C-v>jjly");
    assert_eq!(reg(&e, '0').map(|r| r.0), Some("bc\nfg\njk".into()));
    assert_eq!(h.cursor(), (0, 1));
    assert_eq!(e.state().mode, Mode::Normal);

    feed(&mut e, &mut h, "<C-v>jlr-");
    assert_eq!(h.text(), "a--d\ne--h\nijkl");
    feed(&mut e, &mut h, "u");

    feed(&mut e, &mut h, "gg0l<C-v>jj>");
    assert_eq!(h.text(), "a    bcd\ne    fgh\ni    jkl");
    assert_eq!(h.cursor(), (0, 1));
    feed(&mut e, &mut h, "<C-v>jj<");
    assert_eq!(h.text(), "abcd\nefgh\nijkl");

    feed(&mut e, &mut h, "gg0<C-v>jlU");
    assert_eq!(h.text(), "ABcd\nEFgh\nijkl");

    // A line break typed in the block insert is not copied to the other lines.
    let (mut e, mut h) = vim("ab\ncd");
    feed(&mut e, &mut h, "<C-v>jIx<CR><Esc>");
    assert_eq!(h.text(), "x\nab\ncd");
}

#[test]
fn visual_block_dollar_append() {
    let (mut e, mut h) = vim("ab\nabcd\n\nabc");
    feed(&mut e, &mut h, "<C-v>jjj$Ax<Esc>");
    assert_eq!(h.text(), "abx\nabcdx\nx\nabcx");
    assert_eq!(e.state().mode, Mode::Normal);
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "ab\nabcd\n\nabc");

    // Without $, A appends after the block's right edge, padding short lines.
    let (mut e, mut h) = vim("abc\na\nabc");
    feed(&mut e, &mut h, "l<C-v>jjAx<Esc>");
    assert_eq!(h.text(), "abxc\na x\nabxc");

    // $ then d deletes to the end of every line.
    let (mut e, mut h) = vim("abcd\nab\nabc");
    feed(&mut e, &mut h, "l<C-v>jj$d");
    assert_eq!(h.text(), "a\na\na");

    // The block shape is exposed for drawing.
    let (mut e, mut h) = vim("abcd\nab\nabc");
    feed(&mut e, &mut h, "l<C-v>jl");
    let b = e.visual_block(&h).expect("a block");
    assert_eq!(
        (b.top, b.bottom, b.left, b.right, b.to_end),
        (0, 1, 1, 1, false)
    );
    feed(&mut e, &mut h, "j$");
    let b = e.visual_block(&h).expect("a block");
    assert_eq!((b.top, b.bottom, b.left, b.to_end), (0, 2, 1, true));
    feed(&mut e, &mut h, "<Esc>");
    assert_eq!(e.visual_block(&h), None);
}

#[test]
fn ctrl_a_increments() {
    let (mut e, mut h) = vim("x9");
    feed(&mut e, &mut h, "5<C-a>");
    assert_eq!(h.text(), "x14");
    assert_eq!(h.cursor(), (0, 2), "on the number's last digit");

    let (mut e, mut h) = vim("0x0f");
    feed(&mut e, &mut h, "<C-a>");
    assert_eq!(h.text(), "0x10");
    let (mut e, mut h) = vim("v = 0xFE;");
    feed(&mut e, &mut h, "<C-a>");
    assert_eq!(h.text(), "v = 0xFF;", "the case of the hex digits is kept");
    feed(&mut e, &mut h, "<C-a>");
    assert_eq!(h.text(), "v = 0x100;");
    feed(&mut e, &mut h, "<C-x>");
    assert_eq!(h.text(), "v = 0x0ff;", "and their count");

    let (mut e, mut h) = vim("-1");
    feed(&mut e, &mut h, "<C-a>");
    assert_eq!(h.text(), "0");
    feed(&mut e, &mut h, "<C-x><C-x>");
    assert_eq!(h.text(), "-2");
    let (mut e, mut h) = vim("a-b 007");
    feed(&mut e, &mut h, "<C-a>");
    assert_eq!(
        h.text(),
        "a-b 008",
        "the number after the cursor, keeping its zeros"
    );

    // The cursor inside a number; `.` repeats with the count; `u` undoes one.
    let (mut e, mut h) = vim("n: 1234 é");
    h.set_cursor(0, 5);
    feed(&mut e, &mut h, "10<C-x>");
    assert_eq!(h.text(), "n: 1224 é");
    feed(&mut e, &mut h, "..");
    assert_eq!(h.text(), "n: 1204 é");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "n: 1214 é");

    // No number: nothing changes and a macro stops.
    let (mut e, mut h) = vim("1 a\n2 a\nb\n3 a");
    feed(&mut e, &mut h, "qq0<C-a>jq9@q");
    assert_eq!(h.text(), "2 a\n3 a\nb\n3 a");
}

#[test]
fn ex_tab_commands_reach_host() {
    let (mut e, mut h) = vim("text");
    for cmd in ["e foo.rs", "q", "wa", "qa"] {
        feed(&mut e, &mut h, ":");
        answer(&mut e, &mut h, cmd);
        assert_eq!(h.exs.last().map(String::as_str), Some(cmd));
        assert_eq!(e.state().message, "", ":{cmd} was known to the host");
    }
    feed(&mut e, &mut h, ":");
    answer(&mut e, &mut h, "tabnext");
    assert!(e.state().message.contains("E492"));
    assert_eq!(h.exs, ["e foo.rs", "q", "wa", "qa", "tabnext"]);
    assert_eq!(h.saves, 0);
}

#[test]
fn ctrl_keys_escape_cancel_and_ignore() {
    // Ctrl+[ and Ctrl+C leave Visual and Insert mode like Esc, without a message.
    let (mut e, mut h) = vim("abc\ndef");
    feed(&mut e, &mut h, "vl<C-[>");
    assert_eq!(e.state().mode, Mode::Normal);
    feed(&mut e, &mut h, "<C-v>j<C-c>");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(e.state().message, "");
    feed(&mut e, &mut h, "gg0ix<C-c>");
    assert_eq!(e.state().mode, Mode::Normal);
    assert_eq!(e.state().message, "");
    // Ctrl+C does not copy a block insert to the other lines.
    feed(&mut e, &mut h, "0<C-v>jIy<C-c>");
    assert_eq!(h.text(), "yxabc\ndef");
    // Ctrl+W is swallowed in Normal mode (no window commands).
    assert!(e.key(&mut h, "<C-w>"));
    assert_eq!(h.text(), "yxabc\ndef");
    assert_eq!(e.state().mode, Mode::Normal);
}

#[test]
fn marks_restored_by_undo_after_substitute() {
    let text: Vec<String> = (0..20).map(|i| format!("foo {i}")).collect();
    let (mut e, mut h) = vim(&text.join("\n"));
    h.set_cursor(10, 4);
    feed(&mut e, &mut h, "ma:");
    answer(&mut e, &mut h, &format!("%s/foo/f{}/", "o".repeat(19)));
    feed(&mut e, &mut h, "gg`a");
    assert_eq!(h.cursor(), (10, 21));
    feed(&mut e, &mut h, "ugg`a");
    assert_eq!(h.cursor(), (10, 4), "undo puts the mark back");
    feed(&mut e, &mut h, "<C-r>gg`a");
    assert_eq!(h.cursor(), (10, 21), "redo moves it again");
    feed(&mut e, &mut h, "ugg'a");
    assert_eq!(h.cursor(), (10, 0));
}

#[test]
fn marks_restored_by_undo_after_block_insert() {
    let (mut e, mut h) = vim("abcdef\nabcdef\nabcdef");
    h.set_cursor(1, 3);
    feed(&mut e, &mut h, "magg0<C-v>jjIxxxx<Esc>`a");
    assert_eq!(h.cursor(), (1, 7));
    feed(&mut e, &mut h, "u`a");
    assert_eq!(h.cursor(), (1, 3));
    feed(&mut e, &mut h, "<C-r>`a");
    assert_eq!(h.cursor(), (1, 7));
}

#[test]
fn deleted_line_mark_restored_by_undo() {
    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "jmadd'a");
    assert!(e.state().message.contains("E20"));
    feed(&mut e, &mut h, "ugg'a");
    assert_eq!(
        h.cursor(),
        (1, 0),
        "undo brings the mark back with its line"
    );
    feed(&mut e, &mut h, "<C-r>gg'a");
    assert!(e.state().message.contains("E20"), "redo deletes it again");

    // A mark set after the change follows the text through the undo.
    let (mut e, mut h) = vim("a\nb\nc\nd");
    feed(&mut e, &mut h, "jddjmbu'b");
    assert_eq!(h.text(), "a\nb\nc\nd");
    assert_eq!(h.cursor(), (3, 0));
    // Undo of an edit the engine did not record (made by the host) still moves marks.
    h.buf.begin_group(0);
    h.buf.replace(0..0, "zz\n");
    h.buf.end_group(0);
    e.adjust_marks(0, 0, 3);
    feed(&mut e, &mut h, "u'b");
    assert_eq!(h.text(), "a\nb\nc\nd");
    assert_eq!(h.cursor(), (3, 0));
}

#[test]
fn block_yank_put_keeps_shape() {
    let (mut e, mut h) = vim("abcd\nefgh\nijkl");
    feed(&mut e, &mut h, "<C-v>jly$p");
    assert_eq!(h.text(), "abcdab\nefghef\nijkl");
    assert_eq!(h.cursor(), (0, 4));

    // Short block lines are padded when text follows; lines are added at the end.
    let (mut e, mut h) = vim("12\n3\nabcd\na");
    feed(&mut e, &mut h, "j<C-v>kly2jP");
    assert_eq!(h.text(), "12\n3\n12abcd\n3 a");
    feed(&mut e, &mut h, "u3jp");
    assert_eq!(h.text(), "12\n3\nabcd\na12\n 3");

    // A count repeats the block sideways.
    let (mut e, mut h) = vim("ab\ncd");
    feed(&mut e, &mut h, "<C-v>jy2p");
    assert_eq!(h.text(), "aaab\ncccd");
    assert_eq!(reg(&e, '"').map(|r| r.0), Some("a\nc".into()));
}

#[test]
fn block_delete_then_put_moves_column() {
    let (mut e, mut h) = vim("abc\nabc\nabc");
    feed(&mut e, &mut h, "<C-v>jjdp");
    assert_eq!(h.text(), "bac\nbac\nbac");
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "bc\nbc\nbc", "the put is one undo step");
    // Putting over a Visual block is still refused, leaving the text alone.
    feed(&mut e, &mut h, "<C-v>jp");
    assert_eq!(h.text(), "bc\nbc\nbc");
    assert_eq!(e.state().mode, Mode::Normal);
}

#[test]
fn block_r_cr_does_not_corrupt() {
    let (mut e, mut h) = vim("abcd\nefgh\nijkl");
    feed(&mut e, &mut h, "<C-v>jjr<CR>");
    assert_eq!(h.text(), "abcd\nefgh\nijkl");
    assert_eq!(e.state().mode, Mode::Normal);
}

#[test]
fn visual_ctrl_keys_consumed() {
    let (mut e, mut h) = vim("1\nx 2\n3 4");
    feed(&mut e, &mut h, "Vjj<C-a>");
    assert_eq!(h.text(), "2\nx 3\n4 4", "the first number of each line");
    assert_eq!(e.state().mode, Mode::Normal);
    feed(&mut e, &mut h, "u");
    assert_eq!(h.text(), "1\nx 2\n3 4", "one undo step");

    let (mut e, mut h) = vim("a 5 b 6");
    h.set_cursor(0, 3);
    feed(&mut e, &mut h, "v$3<C-a>");
    assert_eq!(h.text(), "a 5 b 9", "only numbers inside the selection");
    assert_eq!(h.cursor(), (0, 3));

    let (mut e, mut h) = vim("10 10\n20 20");
    h.set_cursor(0, 3);
    feed(&mut e, &mut h, "<C-v>jl<C-x>");
    assert_eq!(h.text(), "10 9\n20 19");

    let (mut e, mut h) = vim("a\nb\nc");
    feed(&mut e, &mut h, "Gggv");
    for k in ["<C-o>", "<C-i>", "<Tab>", "<C-a>", "<C-x>"] {
        assert!(e.key(&mut h, k), "{k} is consumed in Visual mode");
        if e.state().mode == Mode::Normal {
            feed(&mut e, &mut h, "v");
        }
    }
    assert_eq!(h.text(), "a\nb\nc");
}

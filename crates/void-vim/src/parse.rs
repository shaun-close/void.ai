//! The Normal and Visual mode command grammar (`:help normal-index`, `:help visual-index`):
//! `["x][count]command`, where an operator takes `[count]` and a motion, a text object or its
//! own key again (`dd`). Parsing restarts from the pending keys on every key press, so a
//! command is either complete, still waiting for keys, or invalid.

use crate::Mode;
use crate::block::{BlockOp, BlockSize};
use crate::keys::char_of;
use crate::motion::{Find, Motion};
use crate::registers::Registers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fail {
    /// The keys so far are the start of a command.
    More,
    /// The keys are not a command.
    Bad,
}

type P<T> = Result<T, Fail>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
    Toggle,
    Lower,
    Upper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjKind {
    Word { big: bool },
    Block { open: char, close: char },
    Quote(char),
    Paragraph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Object {
    pub inner: bool,
    pub kind: ObjKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    Motion(Motion),
    Object(Object),
    /// The operator's own key again (`dd`, `>>`, `gUU`): `count` whole lines.
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ins {
    Before,
    After,
    LineStart,
    LineEnd,
    Below,
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scroll {
    Top,
    Center,
    Bottom,
    TopFirst,
    CenterFirst,
    BottomFirst,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    LineDown,
    LineUp,
}

/// How much text a Visual operator covered, so `.` can cover the same amount
/// (`:help visual-repeat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extent {
    Lines(usize),
    /// Within one line: this many chars.
    Chars(usize),
    /// Over several lines: this many more lines, ending at this column.
    Span(usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Act {
    Move(Motion),
    Op(Op, Target),
    /// An operator waiting for a `/` or `?` pattern.
    OpSearch(Op, bool),
    Insert(Ins),
    Replace,
    Paste {
        before: bool,
        after: bool,
    },
    Join {
        spaces: bool,
    },
    ReplaceChar(char),
    ToggleCase,
    Repeat,
    Undo,
    Redo,
    Visual(Mode),
    Reselect,
    Search {
        forward: bool,
    },
    Ex,
    Record(char),
    StopRecord,
    Play(char),
    Scroll(Scroll),
    VisualOp(Op),
    VisualLinesOp(Op),
    VisualObject(Object),
    VisualSwap,
    /// Only built by dot-repeat of a Visual operator.
    VisualRepeat(Op, Extent),
    SetMark(char),
    /// `<C-o>` / `<C-i>`.
    Jump {
        older: bool,
    },
    /// `<C-a>` / `<C-x>`.
    Add {
        subtract: bool,
    },
    /// Visual-block `I` / `A`.
    BlockInsert {
        append: bool,
    },
    /// Only built by dot-repeat of a Visual-block command.
    BlockRepeat(BlockOp, BlockSize),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cmd {
    pub reg: Option<char>,
    pub count: Option<usize>,
    pub act: Act,
}

const MAX_COUNT: usize = 99_999_999;

struct Keys<'a> {
    keys: &'a [String],
    i: usize,
}

impl<'a> Keys<'a> {
    fn peek(&self) -> P<&'a str> {
        self.keys.get(self.i).map(String::as_str).ok_or(Fail::More)
    }

    fn next(&mut self) -> P<&'a str> {
        let k = self.peek()?;
        self.i += 1;
        Ok(k)
    }

    /// Digits typed as a count (a `0` only continues one).
    fn count(&mut self, count: &mut Option<usize>) {
        while let Ok(k) = self.peek() {
            let Some(d) = k.chars().next().and_then(|c| c.to_digit(10)) else {
                return;
            };
            if k.len() != 1 || (d == 0 && count.is_none()) {
                return;
            }
            self.i += 1;
            *count = Some((count.unwrap_or(0) * 10 + d as usize).min(MAX_COUNT));
        }
    }
}

pub(crate) fn parse(keys: &[String], visual: bool, recording: bool) -> P<Cmd> {
    let mut k = Keys { keys, i: 0 };
    let mut reg = None;
    let mut count = None;
    loop {
        k.count(&mut count);
        if k.peek()? != "\"" {
            break;
        }
        k.next()?;
        let r = char_of(k.next()?).ok_or(Fail::Bad)?;
        if !Registers::is_valid(r) {
            return Err(Fail::Bad);
        }
        reg = Some(r);
    }
    let key = k.next()?;
    let act = if visual {
        visual_act(key, &mut k)?
    } else {
        normal_act(key, &mut k, recording, &mut count)?
    };
    Ok(Cmd { reg, count, act })
}

fn normal_act(key: &str, k: &mut Keys, recording: bool, count: &mut Option<usize>) -> P<Act> {
    if let Some(op) = operator(key, k)? {
        return op_target(op, k, count);
    }
    if let Some(m) = motion(key, k)? {
        return Ok(Act::Move(m));
    }
    if let Some(s) = scroll(key, k)? {
        return Ok(Act::Scroll(s));
    }
    Ok(match key {
        "x" | "<Del>" => Act::Op(Op::Delete, Target::Motion(Motion::Right)),
        "X" => Act::Op(Op::Delete, Target::Motion(Motion::Left)),
        "D" => Act::Op(Op::Delete, Target::Motion(Motion::LineEnd)),
        "C" => Act::Op(Op::Change, Target::Motion(Motion::LineEnd)),
        "s" => Act::Op(Op::Change, Target::Motion(Motion::Right)),
        "S" => Act::Op(Op::Change, Target::Lines),
        "Y" => Act::Op(Op::Yank, Target::Lines),
        "i" | "<Insert>" => Act::Insert(Ins::Before),
        "a" => Act::Insert(Ins::After),
        "I" => Act::Insert(Ins::LineStart),
        "A" => Act::Insert(Ins::LineEnd),
        "o" => Act::Insert(Ins::Below),
        "O" => Act::Insert(Ins::Above),
        "R" => Act::Replace,
        "p" => Act::Paste {
            before: false,
            after: false,
        },
        "P" => Act::Paste {
            before: true,
            after: false,
        },
        "J" => Act::Join { spaces: true },
        "r" => Act::ReplaceChar(char_arg(k.next()?, true)?),
        "~" => Act::ToggleCase,
        "." => Act::Repeat,
        "u" => Act::Undo,
        "<C-r>" => Act::Redo,
        "m" => {
            let name = char_of(k.next()?).ok_or(Fail::Bad)?;
            if !crate::marks::is_mark(name) {
                return Err(Fail::Bad);
            }
            Act::SetMark(name)
        }
        "<C-o>" => Act::Jump { older: true },
        "<Tab>" => Act::Jump { older: false },
        "<C-a>" => Act::Add { subtract: false },
        "<C-x>" => Act::Add { subtract: true },
        "v" => Act::Visual(Mode::Visual),
        "V" => Act::Visual(Mode::VisualLine),
        "<C-v>" => Act::Visual(Mode::VisualBlock),
        "/" => Act::Search { forward: true },
        "?" => Act::Search { forward: false },
        ":" => Act::Ex,
        "q" if recording => Act::StopRecord,
        "q" => {
            let r = char_of(k.next()?).ok_or(Fail::Bad)?;
            if !(r.is_ascii_alphanumeric() || r == '"') {
                return Err(Fail::Bad);
            }
            Act::Record(r)
        }
        "@" => {
            let r = char_of(k.next()?).ok_or(Fail::Bad)?;
            if !(r == '@' || Registers::is_valid(r)) {
                return Err(Fail::Bad);
            }
            Act::Play(r)
        }
        "g" => match k.next()? {
            "v" => Act::Reselect,
            "J" => Act::Join { spaces: false },
            "p" => Act::Paste {
                before: false,
                after: true,
            },
            "P" => Act::Paste {
                before: true,
                after: true,
            },
            _ => return Err(Fail::Bad),
        },
        _ => return Err(Fail::Bad),
    })
}

fn visual_act(key: &str, k: &mut Keys) -> P<Act> {
    if let Some(m) = motion(key, k)? {
        return Ok(Act::Move(m));
    }
    if let Some(s) = scroll(key, k)? {
        return Ok(Act::Scroll(s));
    }
    Ok(match key {
        "d" | "x" | "<Del>" => Act::VisualOp(Op::Delete),
        "y" => Act::VisualOp(Op::Yank),
        "c" | "s" => Act::VisualOp(Op::Change),
        ">" => Act::VisualOp(Op::Indent),
        "<" => Act::VisualOp(Op::Outdent),
        "~" => Act::VisualOp(Op::Toggle),
        "u" => Act::VisualOp(Op::Lower),
        "U" => Act::VisualOp(Op::Upper),
        "D" | "X" => Act::VisualLinesOp(Op::Delete),
        "Y" => Act::VisualLinesOp(Op::Yank),
        "C" | "S" | "R" => Act::VisualLinesOp(Op::Change),
        "J" => Act::Join { spaces: true },
        "r" => Act::ReplaceChar(char_arg(k.next()?, true)?),
        "<C-a>" => Act::Add { subtract: false },
        "<C-x>" => Act::Add { subtract: true },
        "<C-o>" => Act::Jump { older: true },
        "<Tab>" => Act::Jump { older: false },
        "I" => Act::BlockInsert { append: false },
        "A" => Act::BlockInsert { append: true },
        "p" => Act::Paste {
            before: false,
            after: false,
        },
        "P" => Act::Paste {
            before: true,
            after: false,
        },
        "i" | "a" => Act::VisualObject(object(k.next()?, key == "i")?),
        "o" | "O" => Act::VisualSwap,
        "v" => Act::Visual(Mode::Visual),
        "V" => Act::Visual(Mode::VisualLine),
        "<C-v>" => Act::Visual(Mode::VisualBlock),
        ":" => Act::Ex,
        "/" => Act::Search { forward: true },
        "?" => Act::Search { forward: false },
        "g" => match k.next()? {
            "v" => Act::Reselect,
            "J" => Act::Join { spaces: false },
            "~" => Act::VisualOp(Op::Toggle),
            "u" => Act::VisualOp(Op::Lower),
            "U" => Act::VisualOp(Op::Upper),
            _ => return Err(Fail::Bad),
        },
        _ => return Err(Fail::Bad),
    })
}

fn operator(key: &str, k: &mut Keys) -> P<Option<Op>> {
    Ok(Some(match key {
        "d" => Op::Delete,
        "c" => Op::Change,
        "y" => Op::Yank,
        ">" => Op::Indent,
        "<" => Op::Outdent,
        "g" => {
            let op = match k.peek()? {
                "~" => Op::Toggle,
                "u" => Op::Lower,
                "U" => Op::Upper,
                _ => return Ok(None),
            };
            k.next()?;
            op
        }
        _ => return Ok(None),
    }))
}

fn op_target(op: Op, k: &mut Keys, count: &mut Option<usize>) -> P<Act> {
    let mut count2 = None;
    k.count(&mut count2);
    if let Some(c2) = count2 {
        *count = Some((count.unwrap_or(1) * c2).min(MAX_COUNT));
    }
    let own = match op {
        Op::Delete => "d",
        Op::Change => "c",
        Op::Yank => "y",
        Op::Indent => ">",
        Op::Outdent => "<",
        Op::Toggle => "~",
        Op::Lower => "u",
        Op::Upper => "U",
    };
    let key = k.next()?;
    let g_op = matches!(op, Op::Toggle | Op::Lower | Op::Upper);
    if key == own || (g_op && key == "g" && k.peek()? == own && k.next().is_ok()) {
        return Ok(Act::Op(op, Target::Lines));
    }
    match key {
        "i" | "a" => Ok(Act::Op(op, Target::Object(object(k.next()?, key == "i")?))),
        "/" => Ok(Act::OpSearch(op, true)),
        "?" => Ok(Act::OpSearch(op, false)),
        _ => match motion(key, k)? {
            Some(m) => Ok(Act::Op(op, Target::Motion(m))),
            None => Err(Fail::Bad),
        },
    }
}

fn motion(key: &str, k: &mut Keys) -> P<Option<Motion>> {
    let find = |forward, till, k: &mut Keys| -> P<Option<Motion>> {
        Ok(Some(Motion::Find(
            Find { forward, till },
            char_arg(k.next()?, false)?,
        )))
    };
    Ok(Some(match key {
        "h" | "<Left>" => Motion::Left,
        "l" | "<Right>" => Motion::Right,
        "<BS>" => Motion::BackWrap,
        " " => Motion::ForwardWrap,
        "j" | "<Down>" | "<C-j>" | "<C-n>" => Motion::Down,
        "k" | "<Up>" | "<C-p>" => Motion::Up,
        "w" => Motion::WordStart { big: false },
        "W" => Motion::WordStart { big: true },
        "b" => Motion::WordBack { big: false },
        "B" => Motion::WordBack { big: true },
        "e" => Motion::WordEnd { big: false },
        "E" => Motion::WordEnd { big: true },
        "0" | "<Home>" => Motion::LineStart,
        "^" => Motion::FirstNonBlank,
        "$" | "<End>" => Motion::LineEnd,
        "|" => Motion::Column,
        "G" => Motion::LastLine,
        "f" => return find(true, false, k),
        "t" => return find(true, true, k),
        "F" => return find(false, false, k),
        "T" => return find(false, true, k),
        ";" => Motion::RepeatFind { reverse: false },
        "," => Motion::RepeatFind { reverse: true },
        "%" => Motion::Match,
        "'" | "`" => {
            let name = char_of(k.next()?).ok_or(Fail::Bad)?;
            if !crate::marks::is_mark(name) {
                return Err(Fail::Bad);
            }
            Motion::Mark {
                name,
                line: key == "'",
            }
        }
        "}" => Motion::ParagraphForward,
        "{" => Motion::ParagraphBack,
        "+" | "<CR>" => Motion::NextLine,
        "-" => Motion::PrevLine,
        "_" => Motion::CurrentLine,
        "H" => Motion::ScreenTop,
        "M" => Motion::ScreenMiddle,
        "L" => Motion::ScreenBottom,
        "n" => Motion::SearchNext { reverse: false },
        "N" => Motion::SearchNext { reverse: true },
        "*" => Motion::Word {
            forward: true,
            whole: true,
        },
        "#" => Motion::Word {
            forward: false,
            whole: true,
        },
        "g" => {
            let m = match k.peek()? {
                "g" => Motion::FirstLine,
                "e" => Motion::WordEndBack { big: false },
                "E" => Motion::WordEndBack { big: true },
                "_" => Motion::LastNonBlank,
                "j" => Motion::Down,
                "k" => Motion::Up,
                "0" => Motion::LineStart,
                "^" => Motion::FirstNonBlank,
                "$" => Motion::LineEnd,
                "*" => Motion::Word {
                    forward: true,
                    whole: false,
                },
                "#" => Motion::Word {
                    forward: false,
                    whole: false,
                },
                _ => return Ok(None),
            };
            k.next()?;
            m
        }
        _ => return Ok(None),
    }))
}

fn scroll(key: &str, k: &mut Keys) -> P<Option<Scroll>> {
    Ok(Some(match key {
        "<C-d>" => Scroll::HalfDown,
        "<C-u>" => Scroll::HalfUp,
        "<C-f>" | "<PageDown>" => Scroll::PageDown,
        "<C-b>" | "<PageUp>" => Scroll::PageUp,
        "<C-e>" => Scroll::LineDown,
        "<C-y>" => Scroll::LineUp,
        "z" => match k.next()? {
            "z" => Scroll::Center,
            "t" => Scroll::Top,
            "b" => Scroll::Bottom,
            "<CR>" => Scroll::TopFirst,
            "." => Scroll::CenterFirst,
            "-" => Scroll::BottomFirst,
            _ => return Err(Fail::Bad),
        },
        _ => return Ok(None),
    }))
}

fn object(key: &str, inner: bool) -> P<Object> {
    let kind = match key {
        "w" => ObjKind::Word { big: false },
        "W" => ObjKind::Word { big: true },
        "(" | ")" | "b" => ObjKind::Block {
            open: '(',
            close: ')',
        },
        "[" | "]" => ObjKind::Block {
            open: '[',
            close: ']',
        },
        "{" | "}" | "B" => ObjKind::Block {
            open: '{',
            close: '}',
        },
        "<" | ">" => ObjKind::Block {
            open: '<',
            close: '>',
        },
        "\"" | "'" | "`" => ObjKind::Quote(key.chars().next().unwrap_or('"')),
        "p" => ObjKind::Paragraph,
        _ => return Err(Fail::Bad),
    };
    Ok(Object { inner, kind })
}

/// The character argument of `f`, `t`, `r`, …; `<CR>` (a line break) only where allowed.
fn char_arg(key: &str, newline: bool) -> P<char> {
    match key {
        "<Tab>" => Ok('\t'),
        "<CR>" if newline => Ok('\n'),
        k => char_of(k).ok_or(Fail::Bad),
    }
}

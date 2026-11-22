//! Vim key names: building them from UI key events, normalising aliases, and storing them in a
//! register as text (for macros).

use std::borrow::Cow;

/// A key press as a UI reports it, without depending on any UI crate.
///
/// `key` is either the character the key produces (`"a"`, `"?"`, `"["`) or a key name
/// (`"Escape"`, `"Enter"`, `"ArrowLeft"`, `"F5"`, …). For ASCII letters `shift` decides the case,
/// so `"A"` without Shift is `a`; other characters are taken as given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLike<'a> {
    pub key: &'a str,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// The Vim name of a key press: `"a"`, `"A"`, `"<Esc>"`, `"<CR>"`, `"<C-r>"`, `"<S-Tab>"`, …
/// `None` for keys Vim has no name for (lone modifiers, unknown names).
pub fn key_name(key: KeyLike) -> Option<String> {
    let mut chars = key.key.chars();
    let (first, rest) = (chars.next()?, chars.next());
    let mods = |shift: bool| {
        let mut m = String::new();
        if key.ctrl {
            m.push_str("C-");
        }
        if key.alt {
            m.push_str("M-");
        }
        if shift {
            m.push_str("S-");
        }
        m
    };
    if rest.is_none() {
        let c = if first.is_ascii_alphabetic() {
            if key.shift {
                first.to_ascii_uppercase()
            } else {
                first.to_ascii_lowercase()
            }
        } else {
            first
        };
        let name = match c {
            ' ' => "Space".to_string(),
            '<' => "lt".to_string(),
            c => c.to_string(),
        };
        // Shift is already in the character, except for space.
        let m = mods(key.shift && c == ' ');
        return Some(if m.is_empty() && name.chars().count() == 1 {
            name
        } else if c.is_ascii_alphabetic() && key.ctrl && key.shift {
            // Keep the letter lower-case and say Shift, so Ctrl+Shift+V is <C-S-v>.
            format!("<{}{}>", mods(true), c.to_ascii_lowercase())
        } else {
            format!("<{m}{name}>")
        });
    }
    let name = match key.key {
        "Escape" | "Esc" => "Esc",
        "Enter" | "Return" => "CR",
        "Backspace" => "BS",
        "Tab" => "Tab",
        "Delete" | "Del" => "Del",
        "Insert" => "Insert",
        "ArrowLeft" | "Left" => "Left",
        "ArrowRight" | "Right" => "Right",
        "ArrowUp" | "Up" => "Up",
        "ArrowDown" | "Down" => "Down",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "PageUp",
        "PageDown" => "PageDown",
        "Space" => "Space",
        f if f.len() <= 3
            && f.starts_with('F')
            && f[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) =>
        {
            f
        }
        _ => return None,
    };
    Some(format!("<{}{name}>", mods(key.shift)))
}

/// Normalise aliases so the engine matches one spelling: printable keys become their character,
/// Ctrl+[ is Esc, Ctrl+M is Enter, Ctrl+I is Tab, Ctrl+H is Backspace.
pub(crate) fn canon(key: &str) -> Cow<'_, str> {
    Cow::Borrowed(match key {
        "<Space>" => " ",
        "<lt>" => "<",
        "<Bar>" => "|",
        "<Bslash>" => "\\",
        "<C-[>" => "<Esc>",
        "<Enter>" | "<Return>" | "<C-m>" => "<CR>",
        "<C-i>" => "<Tab>",
        "<C-h>" => "<BS>",
        "<NL>" => "<C-j>",
        _ => return Cow::Borrowed(key),
    })
}

/// The single character a key types, if it is a printable one.
pub(crate) fn char_of(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let c = chars.next()?;
    (chars.next().is_none() && !c.is_control()).then_some(c)
}

const SPECIAL_OPEN: char = '\u{E000}';
const SPECIAL_CLOSE: char = '\u{E001}';

/// Append `key` to `out` the way Vim keeps keys in a register: characters as themselves,
/// Esc/Enter/Tab/Backspace and Ctrl+letter as control characters, any other named key wrapped in
/// private-use markers.
pub(crate) fn encode(key: &str, out: &mut String) {
    let key = canon(key);
    if let Some(c) = char_of(&key) {
        out.push(c);
        return;
    }
    let control = match &*key {
        "<Esc>" => Some('\x1b'),
        "<CR>" => Some('\r'),
        "<Tab>" => Some('\t'),
        "<BS>" => Some('\x08'),
        k => k
            .strip_prefix("<C-")
            .and_then(|r| r.strip_suffix('>'))
            .and_then(|l| {
                let mut c = l.chars();
                match (c.next(), c.next()) {
                    (Some(l), None) if l.is_ascii_lowercase() => {
                        Some(char::from(l as u8 - b'a' + 1))
                    }
                    _ => None,
                }
            }),
    };
    match control {
        Some(c) => out.push(c),
        None => {
            out.push(SPECIAL_OPEN);
            out.push_str(&key);
            out.push(SPECIAL_CLOSE);
        }
    }
}

/// Split register text back into keys (the inverse of [`encode`]); a line break is Ctrl+J.
pub(crate) fn decode(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        let key = match c {
            SPECIAL_OPEN => chars.by_ref().take_while(|c| *c != SPECIAL_CLOSE).collect(),
            '\x1b' => "<Esc>".into(),
            '\r' => "<CR>".into(),
            '\n' => "<C-j>".into(),
            '\t' => "<Tab>".into(),
            '\x08' => "<BS>".into(),
            c @ '\x01'..='\x1a' => format!("<C-{}>", char::from(c as u8 - 1 + b'a')),
            c => c.to_string(),
        };
        keys.push(key);
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_a_register() {
        let keys = [
            "a", "<Esc>", "<CR>", "<C-r>", "<Left>", "<F5>", "é", "<Tab>", "<BS>",
        ];
        let mut text = String::new();
        for k in keys {
            encode(k, &mut text);
        }
        assert_eq!(decode(&text), keys);
        assert_eq!(decode("a\nb"), ["a", "<C-j>", "b"]);
    }
}

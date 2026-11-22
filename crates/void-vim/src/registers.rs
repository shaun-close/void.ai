//! Registers (`:help registers`): unnamed, `0` (last yank), `1`–`9` (deleted lines, shifting),
//! `-` (small deletes), named `a`–`z` (`A`–`Z` append), `_` (black hole). `+` and `*` are the
//! host's clipboard; `:` and `/` hold the last command line and search pattern.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Register {
    pub text: String,
    /// Whole lines: `text` ends with a line break and puts go above or below the cursor line.
    pub linewise: bool,
}

#[derive(Debug, Default)]
pub struct Registers {
    map: HashMap<char, Register>,
}

/// What wrote a register, which decides the numbered and small-delete registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write {
    Yank,
    Delete,
}

impl Registers {
    /// The contents of register `name` (`'"'` is the unnamed one). The clipboard registers live
    /// in the host, so `+` and `*` are not here.
    pub fn get(&self, name: char) -> Option<&Register> {
        self.map.get(&name.to_ascii_lowercase())
    }

    pub(crate) fn is_valid(name: char) -> bool {
        name.is_ascii_alphanumeric() || "\"-_+*.:/".contains(name)
    }

    pub(crate) fn set(&mut self, name: char, reg: Register) {
        self.map.insert(name, reg);
    }

    /// Store text from a yank or delete into `name` (or the defaults when `None`), and point the
    /// unnamed register at it.
    pub(crate) fn store(&mut self, name: Option<char>, reg: Register, write: Write) {
        match name {
            Some('_') => return,
            Some(c @ 'A'..='Z') => {
                let lower = c.to_ascii_lowercase();
                let reg = match self.map.remove(&lower) {
                    Some(mut old) => {
                        if reg.linewise && !old.linewise && !old.text.ends_with('\n') {
                            old.text.push('\n');
                        }
                        old.text.push_str(&reg.text);
                        old.linewise |= reg.linewise;
                        old
                    }
                    None => reg,
                };
                self.map.insert(lower, reg);
                let r = self.map[&lower].clone();
                self.map.insert('"', r);
                return;
            }
            Some(c) if c != '"' => {
                self.map.insert(c, reg.clone());
            }
            _ => match write {
                Write::Yank => {
                    self.map.insert('0', reg.clone());
                }
                Write::Delete if reg.linewise || reg.text.contains('\n') => {
                    for n in (1..9).rev() {
                        let from = char::from(b'0' + n);
                        if let Some(r) = self.map.remove(&from) {
                            self.map.insert(char::from(b'0' + n + 1), r);
                        }
                    }
                    self.map.insert('1', reg.clone());
                }
                Write::Delete => {
                    self.map.insert('-', reg.clone());
                }
            },
        }
        self.map.insert('"', reg);
    }
}

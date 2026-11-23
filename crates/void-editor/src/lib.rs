//! UI-agnostic text editing core: rope buffer, undo history, file IO and watching files for
//! outside changes.
//!
//! Every position is a char index into the rope. Byte offsets and (row, byte column) points
//! appear only in [`Edit`], for tree-sitter.

mod buffer;
mod file;
mod lang;
mod search;
mod syntax;
mod watch;

pub use buffer::{Buffer, Edit, LineEnding, Point};
pub use lang::Lang;
pub use search::{
    CaseMode, Query, captures_in, find_all, find_next, find_prev, replace_all, validate,
};
pub use syntax::{Hl, Span, Symbol, SymbolKind, Syntax, Tree};
pub use watch::{DEBOUNCE, Fingerprint, OnDisk, Watcher, check};

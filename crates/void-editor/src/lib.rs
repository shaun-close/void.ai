//! UI-agnostic text editing core: rope buffer, undo history and file IO.
//!
//! Every position is a char index into the rope. Byte offsets and (row, byte column) points
//! appear only in [`Edit`], for tree-sitter.

mod buffer;
mod file;
mod lang;
mod syntax;

pub use buffer::{Buffer, Edit, LineEnding, Point};
pub use lang::Lang;
pub use syntax::{Hl, Span, Symbol, SymbolKind, Syntax};

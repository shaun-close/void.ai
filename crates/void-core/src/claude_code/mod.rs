//! Claude Code backend building blocks: CLI arguments, stdin lines, and stdout parsing.
//! UI-free; `turn` runs one chat turn's `claude` process for the engine.

pub mod args;
pub mod permission;
pub mod stream;
pub mod turn;

pub use args::{SessionMode, TurnArgs, build_args};
pub use stream::{
    CcEvent, control_response_allow, control_response_deny, control_response_error,
    initialize_request, interrupt_request, parse_events, user_message,
};

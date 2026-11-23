//! Claude Code backend building blocks: CLI arguments, stdin lines, and stdout parsing.
//! Pure and UI-free; the process engine lives elsewhere.

pub mod args;
pub mod stream;

pub use args::{SessionMode, TurnArgs, build_args};
pub use stream::{
    CcEvent, control_response_allow, control_response_deny, control_response_error,
    initialize_request, interrupt_request, parse_events, user_message,
};

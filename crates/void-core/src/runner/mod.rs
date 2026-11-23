//! The runner model (UI-free): moon task discovery, run configs and environment profiles.
//! Runs are the user's own and never sandboxed; env values live in memory only.

pub mod config;
pub mod env;
pub mod moon;

pub use config::{Group, OnFailure, RunConfig, Runs, Step, StepConfig};
pub use env::{EnvProfile, EnvValue, Envs};
pub use moon::{MoonTask, discover, parse_moon_tasks};

/// SIGKILL a whole process group (children started with `process_group(0)`).
pub(crate) fn kill_group(pid: u32) {
    // SAFETY: plain syscall; a stale pid at worst fails with ESRCH.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

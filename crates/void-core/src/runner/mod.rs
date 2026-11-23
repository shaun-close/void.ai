//! The runner model (UI-free): moon task discovery, run configs and environment profiles.
//! Runs are the user's own and never sandboxed; env values live in memory only.

pub mod config;
pub mod env;
pub mod moon;

pub use config::{Group, OnFailure, RunConfig, Runs, Step, StepConfig};
pub use env::{EnvProfile, EnvValue, Envs};
pub use moon::{MoonTask, discover, parse_moon_tasks};

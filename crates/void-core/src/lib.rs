//! void.ai core: the UI-agnostic harness (backends, conversations, context, storage, engine).

pub mod agent;
pub mod backend;
pub mod config;
pub mod context;
pub mod engine;
pub mod fsutil;
pub mod model;
pub mod overlay;
pub mod permissions;
pub mod sandbox;
pub mod store;
#[cfg(test)]
pub(crate) mod testutil;
pub mod tools;

/// The crate version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!super::version().is_empty());
    }
}

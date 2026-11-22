//! void.ai core: the UI-agnostic harness (backends, conversations, context, storage, engine).

pub mod backend;
pub mod config;
pub mod fsutil;
pub mod model;
pub mod store;

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

//! A module outside `src`, which only a `#[path]` reaches.

pub(crate) static CELL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

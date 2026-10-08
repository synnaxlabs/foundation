//! Defines the kind contract (parse, check, discover, run), the thin supervisor, `ctx`,
//! the component library, and the compositions.

pub mod cancel;
pub mod endpoint;
pub mod http;
pub mod kind;
pub mod pace;
pub mod reader;
pub mod retry;
pub mod supervisor;

#[cfg(test)]
mod common;

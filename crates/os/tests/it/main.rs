//! Tests of the `os` drivers on the real operating system, through `env`.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod clock;
mod entropy;
mod wall;

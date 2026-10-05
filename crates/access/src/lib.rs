//! Decides whether a subject may do an action on a name: union of allows, authority
//! cap.

#![deny(clippy::wildcard_enum_match_arm)]

#[cfg(feature = "broken")]
compile_error!("deliberate break: the features job must fail");

/// Doubles `x`. Deliberate break: no test checks it, so the mutants job must fail.
#[must_use]
pub fn double(x: u32) -> u32 {
    x * 2
}

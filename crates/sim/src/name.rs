//! The answers to the name lookups of a run.

use std::net::IpAddr;

use types::time::Span;

/// How a lookup of one name goes. Build it with `..Config::default()`: fields get
/// added.
///
/// ```
/// let historian = "10.0.0.2".parse().expect("an address");
/// let config = sim::name::Config {
///     answer: sim::name::Answer::Addresses(vec![historian]),
///     ..sim::name::Config::default()
/// };
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// What the lookup gives.
    pub answer: Answer,
    /// The time that the lookup takes on the clock of its node. Not negative. Zero,
    /// the default, answers at the first poll.
    pub delay: Span,
}

/// What a lookup gives.
///
/// ```
/// let lost = sim::name::Answer::Failed;
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// These addresses, in order, each with the port of the lookup. With none, the
    /// default, the lookup gives [`env::net::Error::NotFound`].
    Addresses(Vec<IpAddr>),
    /// [`env::net::Error::Io`] with `EAGAIN` (11), as when no name server answers.
    Failed,
}

impl Default for Answer {
    fn default() -> Self {
        Self::Addresses(Vec::new())
    }
}

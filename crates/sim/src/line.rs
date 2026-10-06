//! The serial lines between node ports: their faults.

use crate::chance;

/// The faults of one serial line, in both directions. Build it with
/// `..Config::default()`: fields get added.
///
/// ```
/// let noisy = sim::line::Config { flip: 0.01, ..sim::line::Config::default() };
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Config {
    /// The chance, from 0 to 1, that a byte is lost. 1 cuts the line.
    pub loss: f64,
    /// The chance, from 0 to 1, that one bit of a byte flips. With parity on, the
    /// receiver finds the error and the byte is lost.
    pub flip: f64,
}

impl Config {
    /// Panics with the config when a chance is not from 0 to 1.
    pub(crate) fn check(&self) {
        let valid = [self.loss, self.flip].into_iter().all(chance::valid);
        assert!(valid, "{self:?} has a chance outside 0 to 1");
    }
}

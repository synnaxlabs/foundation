//! Sample quality.

/// An OPC UA status code. The top two bits give the severity: good, uncertain, or bad.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Quality(pub u32);

impl Quality {
    /// The value is good.
    pub const GOOD: Self = Self(0);

    /// Reports whether the severity is good.
    #[must_use]
    pub const fn good(self) -> bool {
        self.0 >> 30 == 0b00
    }

    /// Reports whether the severity is uncertain.
    #[must_use]
    pub const fn uncertain(self) -> bool {
        self.0 >> 30 == 0b01
    }

    /// Reports whether the severity is bad.
    #[must_use]
    pub const fn bad(self) -> bool {
        self.0 >> 30 == 0b10
    }
}

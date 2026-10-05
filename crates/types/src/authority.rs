//! Control authority.

use std::fmt;

/// How strongly a writer claims control of an index. A higher authority takes control
/// from a lower one; an equal one waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Authority(pub u8);

impl Authority {
    /// The highest authority. Nothing can take control from it.
    pub const ABSOLUTE: Self = Self(u8::MAX);
}

impl fmt::Display for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::Authority;

    #[test]
    fn orders_by_number() {
        assert!(Authority(2) > Authority(1));
        assert!(Authority::ABSOLUTE > Authority(254));
    }

    #[test]
    fn displays_the_number() {
        assert_eq!(Authority(7).to_string(), "7");
        assert_eq!(Authority::ABSOLUTE.to_string(), "255");
    }
}

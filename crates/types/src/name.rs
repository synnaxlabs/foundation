//! Names in the one name tree, and the one matcher every selector uses.
//!
//! Channels, nodes, connectors, subjects, and secrets share one tree of dot-separated
//! names. Policies, readers, connectors, and access select names with a
//! [`Selector`]. No other crate matches names.

use std::fmt;
use std::str::FromStr;

/// A name: dot-separated segments of letters, digits, `_`, and `-`. Names are
/// case-sensitive. A segment that starts with `@` is reserved for Foundation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Box<str>);

impl Name {
    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The segments in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    /// Reports whether `prefix` is this name or one of its ancestors, by whole
    /// segments: `site_a` is a prefix of `site_a.pt_1`, but `site` is not.
    #[must_use]
    pub fn starts_with(&self, prefix: &Name) -> bool {
        let _ = prefix;
        todo!()
    }

    /// Reports whether any segment is reserved for Foundation.
    #[must_use]
    pub fn reserved(&self) -> bool {
        todo!()
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Name {
    type Err = Error;

    /// Reads and checks a name. Reserved segments are allowed here; `spec` decides
    /// who may use them.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

/// One pattern over names: `*` matches one segment and `**` matches any number of
/// segments, including none.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pattern {
    _private: (),
}

impl Pattern {
    /// Reports whether the pattern matches `name`.
    #[must_use]
    pub fn matches(&self, name: &Name) -> bool {
        let _ = name;
        todo!()
    }

    /// How specific the pattern is. When several policies match one name, the most
    /// specific pattern wins.
    #[must_use]
    pub fn specificity(&self) -> Specificity {
        todo!()
    }
}

impl FromStr for Pattern {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let _ = s;
        todo!()
    }
}

/// An ordering of patterns by how specific they are. A greater value is more specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Specificity(u64);

/// A set of patterns. A name matches when an include pattern matches it and no
/// exclusion does. An exclusion is a pattern written with a leading `!`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Selector {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl Selector {
    /// Reads a selector from its patterns.
    ///
    /// # Errors
    ///
    /// The first pattern that does not read, or [`Error::Empty`] when no pattern
    /// includes a name.
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Self, Error> {
        drop(patterns);
        todo!()
    }

    /// The specificity of the most specific include pattern that matches `name`, or
    /// `None` when the selector does not match it.
    #[must_use]
    pub fn matches(&self, name: &Name) -> Option<Specificity> {
        let _ = (name, &self.include, &self.exclude);
        todo!()
    }
}

/// A name, pattern, or selector that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text is empty, or a selector includes nothing.
    Empty,
    /// A segment is empty or holds a character other than letters, digits, `_`, `-`,
    /// or a leading `@`.
    Segment {
        /// The whole text.
        input: String,
        /// The segment that is not valid.
        segment: String,
    },
    /// A wildcard appears in a name, or `**` is part of a longer segment.
    Wildcard {
        /// The whole text.
        input: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a name or selector is empty"),
            Self::Segment { input, segment } => write!(
                f,
                "{input:?} has a segment that is not valid: {segment:?}. Use letters, \
                 digits, `_`, and `-`, separated by dots"
            ),
            Self::Wildcard { input } => write!(
                f,
                "{input:?} uses a wildcard where it cannot. `*` and `**` must be whole \
                 segments of a pattern"
            ),
        }
    }
}

impl std::error::Error for Error {}

//! The key of a named reader.

use types::name::Name;

/// The key of a named reader: the subject that opens it and its name. Readers of the
/// same name and other subjects share nothing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// The subject that opens the reader.
    pub subject: Name,
    /// The reader's name.
    pub name: Name,
}

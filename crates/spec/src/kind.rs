//! The kinds of definition, and the tree key of each from its label.

use std::fmt;

use types::name::Name;

/// The kind of a definition. Its tree key is `<label>.@<kind>`, where `<kind>` is the
/// HCL keyword of the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// An access policy.
    Access,
    /// A connector.
    Connector,
    /// The record of a child region.
    Region,
    /// A node settings policy.
    NodeSettings,
}

impl Kind {
    /// The tree key of a definition of this kind with the label `label`.
    ///
    /// # Errors
    ///
    /// [`Error::Reserved`] when a segment of `label` starts with `@`, and
    /// [`Error::Long`] when the key would hold more than [`Name::MAX_BYTES`].
    #[expect(
        clippy::missing_panics_doc,
        clippy::unwrap_in_result,
        reason = "a checked label and a kind segment always make a name"
    )]
    pub fn key(self, label: &Name) -> Result<Name, Error> {
        if label.reserved() {
            return Err(Error::Reserved);
        }
        let segment = self.segment();
        let most = Name::MAX_BYTES
            .saturating_sub(segment.len())
            .saturating_sub(1);
        if label.as_str().len() > most {
            return Err(Error::Long { most });
        }
        Ok(format!("{label}.{segment}").parse().expect(
            "a short label that is not reserved and a kind segment make a name",
        ))
    }

    const fn segment(self) -> &'static str {
        match self {
            Self::Access => "@access",
            Self::Connector => "@connector",
            Self::Region => "@region",
            Self::NodeSettings => "@node_settings",
        }
    }
}

/// A label that makes no tree key. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A segment of the label starts with `@`.
    Reserved,
    /// The key would hold more than [`Name::MAX_BYTES`].
    Long {
        /// The most bytes a label of this kind holds.
        most: usize,
    },
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Reserved => "Remove the `@` from each segment",
            Self::Long { .. } => "Shorten the label to the bytes the message gives",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reserved => f.write_str("a segment of the label starts with `@`"),
            Self::Long { most } => write!(f, "the label is longer than {most} bytes"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const KINDS: [(Kind, &str); 4] = [
        (Kind::Access, "@access"),
        (Kind::Connector, "@connector"),
        (Kind::Region, "@region"),
        (Kind::NodeSettings, "@node_settings"),
    ];

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    #[test]
    fn appends_the_kind_segment() {
        for (kind, segment) in KINDS {
            let key = kind.key(&name("site_a.budget")).unwrap();
            assert_eq!(key, name(&format!("site_a.budget.{segment}")));
        }
    }

    #[test]
    fn refuses_a_reserved_label() {
        for (kind, _) in KINDS {
            assert_eq!(kind.key(&name("site_a.@changes")), Err(Error::Reserved));
        }
        assert_eq!(
            Error::Reserved.to_string(),
            "a segment of the label starts with `@`"
        );
        assert_eq!(Error::Reserved.fix(), "Remove the `@` from each segment");
    }

    #[test]
    fn bounds_the_label_by_the_key() {
        for (kind, segment) in KINDS {
            let most = Name::MAX_BYTES - segment.len() - 1;
            let fits = name(&"a".repeat(most));
            assert_eq!(kind.key(&fits).unwrap().as_str().len(), Name::MAX_BYTES);
            let long = name(&"a".repeat(most + 1));
            assert_eq!(kind.key(&long), Err(Error::Long { most }));
        }
        let error = Error::Long { most: 240 };
        assert_eq!(error.to_string(), "the label is longer than 240 bytes");
        assert_eq!(
            error.fix(),
            "Shorten the label to the bytes the message gives"
        );
    }

    proptest! {
        #[test]
        fn keys_a_label_under_it(
            segments in prop::collection::vec("[a-z0-9_-]{1,12}", 1..8),
            (kind, segment) in prop::sample::select(KINDS.to_vec()),
        ) {
            let label = name(&segments.join("."));
            let key = kind.key(&label).unwrap();
            prop_assert!(key.starts_with(&label));
            prop_assert_eq!(key.segments().last(), Some(segment));
            prop_assert_eq!(key.segments().count(), segments.len() + 1);
        }
    }
}

//! The kinds of definition, and the tree key of each from its label.

use std::fmt;

use types::name::{self, Name};

/// The kind of a definition. Its tree key is `<label>.@<kind>`, where `<kind>` is
/// [`Kind::as_str`].
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
    /// A compression policy.
    Compression,
    /// A placement policy.
    Placement,
    /// A time policy.
    Time,
}

impl Kind {
    /// The tree key of a definition of this kind with the label text `label`.
    ///
    /// # Errors
    ///
    /// The first that applies: [`Error::Long`] when the key would hold more than
    /// [`Name::MAX_BYTES`], [`Error::Name`] when `label` is not a name, and
    /// [`Error::Reserved`] when a segment of `label` starts with `@`.
    #[expect(
        clippy::missing_panics_doc,
        clippy::unwrap_in_result,
        reason = "a checked label and a kind segment always make a name"
    )]
    pub fn key(self, label: &str) -> Result<Name, Error> {
        let kind = self.as_str();
        let most = Name::MAX_BYTES.saturating_sub(kind.len()).saturating_sub(2);
        if label.len() > most {
            return Err(Error::Long { most });
        }
        let label: Name = label.parse().map_err(Error::Name)?;
        if label.reserved() {
            return Err(Error::Reserved);
        }
        Ok(format!("{label}.@{kind}").parse().expect(
            "a short label that is not reserved and a kind segment make a name",
        ))
    }

    /// The name of the kind, such as `node_settings`. A file format names the kind
    /// with it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Connector => "connector",
            Self::Region => "region",
            Self::NodeSettings => "node_settings",
            Self::Compression => "compression",
            Self::Placement => "placement",
            Self::Time => "time",
        }
    }
}

/// A label that makes no tree key. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The label is not a name. The message and the fix are those of the name error.
    Name(name::Error),
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
    pub fn fix(&self) -> String {
        match self {
            Self::Name(error) => error.fix().into(),
            Self::Reserved => "Remove the `@` from each segment".into(),
            Self::Long { most } => format!("Shorten the label to at most {most} bytes"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(error) => error.fmt(f),
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

    const KINDS: [(Kind, &str); 7] = [
        (Kind::Access, "@access"),
        (Kind::Connector, "@connector"),
        (Kind::Region, "@region"),
        (Kind::NodeSettings, "@node_settings"),
        (Kind::Compression, "@compression"),
        (Kind::Placement, "@placement"),
        (Kind::Time, "@time"),
    ];

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    #[test]
    fn names_each_kind_by_its_segment() {
        for (kind, segment) in KINDS {
            assert_eq!(format!("@{}", kind.as_str()), segment);
        }
    }

    #[test]
    fn appends_the_kind_segment() {
        for (kind, segment) in KINDS {
            let key = kind.key("site_a.budget").unwrap();
            assert_eq!(key, name(&format!("site_a.budget.{segment}")));
        }
    }

    #[test]
    fn refuses_a_reserved_label() {
        for (kind, _) in KINDS {
            assert_eq!(kind.key("site_a.@changes"), Err(Error::Reserved));
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
            let fits = "a".repeat(most);
            assert_eq!(kind.key(&fits).unwrap().as_str().len(), Name::MAX_BYTES);
            assert_eq!(kind.key(&"a".repeat(most + 1)), Err(Error::Long { most }));
            let past_a_name = "a".repeat(Name::MAX_BYTES + 1);
            assert_eq!(kind.key(&past_a_name), Err(Error::Long { most }));
        }
        let error = Error::Long { most: 240 };
        assert_eq!(error.to_string(), "the label is longer than 240 bytes");
        assert_eq!(error.fix(), "Shorten the label to at most 240 bytes");
    }

    #[test]
    fn refuses_a_label_that_is_not_a_name() {
        let error = "site_a..budget".parse::<Name>().unwrap_err();
        for (kind, _) in KINDS {
            assert_eq!(kind.key("site_a..budget"), Err(Error::Name(error.clone())));
        }
        let error = Error::Name(error);
        assert_eq!(
            error.to_string(),
            r#"a segment is not valid: "" in "site_a..budget""#
        );
        assert_eq!(
            error.fix(),
            "Use one or more ASCII letters, digits, `_`, and `-` in that segment, after \
             an optional leading `@`"
        );
    }

    #[test]
    fn checks_the_length_before_the_name() {
        let label = format!("{}.@x", "a".repeat(240));
        assert_eq!(
            Kind::NodeSettings.key(&label),
            Err(Error::Long { most: 240 })
        );
        assert_eq!(Kind::NodeSettings.key("@x"), Err(Error::Reserved));
    }

    proptest! {
        #[test]
        fn keys_a_label_under_it(
            segments in prop::collection::vec("[a-z0-9_-]{1,12}", 1..8),
            (kind, segment) in prop::sample::select(KINDS.to_vec()),
        ) {
            let label = segments.join(".");
            let key = kind.key(&label).unwrap();
            let label = name(&label);
            prop_assert!(key.starts_with(&label));
            prop_assert_eq!(key.segments().last(), Some(segment));
            prop_assert_eq!(key.segments().count(), segments.len() + 1);
        }
    }
}

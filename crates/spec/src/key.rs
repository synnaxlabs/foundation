//! The tree key of a definition: its label, then the segment of its kind.

use std::fmt;

use types::name::{self, Name};

use crate::definition::Kind;

impl Kind {
    /// The tree key of a definition of this kind with the label `label`:
    /// `<label>.@<kind>`, where `<kind>` is [`Kind::as_str`], or `label` itself for a
    /// connector, which is at its own name.
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
        let segment = match self {
            Self::Connector => None,
            _ => Some(self.as_str()),
        };
        let most = segment.map_or(Name::MAX_BYTES, |segment| {
            Name::MAX_BYTES - segment.len() - 2
        });
        if label.len() > most {
            return Err(Error::Long { most });
        }
        let label: Name = label.parse().map_err(Error::Name)?;
        if label.reserved() {
            return Err(Error::Reserved);
        }
        Ok(match segment {
            None => label,
            Some(segment) => format!("{label}.@{segment}").parse().expect(
                "a short label that is not reserved and a kind segment make a name",
            ),
        })
    }

    /// The name of the kind, such as `node_settings`: the keyword a file format names
    /// it with, and the segment of its tree key. A connector's key has no segment.
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
    /// A segment of the label starts with `@`.
    Reserved,
    /// The key would hold more than [`Name::MAX_BYTES`].
    Long {
        /// The most bytes a label of this kind holds.
        most: usize,
    },
    /// The label is not a name.
    Name(name::Error),
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub fn fix(&self) -> &'static str {
        match self {
            Self::Reserved => "Remove the `@` from each segment",
            Self::Long { .. } => {
                "Shorten the label to at most the bytes the message gives"
            }
            Self::Name(error) => error.fix(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reserved => f.write_str("a segment of the label starts with `@`"),
            Self::Long { most } => write!(f, "the label is longer than {most} bytes"),
            Self::Name(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Each kind with a segment.
    const KINDS: [(Kind, &str); 6] = [
        (Kind::Access, "@access"),
        (Kind::Region, "@region"),
        (Kind::NodeSettings, "@node_settings"),
        (Kind::Compression, "@compression"),
        (Kind::Placement, "@placement"),
        (Kind::Time, "@time"),
    ];

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    fn error(text: &str) -> name::Error {
        text.parse::<Name>().unwrap_err()
    }

    #[test]
    fn names_each_kind_by_its_segment() {
        for (kind, segment) in KINDS {
            assert_eq!(format!("@{}", kind.as_str()), segment);
        }
        assert_eq!(Kind::Connector.as_str(), "connector");
    }

    #[test]
    fn keys_a_connector_at_its_own_name() {
        let longest = "a".repeat(Name::MAX_BYTES);
        for label in ["site_a.modbus", &longest] {
            assert_eq!(Kind::Connector.key(label), Ok(name(label)));
        }
        assert_eq!(Kind::Connector.key("site_a.@modbus"), Err(Error::Reserved));
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
            let long = "a".repeat(most + 1);
            assert_eq!(kind.key(&long), Err(Error::Long { most }));
        }
        let error = Error::Long { most: 240 };
        assert_eq!(error.to_string(), "the label is longer than 240 bytes");
        assert_eq!(
            error.fix(),
            "Shorten the label to at most the bytes the message gives"
        );
    }

    #[test]
    fn checks_the_length_before_the_segments() {
        let label = format!("{}.@x", "a".repeat(240));
        assert_eq!(
            Kind::NodeSettings.key(&label),
            Err(Error::Long { most: 240 })
        );
    }

    #[test]
    fn checks_the_length_before_the_name() {
        let label = format!("{}.*", "a".repeat(254));
        assert_eq!(
            Kind::NodeSettings.key(&label),
            Err(Error::Long { most: 240 })
        );
        let label = format!("{}.*", "a".repeat(254));
        assert_eq!(Kind::Connector.key(&label), Err(Error::Long { most: 255 }));
    }

    #[test]
    fn refuses_a_label_that_is_not_a_name() {
        for label in ["", "site_a..budget", "site_a.*", "site a"] {
            for (kind, _) in KINDS {
                assert_eq!(kind.key(label), Err(Error::Name(error(label))));
            }
            assert_eq!(Kind::Connector.key(label), Err(Error::Name(error(label))));
        }
        let error = Error::Name(error("site_a.*"));
        assert_eq!(
            error.to_string(),
            "a wildcard is out of place: \"site_a.*\""
        );
        assert_eq!(
            error.fix(),
            "Use `*` and `**` only as whole segments of a pattern, never in a name"
        );
    }

    proptest! {
        #[test]
        fn keys_a_label_under_it(
            segments in prop::collection::vec("[a-z0-9_-]{1,12}", 1..8),
            (kind, segment) in prop::sample::select(KINDS.to_vec()),
        ) {
            let label = name(&segments.join("."));
            let key = kind.key(label.as_str()).unwrap();
            prop_assert!(key.starts_with(&label));
            prop_assert_eq!(key.segments().last(), Some(segment));
            prop_assert_eq!(key.segments().count(), segments.len() + 1);
        }
    }
}

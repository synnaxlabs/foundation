//! Definitions for the tests of a region.

use std::collections::BTreeMap;

use types::authority::Authority;
use types::channel::Key;
use types::ed25519::PublicKey;
use types::name::{Name, Prefix, Selector};
use types::sample::{self, Scalar};

use crate::access::{Action, Policy};
use crate::channel::{Channel, Data, Kind};
use crate::data_type::DataType;
use crate::definition::Definition;
use crate::region::Delegation;
use crate::subject::Subject;

pub(super) fn name(text: &str) -> Name {
    text.parse().unwrap()
}

pub(super) fn prefix(text: &str) -> Prefix {
    text.parse().unwrap()
}

pub(super) fn subject() -> Definition {
    Definition::Subject(Subject::new(vec![PublicKey::new([3; 32]).unwrap()]).unwrap())
}

pub(super) fn access() -> Definition {
    let select = |text| Selector::new([text]).unwrap();
    Definition::Access(Policy::new(
        select("@admin"),
        select("**"),
        [Action::Read].into_iter().collect(),
        Authority(0),
    ))
}

pub(super) fn record() -> Definition {
    Definition::Region(Delegation::new(1, [name("n1")]).unwrap())
}

pub(super) fn index(key: u128) -> Definition {
    Definition::Channel(Channel {
        key: Key::from_u128(key),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    })
}

pub(super) fn data(key: u128, index: u128) -> Definition {
    qualified(key, index, None)
}

/// A data channel whose quality channel is `quality`.
pub(super) fn qualified(key: u128, index: u128, quality: Option<u128>) -> Definition {
    let data = Data::new(
        Key::from_u128(index),
        quality.map(Key::from_u128),
        DataType::Sample(sample::Type::Scalar(Scalar::F64)),
        None,
    )
    .unwrap();
    Definition::Channel(Channel {
        key: Key::from_u128(key),
        kind: Kind::Data(data),
    })
}

pub(super) fn create_definitions(
    definitions: &[(&str, Definition)],
) -> BTreeMap<Name, Definition> {
    definitions
        .iter()
        .map(|(key, definition)| (name(key), definition.clone()))
        .collect()
}

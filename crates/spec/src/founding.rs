//! The definitions that Foundation makes at the first start of a mesh.

use std::collections::BTreeMap;

use types::authority::Authority;
use types::ed25519::PublicKey;
use types::name::{Name, Selector};

use crate::access::{Action, Policy};
use crate::definition::{Definition, Kind};
use crate::subject::Subject;

/// The label of the first admin's subject and of its access policy.
const ADMIN: &str = "@admin";

/// The kind and label of each founding definition. A later build can add an entry and
/// never removes one: a committed spec holds the keys that an earlier build made.
const LABELS: [(Kind, &str); 2] = [SUBJECT, ACCESS];

/// The first admin's subject.
const SUBJECT: (Kind, &str) = (Kind::Subject, ADMIN);

/// The first admin's access policy.
const ACCESS: (Kind, &str) = (Kind::Access, ADMIN);

/// The definitions that Foundation makes at the first start of a mesh, by tree key:
/// the subject `@admin`, which holds `admin`, and the access policy `@admin`, which
/// allows that subject every action on `**`, with no authority. No file can hold
/// them, because their labels are reserved. A later build can give other
/// definitions, so a node keeps what the first start gave.
#[must_use]
#[expect(
    clippy::missing_panics_doc,
    reason = "one key, `@admin`, `**`, a kind segment, and each action always read"
)]
pub fn create(admin: PublicKey) -> BTreeMap<Name, Definition> {
    let subject = Subject::new(vec![admin]).expect("invariant: one key is a subject");
    let policy = Policy::new(
        Selector::new([ADMIN]).expect("invariant: `@admin` is a pattern"),
        Selector::new(["**"]).expect("invariant: `**` is a pattern"),
        Action::ALL.into_iter().collect(),
        Authority(0),
    )
    .expect("invariant: each action is an action");
    BTreeMap::from([
        (key(SUBJECT), Definition::Subject(subject)),
        (key(ACCESS), Definition::Access(policy)),
    ])
}

/// Whether a founding definition of kind `kind` has the label `label`: a key that
/// [`create`] makes, or that an earlier build made.
pub(crate) fn holds(kind: Kind, label: &Name) -> bool {
    LABELS.contains(&(kind, label.as_str()))
}

fn key((kind, label): (Kind, &str)) -> Name {
    kind.join(
        label
            .parse()
            .expect("invariant: a founding label is a name"),
    )
}

#[cfg(test)]
mod tests {
    use types::name::Prefix;

    use super::*;
    use crate::region;

    fn admin() -> PublicKey {
        PublicKey::new([7; 32]).unwrap()
    }

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    #[test]
    fn makes_the_admin_subject_and_its_policy() {
        let founding = create(admin());
        let keys: Vec<&str> = founding.keys().map(Name::as_str).collect();
        assert_eq!(keys, ["@admin.@access", "@admin.@subject"]);
        let Some(Definition::Subject(subject)) = founding.get(&name("@admin.@subject"))
        else {
            panic!("no subject at `@admin.@subject`: {founding:?}");
        };
        assert_eq!(subject.keys(), [admin()]);
        let Some(Definition::Access(policy)) = founding.get(&name("@admin.@access"))
        else {
            panic!("no policy at `@admin.@access`: {founding:?}");
        };
        assert_eq!(policy.subjects(), &Selector::new(["@admin"]).unwrap());
        assert_eq!(policy.select(), &Selector::new(["**"]).unwrap());
        for action in Action::ALL {
            assert!(policy.allow().contains(action), "{action:?}");
        }
        assert_eq!(policy.authority(), Some(Authority(0)));
    }

    #[test]
    fn reads_each_definition_back_from_its_bytes() {
        for (key, definition) in create(admin()) {
            let decoded = Definition::decode(&definition.encode());
            assert_eq!(decoded, Ok(definition), "{key}");
        }
    }

    #[test]
    fn gives_definitions_that_no_file_can_hold() {
        for (key, definition) in create(admin()) {
            let label = definition.kind().label(&key);
            assert_eq!(label, Some(name(ADMIN)), "{key}");
            assert_eq!(
                definition.kind().key(ADMIN),
                Err(crate::key::Error::Reserved)
            );
        }
    }

    #[test]
    fn gives_a_root_region_with_no_problem() {
        assert_eq!(region::check(&Prefix::ROOT, &create(admin())), []);
    }
}

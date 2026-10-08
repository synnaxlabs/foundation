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

/// The definitions that Foundation makes at the first start of a mesh, by tree key:
/// the subject `@admin`, which holds `admin`, and the access policy `@admin`, which
/// allows that subject every action on `**`, with no authority. No file can hold
/// them, because their labels are reserved.
#[must_use]
#[expect(
    clippy::missing_panics_doc,
    reason = "one key, `@admin`, `**`, and a kind segment always read"
)]
pub fn create(admin: PublicKey) -> BTreeMap<Name, Definition> {
    let subject = Subject::new(vec![admin]).expect("invariant: one key is a subject");
    let policy = Policy::new(
        Selector::new([ADMIN]).expect("invariant: `@admin` is a pattern"),
        Selector::new(["**"]).expect("invariant: `**` is a pattern"),
        Action::ALL.into_iter().collect(),
        Authority(0),
    );
    BTreeMap::from([
        (key(Kind::Subject), Definition::Subject(subject)),
        (key(Kind::Access), Definition::Access(policy)),
    ])
}

fn key(kind: Kind) -> Name {
    format!("{ADMIN}.@{}", kind.as_str())
        .parse()
        .expect("invariant: `@admin` and a kind segment are a name")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

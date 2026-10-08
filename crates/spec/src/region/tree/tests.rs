use proptest::prelude::*;

use super::*;
use crate::region::common::{create_definitions, data, index, name, record, subject};

#[test]
fn equals_each_definition_set_on_the_empty_tree() {
    let definitions = create_definitions(&[
        ("plant.@region", record()),
        ("plant.@subject", subject()),
        ("plant.pressure", data(2, 1)),
        ("plant.time", index(1)),
    ]);
    let sets = definitions
        .iter()
        .map(|(key, definition)| Change::Set(key.clone(), definition.encode()));
    let expected = tree::apply(&mut Chunks::default(), tree::empty(), sets).unwrap();
    assert_eq!(tree(&mut Chunks::default(), &definitions), expected);
}

#[test]
fn gives_the_empty_tree_and_no_chunk_for_no_definitions() {
    let mut chunks = Chunks::default();
    assert_eq!(
        tree(&mut chunks, &BTreeMap::new()),
        Update {
            root: tree::empty(),
            chunks: Vec::new(),
        }
    );
}

#[test]
fn builds_definitions_with_a_problem() {
    let definitions = create_definitions(&[("plant.pressure", data(2, 9))]);
    let mut chunks = Chunks::default();
    let update = tree(&mut chunks, &definitions);
    assert_eq!(
        tree::get(&chunks, update.root, &name("plant.pressure")).unwrap(),
        Some(data(2, 9).encode().as_slice())
    );
}

proptest! {
    #[test]
    fn reads_each_definition_back_from_only_the_chunks_it_gives(
        labels in prop::collection::btree_set("[a-c]{1,2}(\\.[a-c]{1,2}){0,2}", 0..64),
    ) {
        let mut definitions = BTreeMap::new();
        for (at, label) in (1_u128..).zip(&labels) {
            definitions.insert(name(&format!("plant.{label}.@subject")), subject());
            definitions.insert(name(&format!("plant.{label}.time")), index(at));
        }
        let mut all = Chunks::default();
        let update = tree(&mut all, &definitions);
        let mut chunks = Chunks::default();
        for digest in &update.chunks {
            chunks.insert(all.get(*digest).unwrap().to_vec());
        }
        for (key, definition) in &definitions {
            let bytes = tree::get(&chunks, update.root, key).unwrap().unwrap();
            prop_assert_eq!(&Definition::decode(bytes).unwrap(), definition);
        }
    }
}

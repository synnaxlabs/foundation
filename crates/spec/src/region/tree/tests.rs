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

    #[test]
    fn definitions_of_the_tree_of_definitions_are_the_same_definitions(
        labels in prop::collection::btree_set("[a-c]{1,2}(\\.[a-c]{1,2}){0,2}", 0..64),
    ) {
        let mut defined = BTreeMap::new();
        for (at, label) in (1_u128..).zip(&labels) {
            defined.insert(name(&format!("plant.{label}.@subject")), subject());
            defined.insert(name(&format!("plant.{label}.p")), data(at + 1000, at));
            defined.insert(name(&format!("plant.{label}.time")), index(at));
        }
        let mut chunks = Chunks::default();
        let update = tree(&mut chunks, &defined);
        prop_assert_eq!(definitions(&chunks, update.root), Ok(defined));
    }
}

#[test]
fn definitions_of_the_empty_tree_are_none() {
    assert_eq!(
        definitions(&Chunks::default(), tree::empty()),
        Ok(BTreeMap::new())
    );
}

#[test]
fn definitions_without_a_chunk_of_the_tree_give_the_missing_chunk() {
    let defined = create_definitions(&[("plant.time", index(1))]);
    let update = tree(&mut Chunks::default(), &defined);
    let read = definitions(&Chunks::default(), update.root);
    let missing = Error::Tree(tree::Error::Missing(update.root));
    assert_eq!(read, Err(missing.clone()));
    assert_eq!(
        missing.to_string(),
        format!("chunk {} is not here", update.root)
    );
}

#[test]
fn definitions_with_a_value_that_does_not_decode_give_its_key() {
    let mut chunks = Chunks::default();
    let sets = [
        Change::Set(name("plant.time"), index(1).encode()),
        Change::Set(name("plant.x"), vec![255]),
    ];
    let update = tree::apply(&mut chunks, tree::empty(), sets).unwrap();
    let read = definitions(&chunks, update.root);
    let error = Error::Definition {
        key: name("plant.x"),
        error: definition::Error::Newer { found: 255 },
    };
    assert_eq!(read, Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "the value at `plant.x` is not a definition: the definition has format version \
         255, newer than 1"
    );
}

#[test]
fn definitions_of_a_chunk_that_is_not_a_tree_give_the_chunk() {
    let mut chunks = Chunks::default();
    let root = chunks.insert(vec![1]);
    assert_eq!(
        definitions(&chunks, root),
        Err(Error::Tree(tree::Error::Corrupt(root)))
    );
}

#[test]
fn definitions_give_definitions_with_a_problem() {
    let defined = create_definitions(&[
        ("plant.pressure", data(2, 9)),
        ("plant.time", subject()),
    ]);
    let mut chunks = Chunks::default();
    let update = tree(&mut chunks, &defined);
    assert_eq!(definitions(&chunks, update.root), Ok(defined));
}

#[test]
fn definitions_of_a_tree_cut_at_other_boundaries_give_its_root() {
    let leaf = |key: &str, value: Vec<u8>| {
        let mut bytes = vec![0, u8::try_from(key.len()).unwrap()];
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(u8::try_from(value.len()).unwrap());
        bytes.extend(value);
        bytes
    };
    let mut chunks = Chunks::default();
    let a = chunks.insert(leaf("p.a", index(1).encode()));
    let b = chunks.insert(leaf("p.b", index(2).encode()));
    let mut root = vec![1, 3];
    root.extend_from_slice(b"p.a");
    root.extend_from_slice(&a.0);
    root.push(3);
    root.extend_from_slice(b"p.b");
    root.extend_from_slice(&b.0);
    let root = chunks.insert(root);
    let corrupt = Error::Tree(tree::Error::Corrupt(root));
    assert_eq!(definitions(&chunks, root), Err(corrupt.clone()));
    assert_eq!(
        corrupt.to_string(),
        format!("chunk {root} is not a chunk of a spec tree")
    );
}

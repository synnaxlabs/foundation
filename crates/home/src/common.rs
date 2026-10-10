//! Test helpers that the modules of `home` reuse.

use std::ops::RangeInclusive;
use std::sync::Arc;

use proptest::prelude::*;
use types::channel::{self, Slot};
use types::frame::key_set::{Group, Interner, KeySet};
use types::sample::{Scalar, Sides, Type};

/// Each scalar.
pub(crate) const SCALARS: [Scalar; 14] = [
    Scalar::Bool,
    Scalar::I8,
    Scalar::I16,
    Scalar::I32,
    Scalar::I64,
    Scalar::U8,
    Scalar::U16,
    Scalar::U32,
    Scalar::U64,
    Scalar::F32,
    Scalar::F64,
    Scalar::Stamp,
    Scalar::Span,
    Scalar::Uuid,
];

/// Any type, with array and list lengths in `len` and matrix sides in `side`.
pub(crate) fn data_type(
    len: RangeInclusive<u32>,
    side: RangeInclusive<u16>,
) -> impl Strategy<Value = Type> {
    let scalar = || prop::sample::select(&SCALARS[..]);
    prop_oneof![
        scalar().prop_map(Type::Scalar),
        (scalar(), len.clone()).prop_map(|(element, len)| Type::Array { element, len }),
        (scalar(), side.clone(), side).prop_map(|(element, rows, columns)| {
            Type::Matrix {
                element,
                sides: Sides { rows, columns },
            }
        }),
        (scalar(), len).prop_map(|(element, max)| Type::List { element, max }),
        Just(Type::String),
        Just(Type::Bytes),
    ]
}

/// The raw values of one series: `count` samples of `data_type` from `state`. A
/// variable sample holds at most 5 elements. A `String` sample is ASCII.
pub(crate) fn values(state: u64, count: u32, data_type: Type) -> Vec<u8> {
    let count = usize::try_from(count).expect("a small count");
    match data_type {
        Type::String => variable(state, count, Scalar::U8, 5, 0x7f),
        Type::Bytes => variable(state, count, Scalar::U8, 5, 0xff),
        Type::List { element, max } => {
            variable(state, count, element, max.min(5), 0xff)
        }
        fixed => {
            let width = fixed.width().expect("a fixed width");
            elements(state, count * width, width)
        }
    }
}

/// The raw values of `count` variable samples of `element`, each of at most `max`
/// elements, each byte of which is cut by `mask`.
fn variable(
    mut state: u64,
    count: usize,
    element: Scalar,
    max: u32,
    mask: u8,
) -> Vec<u8> {
    let width = element.width();
    let lens: Vec<usize> = (0..count)
        .map(|_| {
            state = next(state);
            usize::try_from(state % (u64::from(max) + 1)).expect("small") * width
        })
        .collect();
    let elements: Vec<u8> = elements(state, lens.iter().sum(), width)
        .into_iter()
        .map(|byte| byte & mask)
        .collect();
    let samples: Vec<&[u8]> = lens
        .iter()
        .scan(0, |start, len| {
            *start += len;
            Some(&elements[*start - len..*start])
        })
        .collect();
    raw(Type::List { element, max }, &samples)
}

/// The raw form of `samples` of `data_type`, a `String`, `Bytes`, or `List` type.
pub(crate) fn raw(data_type: Type, samples: &[impl AsRef<[u8]>]) -> Vec<u8> {
    let form = codec::Variable::of(data_type).expect("a variable type");
    let mut out = vec![0; form.len(samples).expect("small samples")];
    form.write(samples, &mut out);
    out
}

/// `len` bytes from `state`, with runs of `width` bytes so that more than one
/// codec applies.
fn elements(mut state: u64, len: usize, width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = next(state);
        let run = usize::try_from(state % 5).expect("small") * width;
        let byte = u8::try_from(state >> 56).expect("one byte");
        out.extend(std::iter::repeat_n(byte, run.max(1)));
    }
    out.truncate(len);
    out
}

/// The xorshift step after `state`.
fn next(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

/// A pool of `budget` bytes on the heap.
pub(crate) fn create_pool(budget: usize) -> block::Pool {
    let budget = u64::try_from(budget).expect("a usize fits in a u64");
    let config = block::Config::new(budget).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

/// A key with the slot number in its first and last byte.
pub(crate) fn key(slot: Slot) -> channel::Key {
    let bits = u128::from(slot.get());
    channel::Key::from_u128(bits << 120 | bits)
}

/// An interner where `key(slot)` has `slot`, for each slot below 64: as an index for
/// each slot in `indexes`, else as a data channel of the type that `groups` gives its
/// key, or `I64`.
pub(crate) fn create_interner(indexes: &[u32], groups: &[Group<'_>]) -> Interner {
    let mut interner = Interner::new();
    for n in 0..64 {
        let key = key(Slot::new(n));
        if indexes.contains(&n) {
            interner.slots().index(key);
        } else {
            let data_type = groups
                .iter()
                .flat_map(|group| group.data)
                .find_map(|&(at, data_type)| (at == key).then_some(data_type));
            let data_type = data_type.unwrap_or(Type::Scalar(Scalar::I64));
            interner.slots().data(key, data_type);
        }
    }
    interner
}

/// The key set of `groups` from an interner where `key(slot)` has `slot`, for each
/// slot below 64, in its role in `groups`.
pub(crate) fn intern(groups: &[Group<'_>]) -> Arc<KeySet> {
    let indexes: Vec<u32> = (0..64)
        .filter(|&n| groups.iter().any(|group| group.index == key(Slot::new(n))))
        .collect();
    create_interner(&indexes, groups).intern(groups)
}

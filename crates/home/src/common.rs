//! Test helpers that the modules of `home` reuse.

use std::ops::RangeInclusive;

use proptest::prelude::*;
use types::channel::{self, Slot};
use types::frame::key_set::Interner;
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
        Type::String => {
            let mut values = variable(state, count, Scalar::U8, 5);
            values[4 * count..].iter_mut().for_each(|byte| *byte &= 0x7f);
            values
        }
        Type::Bytes => variable(state, count, Scalar::U8, 5),
        Type::List { element, max } => variable(state, count, element, max.min(5)),
        fixed => {
            let width = fixed.width().expect("a fixed width");
            elements(state, count * width, width)
        }
    }
}

/// The raw values of `count` variable samples of `element`, each of at most `max`
/// elements: their ends, zeros to the start of the elements, then the elements.
fn variable(mut state: u64, count: usize, element: Scalar, max: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut end = 0;
    for _ in 0..count {
        state = next(state);
        end += u32::try_from(state % (u64::from(max) + 1)).expect("small");
        out.extend(end.to_le_bytes());
    }
    out.resize(out.len().next_multiple_of(element.width().min(8)), 0);
    let width = element.width();
    let len = usize::try_from(end).expect("a small end") * width;
    out.extend(elements(state, len, width));
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
    let config = block::Config { budget };
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

/// A key with the slot number in its first and last byte.
pub(crate) fn key(slot: Slot) -> channel::Key {
    let bits = u128::from(slot.get());
    channel::Key::from_u128(bits << 120 | bits)
}

/// An interner where `key(slot)` has `slot`, for each slot below 64.
pub(crate) fn create_interner() -> Interner {
    let mut interner = Interner::new();
    for n in 0..64 {
        interner.slots().assign(key(Slot::new(n)));
    }
    interner
}

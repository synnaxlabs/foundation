//! Test helpers that the modules of `home` reuse.

use types::channel::{self, Slot};
use types::frame::key_set::Interner;
use types::sample::Scalar;

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

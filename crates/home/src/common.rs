//! Test helpers that the modules of `home` reuse.

/// A pool of `budget` bytes on the heap.
pub(crate) fn pool(budget: usize) -> block::Pool {
    let config = block::Config { budget };
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

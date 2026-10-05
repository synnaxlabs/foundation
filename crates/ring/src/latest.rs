//! A cell of words that one shard replaces and every shard reads with no lock.
//!
//! A sequence number is odd during an update. A reader that sees an odd number, or a
//! number that changed while it read, reads again. So a read never returns a torn
//! value, and a read that returns the old value ended before the update began.

use std::array;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};

use crate::sync::{Arc, AtomicU64, fence, spin_loop};

/// Makes a cell of `N` words that one [`Writer`] replaces and any thread reads with no
/// lock and no allocation.
#[must_use]
pub fn new<const N: usize>(value: [u64; N]) -> (Writer<N>, Reader<N>) {
    let shared = Arc::new(Shared {
        seq: AtomicU64::new(0),
        words: value.map(AtomicU64::new),
    });
    (
        Writer {
            shared: Arc::clone(&shared),
            value,
        },
        Reader { shared },
    )
}

struct Shared<const N: usize> {
    /// Odd during an update.
    seq: AtomicU64,
    words: [AtomicU64; N],
}

/// The one writer of a cell.
#[derive(Debug)]
pub struct Writer<const N: usize> {
    shared: Arc<Shared<N>>,
    value: [u64; N],
}

impl<const N: usize> Writer<N> {
    /// Replaces the value with `f(value)`. A read that overlaps the call runs again on
    /// the new value, so a read that returns the old value ended before `f` began: a
    /// clock reading inside `f` is later than one inside such a read. If `f` panics,
    /// the old value stays.
    pub fn update(&mut self, f: impl FnOnce([u64; N]) -> [u64; N]) {
        let seq = self.shared.seq.load(Relaxed);
        self.shared.seq.store(seq + 1, Relaxed);
        // SeqCst, not Release: the odd number must be visible before `f` reads a
        // clock, and a clock reading is not a memory operation.
        fence(SeqCst);
        let even = Even {
            seq: &self.shared.seq,
            next: seq + 2,
        };
        let value = f(self.value);
        for (word, &new) in self.shared.words.iter().zip(&value) {
            word.store(new, Relaxed);
        }
        self.value = value;
        drop(even);
    }
}

/// Ends an update when it drops, with or without a panic in the writer's closure.
struct Even<'a> {
    seq: &'a AtomicU64,
    next: u64,
}

impl Drop for Even<'_> {
    fn drop(&mut self) {
        self.seq.store(self.next, Release);
    }
}

/// Reads a cell. Clones read the same cell, from any thread.
#[derive(Clone, Debug)]
pub struct Reader<const N: usize> {
    shared: Arc<Shared<N>>,
}

impl<const N: usize> Reader<N> {
    /// Runs `f` on the newest value and returns its result. While an update is in
    /// progress, it waits, and when an update overlaps `f`, `f` runs again on the new
    /// value. So `f` must have no effect other than its result. Only the last result
    /// comes out.
    ///
    /// A clock reading inside `f` is no later than one inside an update that `f` did
    /// not see, when the reading is ordered with the loads around it (an ordered
    /// counter read, as the OS clock makes).
    pub fn read<R>(&self, mut f: impl FnMut([u64; N]) -> R) -> R {
        loop {
            let mut before = self.shared.seq.load(Acquire);
            while before & 1 == 1 {
                spin_loop();
                before = self.shared.seq.load(Acquire);
            }
            let value = array::from_fn(|index| self.shared.words[index].load(Relaxed));
            let result = f(value);
            // SeqCst, not Acquire: a clock reading in `f` is not a memory operation,
            // and the load must see an update whose clock reading came before it.
            fence(SeqCst);
            if self.shared.seq.load(Relaxed) == before {
                return result;
            }
        }
    }
}

impl<const N: usize> std::fmt::Debug for Shared<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("seq", &self.seq.load(Relaxed))
            .field("words", &self.words)
            .finish()
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::thread;

    use super::new;

    #[test]
    fn reads_the_first_value_before_any_update() {
        let (_writer, reader) = new([1, 2, 3]);
        assert_eq!(reader.read(|value| value), [1, 2, 3]);
    }

    #[test]
    fn reads_the_newest_value() {
        let (mut writer, reader) = new([0]);
        writer.update(|[count]| [count + 1]);
        writer.update(|[count]| [count + 1]);
        assert_eq!(reader.read(|[count]| count), 2);
    }

    #[test]
    fn reads_an_empty_cell() {
        let (mut writer, reader) = new([]);
        writer.update(|value| value);
        assert_eq!(reader.read(|[]| 7), 7);
    }

    #[test]
    fn keeps_the_old_value_when_the_update_panics() {
        let (mut writer, reader) = new([5]);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            writer.update(|_| panic!("the clock went back"));
        }));
        assert_eq!(
            outcome.unwrap_err().downcast_ref::<&str>(),
            Some(&"the clock went back")
        );
        assert_eq!(reader.read(|[value]| value), 5);
        writer.update(|[value]| [value + 1]);
        assert_eq!(reader.read(|[value]| value), 6);
    }

    #[test]
    fn debug_shows_the_sequence_and_the_words() {
        let (mut writer, reader) = new([7]);
        let shared = |seq: u64, word: u64| {
            format!("Reader {{ shared: Shared {{ seq: {seq}, words: [{word}] }} }}")
        };
        writer.update(|[word]| {
            assert_eq!(format!("{reader:?}"), shared(1, 7));
            [word + 1]
        });
        assert_eq!(format!("{reader:?}"), shared(2, 8));
        writer.update(|value| value);
        writer.update(|value| value);
        assert_eq!(format!("{reader:?}"), shared(6, 8));
        assert_eq!(
            format!("{writer:?}"),
            "Writer { shared: Shared { seq: 6, words: [8] }, value: [8] }"
        );
    }

    #[test]
    fn clones_read_on_another_thread() {
        let (mut writer, reader) = new([0, 0]);
        writer.update(|_| [1, 2]);
        let clone = reader.clone();
        thread::scope(|scope| {
            scope.spawn(move || assert_eq!(clone.read(|value| value), [1, 2]));
        });
        assert_eq!(reader.read(|value| value), [1, 2]);
    }
}

#[cfg(test)]
#[cfg(loom)]
mod model {
    use std::sync::atomic::Ordering::SeqCst;

    use loom::model::Builder;
    use loom::sync::Arc;
    use loom::sync::atomic::AtomicU64;
    use loom::thread;

    use super::new;

    /// Checks schedules with at most two forced thread switches. A reader that loom
    /// keeps on a stale word repeats its read, so the models need many branches.
    fn bounded(model: impl Fn() + Send + Sync + 'static) {
        let mut builder = Builder::new();
        builder.preemption_bound = Some(2);
        builder.max_branches = 20_000;
        builder.check(model);
    }

    #[test]
    fn never_reads_a_torn_or_stale_value() {
        bounded(|| {
            let (mut writer, reader) = new([0, 0]);
            let other = reader.clone();
            let reads = thread::spawn(move || other.read(|value| value));
            writer.update(|_| [1, 1]);
            let [first, second] = reads.join().unwrap();
            assert_eq!(first, second, "torn");
            assert_eq!(reader.read(|value| value), [1, 1]);
        });
    }

    /// A clock is a third thread that ticks. A clock reading is a `SeqCst` load of
    /// the tick: it synchronizes with nothing, as a real clock does not. The model
    /// fails when either fence is weaker than `SeqCst`.
    #[test]
    fn orders_an_old_read_before_the_update() {
        bounded(|| {
            let (mut writer, reader) = new([0]);
            let clock = Arc::new(AtomicU64::new(0));
            let ticks = {
                let clock = clock.clone();
                thread::spawn(move || {
                    clock.store(1, SeqCst);
                    clock.store(2, SeqCst);
                })
            };
            let reads = {
                let clock = clock.clone();
                thread::spawn(move || {
                    reader.read(|[value]| (value, clock.load(SeqCst)))
                })
            };
            let mut updated = 0;
            writer.update(|_| {
                updated = clock.load(SeqCst);
                [1]
            });
            let (value, read) = reads.join().unwrap();
            ticks.join().unwrap();
            if value == 0 {
                assert!(
                    read <= updated,
                    "an old value read at {read} after {updated}"
                );
            }
        });
    }
}

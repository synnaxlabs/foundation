//! The reads of one carrier that wait for a block from the shard's pool.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::task::Waker;

use block::{Pool, Unique};
use types::hash::Map;
use types::time::{Monotonic, Span};

use super::{connection, stream};
use crate::{Class, Status};

/// How long the reads that wait for a block wait before the first tries again.
pub(super) const RETRY: Span = Span::from_nanos(10 * Span::MILLISECOND.nanos());

/// The reads that found no block in the shard's pool, in the order they take one:
/// highest class first, then oldest first. A read tries for a block only in its
/// turn, so a later read cannot take the block that an earlier one waits for.
#[derive(Debug, Default)]
pub(super) struct Queue {
    /// The reads that wait for a block.
    reads: BTreeMap<Place, Read>,
    /// The place of each read that waits for a block.
    places: Map<stream::Key, Place>,
    tickets: u64,
    /// When the queue last went from empty to holding a read.
    since: Option<Monotonic>,
    /// The nanoseconds that the queue held a read, up to `since`.
    waited: i64,
    refusals: u64,
}

/// A read's class rank, then its ticket.
type Place = (usize, u64);

#[derive(Debug)]
struct Read {
    stream: stream::Key,
    waker: Waker,
}

impl Queue {
    /// A block of `len` bytes from `pool` for the read of `stream`, of `class`, when
    /// the read has its turn and the pool has room; the read then leaves the queue.
    /// Else `None`, and the read waits with `waker`, or keeps its place and takes
    /// `waker` when it waits. A refused commit counts.
    ///
    /// # Panics
    ///
    /// When `pool` cannot hold `len` bytes.
    pub(super) fn take(
        &mut self,
        now: Monotonic,
        pool: &Pool,
        stream: stream::Key,
        class: Class,
        len: usize,
        waker: &Waker,
    ) -> Option<Unique> {
        if self.turn(stream, class) {
            match pool.alloc(len) {
                Ok(block) => {
                    self.leave(now, stream);
                    return Some(block);
                }
                Err(block::Error::Exhausted { .. }) => {}
                Err(block::Error::Refused { .. }) => self.refusals += 1,
                Err(error @ block::Error::TooLarge { .. }) => {
                    panic!("the pool cannot hold a message of `bytes_max`: {error}")
                }
            }
        }
        self.wait(now, stream, class, waker);
        None
    }

    /// Whether the read of `stream`, of `class`, may try for a block now: no read
    /// before its place waits. A read with no place goes after each read of its
    /// class.
    fn turn(&self, stream: stream::Key, class: Class) -> bool {
        let place =
            (self.places.get(&stream).copied()).unwrap_or((class.rank(), u64::MAX));
        self.reads.keys().next().is_none_or(|&first| first >= place)
    }

    /// Makes the read of `stream`, of `class`, wait with `waker`, or keeps its place
    /// when it waits.
    fn wait(
        &mut self,
        now: Monotonic,
        stream: stream::Key,
        class: Class,
        waker: &Waker,
    ) {
        if self.reads.is_empty() {
            self.since = Some(now);
        }
        let Self {
            places, tickets, ..
        } = self;
        let place = *places.entry(stream).or_insert_with(|| {
            *tickets += 1;
            (class.rank(), *tickets)
        });
        match self.reads.entry(place) {
            Entry::Occupied(mut read) => read.get_mut().waker.clone_from(waker),
            Entry::Vacant(read) => {
                read.insert(Read {
                    stream,
                    waker: waker.clone(),
                });
            }
        }
    }

    /// Ends the wait of the read of `stream`, if it waits. When it waited first, the
    /// next read gets its turn now.
    pub(super) fn leave(&mut self, now: Monotonic, stream: stream::Key) {
        // Each read leaves, so skip the hash when none waits.
        if self.places.is_empty() {
            return;
        }
        if let Some(place) = self.places.remove(&stream) {
            self.remove(now, place);
        }
    }

    /// Removes the read at `place` from the reads that wait for a block.
    fn remove(&mut self, now: Monotonic, place: Place) {
        let first = self.reads.keys().next() == Some(&place);
        self.reads.remove(&place);
        self.settle(now, first);
    }

    /// Ends the wait of each read of `connection`, and wakes each that waits for a
    /// block.
    pub(super) fn end(&mut self, now: Monotonic, connection: connection::Key) {
        self.places
            .retain(|stream, _| stream.connection != connection);
        let first = self.reads.keys().next().copied();
        self.reads.retain(|_, read| {
            let ends = read.stream.connection == connection;
            if ends {
                read.waker.wake_by_ref();
            }
            !ends
        });
        let moved = self.reads.keys().next().copied() != first;
        self.settle(now, moved);
    }

    /// Wakes the read that waits first, so it tries again.
    pub(super) fn wake_first(&self) {
        if let Some(read) = self.reads.values().next() {
            read.waker.wake_by_ref();
        }
    }

    /// Whether a read waits.
    pub(super) fn waiting(&self) -> bool {
        !self.reads.is_empty()
    }

    /// The counts of the reads up to `now`, with no budget waits: the endpoint counts
    /// those.
    pub(super) fn status(&self, now: Monotonic) -> Status {
        let open = self.since.map_or(0, |since| (now - since).nanos());
        Status {
            waited: Span::from_nanos(self.waited.saturating_add(open)),
            refusals: self.refusals,
            budget_waits: 0,
        }
    }

    /// After reads left: wakes the new first read when the first left, and stops the
    /// wait time when none waits.
    fn settle(&mut self, now: Monotonic, first_left: bool) {
        if first_left {
            self.wake_first();
        }
        if self.reads.is_empty()
            && let Some(since) = self.since.take()
        {
            self.waited = self.waited.saturating_add((now - since).nanos());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    use block::testing::Scarce;
    use block::{Config, Heap, Pool};
    use noq_proto::{ConnectionHandle, Dir, Side, StreamId};
    use types::time::{Monotonic, Span};

    use super::Queue;
    use crate::quic::{connection, stream};
    use crate::{Class, Status};

    /// Counts its wakes.
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A waker, and its count of wakes.
    fn waker() -> (Waker, Arc<Count>) {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        (Waker::from(Arc::clone(&count)), count)
    }

    fn woken(count: &Count) -> usize {
        count.0.load(Ordering::Relaxed)
    }

    /// Stream `index` of connection `serial`.
    fn stream(serial: u64, index: u64) -> stream::Key {
        stream::Key {
            connection: connection(serial),
            id: StreamId::new(Side::Client, Dir::Uni, index),
        }
    }

    fn connection(serial: u64) -> connection::Key {
        connection::Key {
            handle: ConnectionHandle(0),
            serial,
        }
    }

    fn at(millis: u64) -> Monotonic {
        Monotonic(millis * 1_000_000)
    }

    fn millis(n: i64) -> Span {
        Span::from_nanos(n * Span::MILLISECOND.nanos())
    }

    fn pool(budget: usize) -> Pool {
        let config = Config { budget };
        let memory = Heap::new(config.reservation());
        Pool::new(config, memory)
    }

    #[test]
    fn a_take_in_turn_gives_a_block_and_leaves_the_queue() {
        let pool = pool(1 << 16);
        let mut queue = Queue::default();
        let (first, count) = waker();
        let (next, next_count) = waker();
        queue.wait(at(0), stream(0, 0), Class::Complete, &first);
        queue.wait(at(0), stream(0, 1), Class::Complete, &next);
        let block =
            queue.take(at(1), &pool, stream(0, 0), Class::Complete, 100, &first);
        assert_eq!(block.map(|block| block.len()), Some(100));
        assert_eq!([woken(&count), woken(&next_count)], [0, 1]);
        assert!(queue.turn(stream(0, 1), Class::Complete));
    }

    #[test]
    fn a_take_out_of_turn_takes_no_block_and_waits_behind() {
        let pool = pool(1 << 16);
        let mut queue = Queue::default();
        let (waker, _) = waker();
        queue.wait(at(0), stream(0, 0), Class::Command, &waker);
        let block =
            queue.take(at(1), &pool, stream(0, 1), Class::Complete, 100, &waker);
        assert!(block.is_none());
        assert_eq!(pool.committed(), 0);
        queue.leave(at(2), stream(0, 0));
        assert!(queue.turn(stream(0, 1), Class::Complete));
    }

    #[test]
    fn a_take_with_a_full_pool_waits_then_takes_the_block_that_frees() {
        // A 100-byte block takes 192 bytes of the budget.
        let pool = pool(300);
        let held = pool.alloc(100).expect("room");
        let mut queue = Queue::default();
        let (waker, _) = waker();
        let read = (stream(0, 0), Class::Complete);
        assert!(
            queue
                .take(at(0), &pool, read.0, read.1, 100, &waker)
                .is_none()
        );
        assert!(queue.waiting());
        drop(held);
        assert!(
            queue
                .take(at(1), &pool, read.0, read.1, 100, &waker)
                .is_some()
        );
        assert!(!queue.waiting());
        assert_eq!(queue.status(at(2)).refusals, 0);
    }

    #[test]
    fn the_wait_time_stops_when_the_last_read_leaves() {
        let mut queue = Queue::default();
        let (waker, _) = waker();
        queue.wait(at(0), stream(0, 0), Class::Complete, &waker);
        queue.leave(at(2), stream(0, 0));
        queue.leave(at(3), stream(0, 1));
        assert!(!queue.waiting());
        assert_eq!(queue.status(at(5)).waited, millis(2));
    }

    #[test]
    #[should_panic(expected = "the pool cannot hold a message of `bytes_max`")]
    fn a_take_over_what_the_pool_holds_panics() {
        let pool = pool(300);
        let (waker, _) = waker();
        let read = (stream(0, 0), Class::Complete);
        drop(Queue::default().take(at(0), &pool, read.0, read.1, 1_000, &waker));
    }

    #[test]
    fn with_no_wait_each_read_has_its_turn() {
        let queue = Queue::default();
        assert!(queue.turn(stream(0, 0), Class::CatchUp));
        assert!(!queue.waiting());
    }

    #[test]
    fn a_read_that_does_not_wait_has_its_turn_only_above_each_wait() {
        let mut queue = Queue::default();
        let (waker, _) = waker();
        queue.wait(at(0), stream(0, 0), Class::Complete, &waker);
        assert!(queue.waiting());
        assert!(queue.turn(stream(0, 0), Class::Complete));
        assert!(queue.turn(stream(0, 1), Class::Command));
        assert!(queue.turn(stream(0, 1), Class::Latest));
        assert!(!queue.turn(stream(0, 1), Class::Complete));
        assert!(!queue.turn(stream(0, 1), Class::CatchUp));
    }

    #[test]
    fn reads_take_turns_highest_class_first_then_oldest() {
        let mut queue = Queue::default();
        let reads = [
            (stream(0, 0), Class::CatchUp),
            (stream(0, 1), Class::Complete),
            (stream(1, 0), Class::Complete),
            (stream(0, 2), Class::Command),
        ];
        let wakers = reads.map(|_| waker());
        for ((stream, class), (waker, _)) in reads.iter().zip(&wakers) {
            queue.wait(at(0), *stream, *class, waker);
        }
        let mut waiting = vec![0, 1, 2, 3];
        let mut order = Vec::new();
        while queue.waiting() {
            let turns: Vec<_> = waiting
                .iter()
                .copied()
                .filter(|&i| queue.turn(reads[i].0, reads[i].1))
                .collect();
            let [first] = turns[..] else {
                panic!("one turn, not {turns:?}")
            };
            order.push(first);
            waiting.retain(|&i| i != first);
            queue.leave(at(0), reads[first].0);
        }
        assert_eq!(order, [3, 1, 2, 0]);
        let counts = wakers.each_ref().map(|(_, count)| woken(count));
        assert_eq!(counts, [1, 1, 1, 0]);
    }

    #[test]
    fn a_read_that_waits_again_keeps_its_place_and_takes_the_new_waker() {
        let mut queue = Queue::default();
        let (old, old_count) = waker();
        let (new, new_count) = waker();
        let (other, other_count) = waker();
        queue.wait(at(0), stream(0, 0), Class::Complete, &old);
        queue.wait(at(1), stream(0, 1), Class::Complete, &other);
        queue.wait(at(2), stream(0, 0), Class::Complete, &new);
        assert!(queue.turn(stream(0, 0), Class::Complete));
        queue.wake_first();
        let wakes = [&old_count, &new_count, &other_count].map(|count| woken(count));
        assert_eq!(wakes, [0, 1, 0]);
    }

    #[test]
    fn a_read_that_leaves_behind_the_first_wakes_none() {
        let mut queue = Queue::default();
        let (first, first_count) = waker();
        let (second, second_count) = waker();
        queue.wait(at(0), stream(0, 0), Class::Command, &first);
        queue.wait(at(0), stream(0, 1), Class::Complete, &second);
        queue.leave(at(0), stream(0, 1));
        queue.leave(at(0), stream(0, 2));
        assert_eq!([woken(&first_count), woken(&second_count)], [0, 0]);
        assert!(queue.turn(stream(0, 0), Class::Command));
        assert!(!queue.turn(stream(0, 1), Class::Complete));
    }

    #[test]
    fn an_end_wakes_and_drops_each_read_of_the_connection_and_wakes_the_next() {
        let mut queue = Queue::default();
        let (ended, ended_count) = waker();
        let (also, also_count) = waker();
        let (kept, kept_count) = waker();
        queue.wait(at(0), stream(0, 0), Class::Command, &ended);
        queue.wait(at(0), stream(1, 0), Class::Complete, &kept);
        queue.wait(at(0), stream(0, 1), Class::CatchUp, &also);
        queue.end(at(0), connection(0));
        let wakes = [&ended_count, &also_count, &kept_count].map(|count| woken(count));
        assert_eq!(wakes, [1, 1, 1]);
        assert!(queue.turn(stream(1, 0), Class::Complete));
        assert!(!queue.turn(stream(0, 1), Class::Complete));
        assert!(queue.turn(stream(0, 0), Class::Command));
        queue.leave(at(0), stream(1, 0));
        assert!(!queue.waiting());
    }

    #[test]
    fn the_status_counts_the_time_a_read_waits_and_each_refused_take() {
        let config = Config { budget: 1 << 16 };
        let (memory, switch) = Scarce::new(config.reservation());
        let pool = Pool::new(config, memory);
        let mut queue = Queue::default();
        let (waker, _) = waker();
        let take = |queue: &mut Queue, millis, index| {
            let read = stream(0, index);
            queue.take(at(millis), &pool, read, Class::Complete, 9_000, &waker)
        };
        let none = Status {
            waited: Span::ZERO,
            refusals: 0,
            budget_waits: 0,
        };
        assert_eq!(queue.status(at(5)), none);
        switch.refuse();
        assert!(take(&mut queue, 10, 0).is_none());
        assert!(take(&mut queue, 12, 1).is_none());
        assert!(take(&mut queue, 14, 0).is_none());
        let status = Status {
            waited: millis(5),
            refusals: 2,
            budget_waits: 0,
        };
        assert_eq!(queue.status(at(15)), status);
        switch.allow();
        assert!(take(&mut queue, 20, 0).is_some());
        assert!(take(&mut queue, 30, 1).is_some());
        let status = Status {
            waited: millis(20),
            refusals: 2,
            budget_waits: 0,
        };
        assert_eq!(queue.status(at(40)), status);
        queue.wait(at(50), stream(0, 2), Class::Command, &waker);
        queue.end(at(53), connection(0));
        let status = Status {
            waited: millis(23),
            refusals: 2,
            budget_waits: 0,
        };
        assert_eq!(queue.status(at(60)), status);
    }
}

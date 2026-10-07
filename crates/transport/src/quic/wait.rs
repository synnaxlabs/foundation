//! The reads of one carrier that wait for a block from the shard's pool.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::task::Waker;

use types::hash::Map;
use types::time::{Monotonic, Span};

use super::{connection, stream};
use crate::message::Miss;
use crate::{Class, Status};

/// How long the reads that wait for a block wait before the first tries again.
pub(super) const RETRY: Span = Span::from_nanos(10 * Span::MILLISECOND.nanos());

/// The reads that found no block in the shard's pool, in the order they take one:
/// highest class first, then oldest first. A read tries for a block only in its
/// turn, so a later read cannot take the block that an earlier one waits for.
#[derive(Debug, Default)]
pub(super) struct Queue {
    reads: BTreeMap<Place, Read>,
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
    /// Whether the read of `stream`, of `class`, may try for a block now: it waits
    /// first, or it does not wait and no read of its class or a higher one waits.
    pub(super) fn turn(&self, stream: stream::Key, class: Class) -> bool {
        let first = self.reads.keys().next();
        match self.places.get(&stream) {
            Some(place) => first == Some(place),
            None => first.is_none_or(|&(rank, _)| rank > class.rank()),
        }
    }

    /// Makes the read of `stream`, of `class`, wait with `waker`, or keeps its place
    /// when it waits. `miss` is why its try failed, when it tried.
    pub(super) fn wait(
        &mut self,
        now: Monotonic,
        stream: stream::Key,
        class: Class,
        miss: Option<Miss>,
        waker: &Waker,
    ) {
        if miss == Some(Miss::Refused) {
            self.refusals += 1;
        }
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
        if let Some(place) = self.places.remove(&stream) {
            self.remove(now, |read_place, _| *read_place == place);
        }
    }

    /// Ends the wait of each read of `connection`, and wakes it.
    pub(super) fn end(&mut self, now: Monotonic, connection: connection::Key) {
        self.places
            .retain(|stream, _| stream.connection != connection);
        self.remove(now, |_, read| {
            let ends = read.stream.connection == connection;
            if ends {
                read.waker.wake_by_ref();
            }
            ends
        });
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

    /// The counts up to `now`.
    pub(super) fn status(&self, now: Monotonic) -> Status {
        let open = self.since.map_or(0, |since| (now - since).nanos());
        Status {
            waited: Span::from_nanos(self.waited.saturating_add(open)),
            refusals: self.refusals,
        }
    }

    /// Removes each read that `ends` picks. Wakes the new first read when the first
    /// left, and stops the wait time when none waits.
    fn remove(&mut self, now: Monotonic, mut ends: impl FnMut(&Place, &Read) -> bool) {
        let first = self.reads.keys().next().copied();
        self.reads.retain(|place, read| !ends(place, read));
        if self.reads.keys().next().copied() != first {
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

    use noq_proto::{ConnectionHandle, Dir, Side, StreamId};
    use types::time::{Monotonic, Span};

    use super::Queue;
    use crate::message::Miss;
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
        queue.wait(at(0), stream(0, 0), Class::Complete, None, &waker);
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
            queue.wait(at(0), *stream, *class, None, waker);
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
        queue.wait(at(0), stream(0, 0), Class::Complete, None, &old);
        queue.wait(at(1), stream(0, 1), Class::Complete, None, &other);
        queue.wait(at(2), stream(0, 0), Class::Complete, None, &new);
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
        queue.wait(at(0), stream(0, 0), Class::Command, None, &first);
        queue.wait(at(0), stream(0, 1), Class::Complete, None, &second);
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
        queue.wait(at(0), stream(0, 0), Class::Command, None, &ended);
        queue.wait(at(0), stream(1, 0), Class::Complete, None, &kept);
        queue.wait(at(0), stream(0, 1), Class::CatchUp, None, &also);
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
    fn the_status_counts_the_time_a_read_waits_and_each_refused_try() {
        let mut queue = Queue::default();
        let (waker, _) = waker();
        let none = Status {
            waited: Span::ZERO,
            refusals: 0,
        };
        assert_eq!(queue.status(at(5)), none);
        queue.wait(
            at(10),
            stream(0, 0),
            Class::Complete,
            Some(Miss::Refused),
            &waker,
        );
        queue.wait(
            at(12),
            stream(0, 1),
            Class::Complete,
            Some(Miss::Exhausted),
            &waker,
        );
        queue.wait(
            at(14),
            stream(0, 0),
            Class::Complete,
            Some(Miss::Refused),
            &waker,
        );
        let status = Status {
            waited: millis(5),
            refusals: 2,
        };
        assert_eq!(queue.status(at(15)), status);
        queue.leave(at(20), stream(0, 0));
        queue.leave(at(30), stream(0, 1));
        let status = Status {
            waited: millis(20),
            refusals: 2,
        };
        assert_eq!(queue.status(at(40)), status);
        queue.wait(at(50), stream(0, 2), Class::Command, None, &waker);
        queue.end(at(53), connection(0));
        let status = Status {
            waited: millis(23),
            refusals: 2,
        };
        assert_eq!(queue.status(at(60)), status);
    }
}

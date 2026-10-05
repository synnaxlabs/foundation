//! The datagrams of one connection: whole messages, each in one QUIC DATAGRAM
//! frame, that may be lost.

use std::collections::VecDeque;

use block::{Block, Pool};
use bytes::Bytes;

use super::connection::{self, Connection};
use super::{Body, Event, queue};
use crate::{Error, message};

/// The most datagrams that wait untaken on one connection.
const WAITING_MAX: usize = 64;

/// The datagrams of one connected connection.
pub(crate) struct Datagrams<'a> {
    connection: &'a mut Connection,
    /// The endpoint's queue for [`Endpoint::transmit`](super::Endpoint::transmit).
    ready: &'a mut VecDeque<connection::Key>,
}

impl<'a> Datagrams<'a> {
    pub(super) fn new(
        connection: &'a mut Connection,
        ready: &'a mut VecDeque<connection::Key>,
    ) -> Self {
        Self { connection, ready }
    }

    /// Queues `message` as one datagram. It never waits: when the queue is full, the
    /// oldest unsent datagram drops.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when `message` is over [`Datagrams::bytes_max`], or the
    /// peer takes no datagrams.
    #[expect(
        clippy::unwrap_in_result,
        reason = "datagrams are on, the size is checked, and a send that drops never \
                  blocks"
    )]
    pub(crate) fn send(&mut self, message: Block) -> Result<(), Error> {
        let bytes = message.len();
        let mut datagrams = self.connection.inner.datagrams();
        match datagrams.max_size() {
            Some(bytes_max) if bytes <= bytes_max => {}
            bytes_max => {
                let bytes_max = bytes_max.unwrap_or(0);
                return Err(Error::TooLarge { bytes, bytes_max });
            }
        }
        datagrams
            .send(Bytes::from_owner(Body(message)), true)
            .expect("invariant: noq-proto takes a datagram that fits");
        queue(self.ready, self.connection);
        Ok(())
    }

    /// The oldest datagram that arrived and was not taken.
    pub(crate) fn receive(&mut self) -> Option<Block> {
        self.connection.received.0.pop_front()
    }

    /// The largest datagram [`Datagrams::send`] takes now. It changes with the path,
    /// and is 0 when the peer takes no datagrams.
    pub(crate) fn bytes_max(&mut self) -> usize {
        self.connection.inner.datagrams().max_size().unwrap_or(0)
    }
}

/// The datagrams that arrived on one connection and wait to be taken, oldest first.
#[derive(Default)]
pub(super) struct Received(VecDeque<Block>);

impl Received {
    /// Moves each datagram that `inner` holds into a block from `pool`, and gives
    /// [`Event::Datagram`] when none waited. A datagram drops when it gets no block
    /// (`pool` or the system has no room). When [`WAITING_MAX`] wait, the oldest
    /// drops.
    pub(super) fn pull(
        &mut self,
        inner: &mut noq_proto::Connection,
        pool: &Pool,
        key: connection::Key,
        events: &mut VecDeque<Event>,
    ) {
        let waited = !self.0.is_empty();
        let mut datagrams = inner.datagrams();
        while let Some(datagram) = datagrams.recv() {
            if self.0.len() == WAITING_MAX {
                self.0.pop_front();
            }
            let Ok(mut block) = message::alloc(pool, datagram.len()) else {
                continue;
            };
            block.copy_from_slice(&datagram);
            self.0.push_back(block.freeze());
        }
        if !waited && !self.0.is_empty() {
            events.push_back(Event::Datagram { key });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::num::NonZeroUsize;
    use std::rc::Rc;
    use std::time::Duration;

    use block::Heap;
    use types::time::Span;

    use super::*;
    use crate::quic::Endpoint;
    use crate::quic::testing::{self, Pair, Side};
    use crate::{Code, Config, tls};

    /// The link delay each way.
    const DELAY: Duration = Duration::from_millis(10);
    /// Long enough for the datagrams sent before it to arrive.
    const RUN: Duration = Duration::from_millis(100);

    /// A connected pair whose server has `config`.
    fn dial_with(shard: &testing::Shard, config: &Config) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.server.endpoint =
            Endpoint::new(config, testing::SERVER_SHARD, NonZeroUsize::MIN);
        pair.dial(tls::public(&testing::SERVER_KEY));
        pair.run(RUN);
        pair
    }

    fn dial(shard: &testing::Shard) -> Pair {
        dial_with(shard, &shard.config(testing::SERVER_KEY, Span::SECOND))
    }

    /// A pool with room for one block of 100 bytes and not two.
    fn small() -> Rc<Pool> {
        // A 100-byte block takes 192 bytes of the budget.
        let config = block::Config { budget: 300 };
        let memory = Heap::new(config.reservation());
        Rc::new(Pool::new(config, memory))
    }

    /// A server config that takes messages from `pool`.
    fn with_pool(shard: &testing::Shard, pool: &Rc<Pool>) -> Config {
        Config {
            pool: Rc::clone(pool),
            ..shard.config(testing::SERVER_KEY, Span::SECOND)
        }
    }

    fn datagrams(side: &mut Side) -> Datagrams<'_> {
        let key = side.key.expect("a connection");
        side.endpoint.datagrams(key).expect("connected")
    }

    /// Takes every datagram that waits on `side`.
    fn take(side: &mut Side) -> Vec<Vec<u8>> {
        let mut datagrams = datagrams(side);
        iter::from_fn(|| datagrams.receive().map(|block| block.to_vec())).collect()
    }

    /// How many [`Event::Datagram`] `side` got.
    fn arrivals(side: &Side) -> usize {
        let events = side.events.iter();
        let datagrams =
            events.filter(|(_, event)| matches!(event, Event::Datagram { .. }));
        datagrams.count()
    }

    type Sides = fn(&mut Pair) -> (&mut Side, &mut Side);

    fn forward(pair: &mut Pair) -> (&mut Side, &mut Side) {
        (&mut pair.client, &mut pair.server)
    }

    fn back(pair: &mut Pair) -> (&mut Side, &mut Side) {
        (&mut pair.server, &mut pair.client)
    }

    #[test]
    fn arrive_whole_in_one_block_with_one_event_each() {
        testing::run(1, |shard| {
            let directions: [Sides; 2] = [forward, back];
            for sides in directions {
                let mut pair = dial(shard);
                let full: Vec<u8> = (0..=u8::MAX).cycle().take(1_000).collect();
                for message in [full, Vec::new()] {
                    let (sender, _) = sides(&mut pair);
                    datagrams(sender).send(shard.block(&message)).expect("sent");
                    pair.run(RUN);
                    let (_, receiver) = sides(&mut pair);
                    assert_eq!(take(receiver), [message]);
                }
                let (_, receiver) = sides(&mut pair);
                let key = receiver.key.expect("a connection");
                let events = receiver.events[1..].iter().map(|(_, event)| event);
                let datagram = Event::Datagram { key };
                assert_eq!(events.collect::<Vec<_>>(), [&datagram, &datagram]);
            }
        });
    }

    #[test]
    fn take_up_to_the_path_limit() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.dial(tls::public(&testing::SERVER_KEY));
            // The client connects at 47 ms, after a retry. Its first MTU probe
            // returns at 67 ms.
            pair.run(Duration::from_millis(50));
            let mut client = datagrams(&mut pair.client);
            // The first MTU, less a short header (1 + 8 + 4 + 16) and the frame
            // header (9).
            assert_eq!(client.bytes_max(), 1_162);
            let over = client.send(shard.block(&[1; 1_163]));
            let too_large = Error::TooLarge {
                bytes: 1_163,
                bytes_max: 1_162,
            };
            assert_eq!(over, Err(too_large));
            client.send(shard.block(&[2; 1_162])).expect("sent");
            pair.run(RUN);
            assert_eq!(take(&mut pair.server), [vec![2; 1_162]]);
        });
    }

    #[test]
    fn grow_with_the_path() {
        testing::run(1, |shard| {
            let mut pair = dial(shard);
            pair.run(Duration::from_secs(1));
            // The largest MTU that discovery tries, less the same headers.
            assert_eq!(datagrams(&mut pair.client).bytes_max(), 1_414);
            assert_eq!(datagrams(&mut pair.server).bytes_max(), 1_414);
        });
    }

    #[test]
    fn take_up_to_a_smaller_peer_limit_less_the_frame_header() {
        testing::run(1, |shard| {
            let config = Config {
                message_bytes_max: NonZeroUsize::new(500).expect("not zero"),
                ..shard.config(testing::SERVER_KEY, Span::SECOND)
            };
            let mut pair = dial_with(shard, &config);
            let mut client = datagrams(&mut pair.client);
            assert_eq!(client.bytes_max(), 491);
            let over = client.send(shard.block(&[1; 492]));
            let too_large = Error::TooLarge {
                bytes: 492,
                bytes_max: 491,
            };
            assert_eq!(over, Err(too_large));
            client.send(shard.block(&[2; 491])).expect("sent");
            pair.run(RUN);
            assert_eq!(take(&mut pair.server), [vec![2; 491]]);
        });
    }

    #[test]
    fn a_full_send_queue_drops_the_oldest() {
        testing::run(1, |shard| {
            let mut pair = dial(shard);
            let mut client = datagrams(&mut pair.client);
            for byte in 0..80 {
                client.send(shard.block(&[byte; 1_100])).expect("sent");
            }
            pair.run(Duration::from_secs(1));
            // 64 KiB holds 59 datagrams of 1100 bytes.
            let newest = (21..80).map(|byte| vec![byte; 1_100]);
            assert_eq!(take(&mut pair.server), newest.collect::<Vec<_>>());
        });
    }

    #[test]
    fn a_receiver_that_falls_behind_keeps_the_newest_64() {
        testing::run(1, |shard| {
            let mut pair = dial(shard);
            let mut client = datagrams(&mut pair.client);
            for byte in 0..100 {
                client.send(shard.block(&[byte; 100])).expect("sent");
            }
            pair.run(Duration::from_secs(1));
            assert_eq!(arrivals(&pair.server), 1);
            let newest = (36..100).map(|byte| vec![byte; 100]);
            assert_eq!(take(&mut pair.server), newest.collect::<Vec<_>>());
        });
    }

    #[test]
    fn one_that_finds_the_pool_full_drops_with_no_event() {
        testing::run(1, |shard| {
            let pool = small();
            let mut pair = dial_with(shard, &with_pool(shard, &pool));
            let held = pool.alloc(100).expect("room");
            datagrams(&mut pair.client)
                .send(shard.block(&[1; 100]))
                .expect("sent");
            pair.run(RUN);
            assert_eq!(arrivals(&pair.server), 0);
            assert_eq!(take(&mut pair.server), Vec::<Vec<u8>>::new());
            drop(held);
            datagrams(&mut pair.client)
                .send(shard.block(&[2; 100]))
                .expect("sent");
            pair.run(RUN);
            assert_eq!(arrivals(&pair.server), 1);
            assert_eq!(take(&mut pair.server), [vec![2; 100]]);
        });
    }

    #[test]
    fn are_there_only_while_connected_and_free_when_the_connection_drains() {
        testing::run(1, |shard| {
            let pool = small();
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.server.endpoint = Endpoint::new(
                &with_pool(shard, &pool),
                testing::SERVER_SHARD,
                NonZeroUsize::MIN,
            );
            pair.dial(tls::public(&testing::SERVER_KEY));
            let client = pair.client.key.expect("a key");
            assert!(pair.client.endpoint.datagrams(client).is_none());
            pair.run(RUN);
            datagrams(&mut pair.client)
                .send(shard.block(&[1; 100]))
                .expect("sent");
            pair.run(RUN);
            let server = pair.server.key.expect("a key");
            pair.server.endpoint.close(pair.now(), server, Code(7));
            assert!(pair.server.endpoint.datagrams(server).is_none());
            pair.run(RUN);
            assert!(pair.client.endpoint.datagrams(client).is_none());
            let full = block::Error::Exhausted {
                requested: 100,
                available: 108,
            };
            assert_eq!(pool.alloc(100).err(), Some(full));
            pair.run(Duration::from_secs(3));
            pool.alloc(100).expect("the drain freed the datagram");
        });
    }
}

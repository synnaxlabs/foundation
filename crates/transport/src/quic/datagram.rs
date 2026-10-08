//! The datagrams that arrived on one connection: whole messages, each in one QUIC
//! DATAGRAM frame, that may be lost.

use std::collections::VecDeque;

use block::{Block, Pool};

use super::{Event, connection};

/// The most datagrams that wait untaken on one connection.
const WAITING_MAX: usize = 64;

/// The datagrams that arrived on one connection and wait to be taken, oldest first.
#[derive(Default)]
pub(super) struct Received(VecDeque<Block>);

impl Received {
    /// Moves each datagram that `inner` holds into a block from `pool`, and gives
    /// [`Event::Datagram`] when none waited. A datagram drops when it gets no block
    /// (`pool` or the system has no room). When [`WAITING_MAX`] wait, a new one drops
    /// the oldest.
    ///
    /// # Panics
    ///
    /// When `pool` cannot hold a datagram that `inner` took.
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
            let mut block = match pool.alloc(datagram.len()) {
                Ok(block) => block,
                Err(block::Error::Exhausted { .. } | block::Error::Refused { .. }) => {
                    continue;
                }
                Err(error @ block::Error::TooLarge { .. }) => {
                    panic!("invariant: the pool holds `message_bytes_max`: {error}")
                }
            };
            block.copy_from_slice(&datagram);
            if self.0.len() == WAITING_MAX {
                self.0.pop_front();
            }
            self.0.push_back(block.freeze());
        }
        if !waited && !self.0.is_empty() {
            events.push_back(Event::Datagram { key });
        }
    }

    /// Takes the oldest datagram.
    pub(super) fn pop(&mut self) -> Option<Block> {
        self.0.pop_front()
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
    use crate::quic::pair::{self, Pair, Side};
    use crate::quic::{Datagrams, Endpoint};
    use crate::testing;
    use crate::{Code, Config, Error};

    /// The link delay each way.
    const DELAY: Duration = Duration::from_millis(10);
    /// Long enough for the datagrams sent before it to arrive.
    const RUN: Duration = Duration::from_millis(100);

    /// A connected pair whose server has `config`.
    fn dial_with(shard: &testing::Shard, config: &Config) -> Pair {
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.server.endpoint =
            Endpoint::new(config, pair::SERVER_SHARD, NonZeroUsize::MIN);
        pair.dial(pair::SERVER_KEY.public());
        pair.run(RUN);
        pair
    }

    fn dial(shard: &testing::Shard) -> Pair {
        dial_with(shard, &shard.config(pair::SERVER_KEY, Span::SECOND))
    }

    /// A pool with `budget` bytes. A 100-byte block takes 192 of them.
    fn pool(budget: usize) -> Rc<Pool> {
        let config = block::Config { budget };
        let memory = Heap::new(config.reservation());
        Rc::new(Pool::new(config, memory))
    }

    /// A pool with room for one block of 100 bytes and not two while the block it
    /// gives is held.
    fn small() -> (Rc<Pool>, block::Unique) {
        let pool = pool(block::footprint(1_472) + 300);
        let filled = pool.alloc(1_472).expect("room");
        (pool, filled)
    }

    /// A server config that takes messages from `pool`, up to its largest block.
    fn with_pool(shard: &testing::Shard, pool: &Rc<Pool>) -> Config {
        Config {
            message_bytes_max: NonZeroUsize::new(pool.largest()).expect("not zero"),
            pool: Rc::clone(pool),
            ..shard.config(pair::SERVER_KEY, Span::SECOND)
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
                let events = receiver.events[2..].iter().map(|(_, event)| event);
                let datagram = Event::Datagram { key };
                assert_eq!(events.collect::<Vec<_>>(), [&datagram, &datagram]);
            }
        });
    }

    #[test]
    fn take_up_to_the_path_limit() {
        testing::run(1, |shard| {
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.dial(pair::SERVER_KEY.public());
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
    #[should_panic(expected = "config message_bytes_max must be at least 1472")]
    fn a_largest_message_below_one_packet_panics() {
        testing::run(1, |shard| {
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_471).expect("not zero"),
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            drop(Endpoint::new(
                &config,
                pair::SERVER_SHARD,
                NonZeroUsize::MIN,
            ));
        });
    }

    #[test]
    fn a_full_packet_of_small_ones_all_arrive_at_the_smallest_limit() {
        testing::run(1, |shard| {
            let config = Config {
                message_bytes_max: NonZeroUsize::new(1_472).expect("not zero"),
                ..shard.config(pair::SERVER_KEY, Span::SECOND)
            };
            let mut pair = dial_with(shard, &config);
            pair.run(Duration::from_secs(1));
            let mut client = datagrams(&mut pair.client);
            // 10 frames of 1 + 2 + 138 bytes fit in one packet.
            for byte in 0..10 {
                client.send(shard.block(&[byte; 138])).expect("sent");
            }
            pair.run(RUN);
            assert_eq!(arrivals(&pair.server), 1);
            let all = (0..10).map(|byte| vec![byte; 138]);
            assert_eq!(take(&mut pair.server), all.collect::<Vec<_>>());
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
            let (pool, _filled) = small();
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
    fn one_with_no_block_drops_no_other() {
        testing::run(1, |shard| {
            // Room for 64 blocks of 100 bytes, and not for one of 1000.
            let pool = pool(64 * 192 + 500);
            let mut pair = dial_with(shard, &with_pool(shard, &pool));
            let mut client = datagrams(&mut pair.client);
            for byte in 0..64 {
                client.send(shard.block(&[byte; 100])).expect("sent");
            }
            pair.run(RUN);
            datagrams(&mut pair.client)
                .send(shard.block(&[64; 1_000]))
                .expect("sent");
            pair.run(RUN);
            let all = (0..64).map(|byte| vec![byte; 100]);
            assert_eq!(take(&mut pair.server), all.collect::<Vec<_>>());
        });
    }

    #[test]
    fn the_ones_after_one_with_no_block_still_arrive() {
        testing::run(1, |shard| {
            let (pool, _filled) = small();
            let mut pair = dial_with(shard, &with_pool(shard, &pool));
            let mut client = datagrams(&mut pair.client);
            for byte in 1..=3 {
                client.send(shard.block(&[byte; 100])).expect("sent");
            }
            pair.run(RUN);
            assert_eq!(take(&mut pair.server), [vec![1; 100]]);
            datagrams(&mut pair.client)
                .send(shard.block(&[4; 100]))
                .expect("sent");
            pair.run(RUN);
            assert_eq!(arrivals(&pair.server), 2);
            assert_eq!(take(&mut pair.server), [vec![4; 100]]);
        });
    }

    #[test]
    fn are_there_only_while_connected_and_free_when_the_connection_ends() {
        testing::run(1, |shard| {
            let (pool, _filled) = small();
            let mut pair = Pair::new(shard, Span::SECOND, DELAY);
            pair.server.endpoint = Endpoint::new(
                &with_pool(shard, &pool),
                pair::SERVER_SHARD,
                NonZeroUsize::MIN,
            );
            pair.dial(pair::SERVER_KEY.public());
            let client = pair.client.key.expect("a key");
            assert!(pair.client.endpoint.datagrams(client).is_none());
            pair.run(RUN);
            datagrams(&mut pair.client)
                .send(shard.block(&[1; 100]))
                .expect("sent");
            pair.run(RUN);
            let server = pair.server.key.expect("a key");
            let full = block::Error::Exhausted {
                requested: 100,
                available: 108,
            };
            assert_eq!(pool.alloc(100).err(), Some(full));
            pair.server.endpoint.close(pair.now(), server, Code(7));
            assert!(pair.server.endpoint.datagrams(server).is_none());
            pool.alloc(100).expect("the close freed the datagram");
            pair.run(RUN);
            assert!(pair.client.endpoint.datagrams(client).is_none());
        });
    }
}

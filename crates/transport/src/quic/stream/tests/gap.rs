//! The send window and the held blocks after a reset of a stream with bytes that the
//! peer did not acknowledge.

use super::*;
use crate::quic::pair::Foreign;

#[test]
fn a_reset_after_a_loss_gives_back_each_byte_of_the_window() {
    testing::run(1, |shard| {
        let mut pair = connected(shard);
        let mut sender = open_sender(&mut pair, Class::Complete);
        let mut incoming = None;
        // Grows the congestion window until it takes a whole send window at once.
        for _ in 0..16 {
            fill(&mut pair, shard, &mut sender);
            pair.run(RUN);
            let incoming = incoming.get_or_insert_with(|| accept(&mut pair.server));
            let now = pair.now();
            drain(&mut pair.server, now, &mut incoming.receiver);
            pair.run(RUN);
        }
        pair.client.drops = 1;
        let full = fill(&mut pair, shard, &mut sender);
        // The acknowledgments of the later batches come back.
        pair.run(2 * DELAY + STEP);
        let now = pair.now();
        pair.client.endpoint.reset(now, &mut sender, Code(9));
        pair.run(RUN);
        let mut next = open_sender(&mut pair, Class::Complete);
        assert_eq!(fill(&mut pair, shard, &mut next), full);
    });
}

#[test]
fn a_reset_frees_its_blocks_before_the_peer_acknowledges_it() {
    testing::run(1, |shard| {
        // A peer that gives credit for 8 windows, so only the send window limits.
        let credit = VarInt::from_u32(8 << 20);
        let foreign = Foreign::new(shard, |config| {
            config.receive_window(credit).stream_receive_window(credit);
        });
        let mut pair = Pair::new(shard, Span::SECOND, DELAY);
        pair.foreign = Some(foreign);
        let (now, peer) = (pair.now(), pair::FOREIGN_KEY.public());
        pair.client.key =
            Some(pair.client.endpoint.connect(now, Some(peer), pair::FOREIGN));
        pair.run(RUN);
        let own = Hello {
            window_bytes: 1 << 20,
            message_bytes_max: MESSAGE_MAX,
        };
        let connection = pair.foreign.as_mut().expect("a foreign peer").connection();
        raw(connection, Dir::Uni, &own.encode(), true);
        pair.run(RUN);
        // No packet of the peer arrives after this.
        pair.foreign = None;
        let config = block::Config::new(1 << 25).expect("the budget fits");
        let memory = Heap::new(config.reservation());
        let pool = Pool::new(config, memory);
        let mut first = open_sender(&mut pair, Class::Complete);
        fill_from(&mut pair, &pool, &mut first);
        pair.run(RUN);
        let now = pair.now();
        pair.client.endpoint.reset(now, &mut first, Code(9));
        pair.run(RUN);
        let mut second = open_sender(&mut pair, Class::Complete);
        fill_from(&mut pair, &pool, &mut second);
        let held = pool.committed() / MESSAGE_MAX;
        assert!(held <= (1 << 20) / MESSAGE_MAX + 2, "{held} blocks");
    });
}

/// Writes messages of [`MESSAGE_MAX`] bytes from `pool` to the client's `sender`
/// until one waits for the window.
fn fill_from(pair: &mut Pair, pool: &Pool, sender: &mut Sender) {
    let now = pair.now();
    loop {
        let message = testing::alloc(pool, MESSAGE_MAX).expect("room").freeze();
        let written =
            pair::write(&mut pair.client.endpoint, now, sender, &mut Some(message));
        if written.expect("written").is_pending() {
            return;
        }
    }
}

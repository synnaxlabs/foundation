//! Tests of simulated TCP on a link with loss.

use super::*;

/// A link that loses each packet.
fn cut() -> link::Config {
    link::Config {
        loss: 1.0,
        ..link::Config::default()
    }
}

fn secs(n: i64) -> Span {
    Span::from_nanos(n * Span::SECOND.nanos())
}

/// What `future` gives within `span` on the clock of `node`, or `None`.
async fn within<F: Future>(node: &node::Node, span: Span, future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut sleep = pin!(node.clock().sleep(span));
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(Some(output)),
        Poll::Pending => sleep.as_mut().poll(cx).map(|()| None),
    })
    .await
}

#[test]
fn a_stream_on_a_link_with_loss_half_carries_each_byte_in_order() {
    let link = link::Config {
        loss: 0.5,
        jitter: delay(),
        ..link::Config::default()
    };
    for seed in 0..4 {
        let (_, received) = send(seed, link, 1 << 16);
        assert_eq!(received, (pattern(1 << 16), Ok(())), "seed {seed}");
    }
}

#[test]
fn one_seed_gives_one_digest_on_a_link_with_loss() {
    let link = link::Config {
        loss: 0.3,
        ..link::Config::default()
    };
    let digest = send(3, link, 1 << 14).0;
    assert_eq!(send(3, link, 1 << 14).0, digest);
    assert_ne!(send(4, link, 1 << 14).0, digest);
}

#[test]
fn a_connect_on_a_cut_link_waits_until_its_caller_gives_up_and_then_sends_nothing() {
    let (mut sim, a, b) = pair(0, cut());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let connect = connect(&node, remote, options());
        let connected = within(&node, secs(10), connect).await;
        (node.clock().now(), connected.is_some())
    });
    sim.run_for(secs(11)).unwrap();
    assert_eq!(take(&client), (after(secs(10)), false));
    let digest = sim.digest();
    sim.run_for(secs(200)).unwrap();
    assert_eq!(sim.digest(), digest);
}

#[test]
fn a_connect_on_a_cut_link_times_out_after_127_s() {
    let (mut sim, a, b) = pair(0, cut());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run().unwrap();
    // Linux sends a SYN 7 times, 1 s after the first and twice as long after each.
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (after(secs(127)), Some(error)));
}

#[test]
fn a_lost_syn_goes_again_on_the_link_that_heals() {
    let (mut sim, a, b) = pair(0, cut());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        accept(&mut listener).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        connect(&node, remote, options()).await.unwrap();
        node.clock().now()
    });
    sim.run_for(secs(2)).unwrap();
    sim.link(&a, &b, link::Config::default());
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The tries at 0 s and 1 s are lost, and the one at 3 s is not.
    assert_eq!(take(&client), legs(2) + secs(3));
}

#[test]
fn a_writer_whose_bytes_are_lost_times_out_and_its_quiet_peer_waits() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read(&mut tcp, 1).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(Span::SECOND).await;
        write_all(&mut tcp, &[7]).await.unwrap();
        let error = read(&mut tcp, 1).await.unwrap_err();
        (node.clock().now(), error)
    });
    sim.run_for(millis(500)).unwrap();
    sim.link(&a, &b, cut());
    let stuck = Error::Stuck {
        threads: vec!["server".into()],
        seed: 0,
    };
    assert_eq!(sim.run().unwrap_err(), stuck);
    // Linux sends the bytes 16 times, from an RTO of the round trip plus 200 ms,
    // twice as long after each, to at most 120 s: 205.1115 s, then 6 times 120 s.
    let wrote = legs(2) + Span::SECOND;
    let timed_out = wrote + Span::from_nanos(925_111_500_000);
    assert_eq!(take(&client), (timed_out, Net::TimedOut { remote }));
}

#[test]
fn a_syn_ack_that_is_lost_each_time_times_out_the_connect_and_leaves_the_backlog() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let listen = tcp::Listen {
        local: at(&b, 4433),
        backlog: 0,
        options: options(),
    };
    let mut listener = b.net().listen(&listen).unwrap();
    let remote = at(&b, 4433);
    let first = start(&a, "first", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run_for(secs(64)).unwrap();
    // Linux sends a SYN-ACK 6 times, 1 s after the first and twice as long after each.
    let timed_out = legs(1) + secs(63);
    let error = Net::TimedOut { remote };
    assert_eq!(take(&first), (timed_out, Some(error)));
    sim.link(&b, &a, link::Config::default());
    let server = start(&b, "server", move |_| async move {
        accept(&mut listener).await.peer()
    });
    let _second = start(&a, "second", move |node| async move {
        connect(&node, remote, options()).await.unwrap()
    });
    sim.run().unwrap();
    assert_eq!(take(&server), at(&a, 49_153));
}

#[test]
fn a_reset_drops_the_lost_bytes_of_its_stream_and_goes_at_once() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        let read = read(&mut tcp, 100).await;
        (node.clock().now(), read)
    });
    let remote = at(&b, 4433);
    let _client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[7; 100]).await.unwrap();
        node.clock().sleep(millis(100)).await;
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(millis(50)).unwrap();
    sim.link(&a, &b, link::Config::default());
    sim.run().unwrap();
    // The bytes would go again at 210.5 ms; the drop at 110 ms resets at once.
    let reset = legs(3) + millis(110);
    let error = Net::Reset {
        remote: at(&a, 49_152),
    };
    assert_eq!(take(&server), (reset, Err(error)));
}

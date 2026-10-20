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
async fn within<F: Future>(
    node: &node::Node,
    span: Span,
    future: F,
) -> Option<F::Output> {
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
    let _server = start(
        &b,
        "server",
        move |_| async move { accept(&mut listener).await },
    );
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

/// The time from the write of bytes that are each time lost on a link of `delay` to
/// the timeout of the writer. Its peer, which waits to read, is stuck.
fn write_timeout(delay: Span) -> Span {
    let link = link::Config {
        delay,
        ..link::Config::default()
    };
    let (mut sim, a, b) = pair(0, link);
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read(&mut tcp, 1).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(secs(2)).await;
        let wrote = node.clock().now();
        write_all(&mut tcp, &[7]).await.unwrap();
        let error = read(&mut tcp, 1).await.unwrap_err();
        (node.clock().now() - wrote, error)
    });
    sim.run_for(Span::from_nanos(3 * delay.nanos() + Span::SECOND.nanos()))
        .unwrap();
    sim.link(&a, &b, link::Config { loss: 1.0, ..link });
    let stuck = Error::Stuck {
        threads: vec!["server".into()],
        seed: 0,
    };
    assert_eq!(sim.run().unwrap_err(), stuck);
    let (span, error) = take(&client);
    assert_eq!(error, Net::TimedOut { remote });
    span
}

#[test]
fn a_writer_whose_bytes_are_lost_times_out_at_the_first_try_after_924_6_s() {
    // Linux tries from an RTO of the round trip plus 200 ms, twice as long after
    // each loss, to at most 120 s, and stops at the first try 924.6 s after the
    // first send.
    assert_eq!(write_timeout(delay()), Span::from_nanos(925_111_500_000));
    assert_eq!(write_timeout(millis(5)), Span::from_nanos(934_830_000_000));
    assert_eq!(
        write_timeout(millis(500)),
        Span::from_nanos(992_400_000_000)
    );
}

#[test]
fn a_lost_ack_goes_with_the_next_segment_of_its_direction() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        read(&mut tcp, 1).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[9]).await.unwrap();
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let options = tcp::Options {
            send_buffer_bytes: 1,
            ..options()
        };
        let mut tcp = connect(&node, remote, options).await.unwrap();
        write_all(&mut tcp, &[1]).await.unwrap();
        write_all(&mut tcp, &[2]).await.unwrap();
        node.clock().now()
    });
    sim.run_for(Span::from_nanos(5 * delay().nanos() / 2))
        .unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The ACK of the first byte is lost at leg 3, and the bytes of the server carry
    // it 10 ms later, before its RTO of 200.5 ms.
    assert_eq!(take(&client), legs(4) + millis(10));
}

#[test]
fn a_syn_ack_lost_each_time_leaves_the_backlog_at_63_s_and_the_connect_at_127_s() {
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
    // Linux sends a SYN-ACK 6 times, 1 s after the first and twice as long after
    // each, and drops the request at the timeout of the last.
    sim.run_for(Span::from_nanos(delay().nanos() + secs(63).nanos()))
        .unwrap();
    sim.link(&b, &a, link::Config::default());
    let server = start(&b, "server", move |_| async move {
        accept(&mut listener).await.peer()
    });
    let _second = start(&a, "second", move |node| async move {
        connect(&node, remote, options()).await.unwrap()
    });
    sim.run_for(secs(1)).unwrap();
    assert_eq!(take(&server), at(&a, 49_153));
    // The client does not know that its SYN arrived, and gives up 127 s after it.
    sim.run().unwrap();
    let error = Net::TimedOut { remote };
    assert_eq!(take(&first), (after(secs(127)), Some(error)));
}

#[test]
fn a_power_cut_keeps_the_connects_that_give_up_of_the_other_nodes() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let c = sim.node(node::Config::default());
    sim.link(&b, &a, cut());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run_for(secs(70)).unwrap();
    sim.crash(&c, Crash::Power);
    sim.run().unwrap();
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (after(secs(127)), Some(error)));
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

/// Options with a send buffer of 1 byte, so that a write of a second byte waits for
/// the ACK of the first.
fn one_byte() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1,
        ..options()
    }
}

#[test]
fn an_ack_goes_at_once_while_the_bytes_of_its_end_wait_to_go_again() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen_on(&b, at(&b, 4433), one_byte());
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[2]).await.unwrap();
        write_all(&mut tcp, &[3]).await.unwrap();
        (node.clock().now(), read(&mut tcp, 1).await.unwrap(), tcp)
    });
    let remote = at(&b, 4433);
    let _client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        tcp
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(Span::from_nanos(5_600_000)).unwrap();
    sim.link(&a, &b, link::Config::default());
    sim.run().unwrap();
    // The byte of the client is lost at leg 2 plus 10 ms. The ACK of the first byte
    // of the server goes at leg 4 plus 10 ms, not behind it at its RTO.
    let (wrote, read, _tcp) = take(&server);
    assert_eq!((wrote, read), (legs(5) + millis(10), vec![1]));
}

#[test]
fn a_lost_ack_goes_with_the_first_segment_sent_after_it_not_one_in_flight() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[9]).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[10]).await.unwrap();
        tcp
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, one_byte()).await.unwrap();
        node.clock().sleep(Span::from_nanos(10_100_000)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        write_all(&mut tcp, &[2]).await.unwrap();
        (node.clock().now(), tcp)
    });
    let leg = delay().nanos();
    sim.run_for(Span::from_nanos(3 * leg + 10_050_000)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(Span::from_nanos(100_000)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The ACK of the first byte is lost while the byte 9 is in flight. The byte 10,
    // sent after it, carries it.
    let (wrote, _tcp) = take(&client);
    assert_eq!(wrote, legs(4) + millis(20));
}

#[test]
fn a_power_cut_keeps_the_lost_bytes_of_the_other_nodes() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let c = sim.node(node::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        let read = read(&mut tcp, 1).await.unwrap();
        (node.clock().now(), read, tcp)
    });
    let remote = at(&b, 4433);
    let _client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        tcp
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(millis(7)).unwrap();
    sim.crash(&c, Crash::Power);
    sim.link(&a, &b, link::Config::default());
    sim.run().unwrap();
    // The byte is lost at leg 2 plus 10 ms, and goes again after its RTO of 2 legs
    // plus 200 ms.
    let (read_at, read, _tcp) = take(&server);
    assert_eq!((read_at, read), (legs(5) + millis(210), vec![1]));
}

#[test]
fn a_power_cut_keeps_the_lost_acks_of_the_other_nodes() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let c = sim.node(node::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, one_byte()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        write_all(&mut tcp, &[2]).await.unwrap();
        (node.clock().now(), tcp)
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(millis(7)).unwrap();
    sim.crash(&c, Crash::Power);
    sim.link(&b, &a, link::Config::default());
    sim.run_for(secs(1)).unwrap();
    // The ACK of the first byte is lost at leg 3 plus 10 ms, and goes again after
    // its RTO of 2 legs plus 200 ms.
    let (wrote, _tcp) = take(&client);
    assert_eq!(wrote, legs(6) + millis(210));
}

/// Asserts that `client` gives `Error::TimedOut` when each segment from the server
/// is lost from 5 ms on. The server's listener has `options`, and the client starts
/// 10 ms after its connect.
fn times_out<T, F, G>(
    options: tcp::Options,
    server: impl FnOnce(Tcp) -> F + Send + 'static,
    client: impl FnOnce(Tcp) -> G + Send + 'static,
) where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
    G: Future<Output = Net> + 'static,
{
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen_on(&b, at(&b, 4433), options);
    let _server = start(&b, "server", move |_| async move {
        server(accept(&mut listener).await).await
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let tcp = connect(&node, remote, self::options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        client(tcp).await
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run().unwrap();
    assert_eq!(take(&client), Net::TimedOut { remote });
}

#[test]
fn a_lost_ack_times_out_the_writer_of_the_bytes_it_acks() {
    let server = |mut tcp| async move {
        read(&mut tcp, 1).await.unwrap();
        tcp
    };
    times_out(options(), server, |mut tcp| async move {
        write_all(&mut tcp, &[1]).await.unwrap();
        read(&mut tcp, 1).await.unwrap_err()
    });
}

#[test]
fn a_lost_window_update_times_out_the_writer_that_waits_for_room() {
    let options = tcp::Options {
        recv_buffer_bytes: 2,
        ..options()
    };
    let server = |mut tcp| async move {
        read(&mut tcp, 2).await.unwrap();
        tcp
    };
    times_out(options, server, |mut tcp| async move {
        write_all(&mut tcp, &[1, 2, 3, 4]).await.unwrap();
        read(&mut tcp, 1).await.unwrap_err()
    });
}

#[test]
fn a_lost_ack_of_a_fin_times_out_the_end_that_closed() {
    let server = |mut tcp| async move {
        assert_eq!(read(&mut tcp, 1).await, Ok(vec![]));
        tcp
    };
    times_out(options(), server, |mut tcp| async move {
        close(&mut tcp).await.unwrap();
        read(&mut tcp, 1).await.unwrap_err()
    });
}

#[test]
fn a_lost_ack_that_no_end_waits_for_breaks_nothing() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        assert_eq!(read(&mut tcp, 4_096).await, Ok(pattern(2_000)));
        node.clock().sleep(secs(1_000)).await;
        write_all(&mut tcp, &[9]).await.unwrap();
        tcp
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(2_000)).await.unwrap();
        read(&mut tcp, 1).await
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(secs(999)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The window update of the read at 10 ms is lost each time, and the client,
    // whose bytes are each acked, waits for nothing from it.
    assert_eq!(take(&client), Ok(vec![9]));
}

#[test]
fn a_lost_ack_of_a_handshake_times_out_the_accept_and_leaves_the_backlog() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let listen = tcp::Listen {
        local: at(&b, 4433),
        backlog: 0,
        options: options(),
    };
    let mut listener = b.net().listen(&listen).unwrap();
    let remote = at(&b, 4433);
    let _first = start(&a, "first", move |node| async move {
        connect(&node, remote, options()).await.unwrap()
    });
    sim.run_for(Span::from_nanos(3 * delay().nanos() / 2))
        .unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(secs(1_000)).unwrap();
    sim.link(&a, &b, link::Config::default());
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
fn a_held_byte_times_out_by_the_time_it_was_held_not_the_time_it_went() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        let read = read(&mut tcp, 2).await;
        (read, tcp)
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        let wrote = node.clock().now();
        write_all(&mut tcp, &[1]).await.unwrap();
        write_all(&mut tcp, &[2]).await.unwrap();
        let error = read(&mut tcp, 1).await.unwrap_err();
        (node.clock().now() - wrote, error)
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(secs(205)).unwrap();
    let half = link::Config {
        loss: 0.5,
        ..link::Config::default()
    };
    sim.link(&a, &b, half);
    sim.run_for(millis(200)).unwrap();
    sim.link(&a, &b, cut());
    sim.run().unwrap();
    // On this seed, the first byte arrives at 205.1 s, and the second byte, held since
    // its write, is lost at each try from then. It times out at the first try 924.6 s
    // after its hold, not after its first send at 205.1 s.
    let (read, _tcp) = take(&server);
    assert_eq!(read, Ok(vec![1]));
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (Span::from_nanos(1_010_223_000_000), error));
}

#[test]
fn a_new_lost_ack_times_out_from_its_own_send_not_from_an_older_lost_ack() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        assert_eq!(read(&mut tcp, 4_096).await, Ok(pattern(2_000)));
        assert_eq!(read(&mut tcp, 1).await, Ok(vec![1]));
        write_all(&mut tcp, &[9]).await.unwrap();
        node.clock().sleep(secs(400)).await;
        tcp
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(2_000)).await.unwrap();
        node.clock().sleep(secs(920)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        read(&mut tcp, 1).await
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(secs(1_000)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The window update of the read at 10 ms waits for no end. The ACK of the byte
    // written at 920 s times out 924.6 s after it, so it goes on the link that heals
    // at 1000 s.
    assert_eq!(take(&client), Ok(vec![9]));
}

#[test]
fn a_write_after_a_lost_window_update_goes_on_the_link_that_heals() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let small = tcp::Options {
        recv_buffer_bytes: 2,
        ..options()
    };
    let mut listener = listen_on(&b, at(&b, 4433), small);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        assert_eq!(read(&mut tcp, 2).await, Ok(vec![1, 2]));
        let read = within(&node, secs(1_200), read(&mut tcp, 1)).await;
        (read, tcp)
    });
    let remote = at(&b, 4433);
    let _client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &[1, 2]).await.unwrap();
        node.clock().sleep(secs(1_000)).await;
        write_all(&mut tcp, &[3]).await.unwrap();
        node.clock().sleep(secs(400)).await;
        tcp
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(secs(950)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    let (read, _tcp) = take(&server);
    assert_eq!(read, Some(Ok(vec![3])));
}

#[test]
fn a_stale_ack_that_goes_again_does_not_shrink_the_window() {
    let link = link::Config {
        delay: millis(10),
        ..link::Config::default()
    };
    let (mut sim, a, b) = pair(0, link);
    let small = tcp::Options {
        recv_buffer_bytes: 2,
        ..options()
    };
    let mut listener = listen_on(&b, at(&b, 4433), small);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(225)).await;
        let first = read(&mut tcp, 2).await.unwrap();
        let second = read(&mut tcp, 2).await;
        (first, second, tcp)
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1, 2]).await.unwrap();
        node.clock().sleep(millis(270)).await;
        write_all(&mut tcp, &[3, 4]).await.unwrap();
        tcp
    });
    sim.run_for(millis(35)).unwrap();
    sim.link(&b, &a, link::Config { loss: 1.0, ..link });
    sim.run_for(millis(10)).unwrap();
    sim.link(&b, &a, link);
    sim.run().unwrap();
    // The ACK of [1, 2], lost at 40 ms, would go again at 260 ms, after the window
    // update of the read at 255 ms.
    let _tcp = take(&client);
    let (first, second, _tcp) = take(&server);
    assert_eq!((first, second), (vec![1, 2], Ok(vec![3, 4])));
}

#[test]
fn a_connect_whose_rst_is_lost_times_out_after_127_s() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run().unwrap();
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (after(secs(127)), Some(error)));
}

#[test]
fn a_connect_whose_rst_is_lost_is_refused_on_the_link_that_heals() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run_for(secs(20)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The RST goes again 1, 3, 7, 15, and 31 s after the SYN arrives at 250 us.
    let error = Net::Refused { remote };
    let at = after(Span::from_nanos(31_000_500_000));
    assert_eq!(take(&client), (at, Some(error)));
}

#[test]
fn a_lost_rst_goes_again_on_the_times_of_the_syn_that_it_answers() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&a, &b, cut());
    sim.link(&b, &a, cut());
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run_for(secs(2)).unwrap();
    sim.link(&a, &b, link::Config::default());
    sim.run_for(secs(3)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The SYN is lost at 0 and 1 s and arrives at 3 s. Its RST is lost, and goes
    // again 4 s later, as the third SYN would.
    let error = Net::Refused { remote };
    let at = after(Span::from_nanos(7_000_500_000));
    assert_eq!(take(&client), (at, Some(error)));
}

#[test]
fn a_writer_whose_bytes_meet_a_lost_rst_times_out() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let tcp = accept(&mut listener).await;
        node.clock().sleep(millis(5)).await;
        drop(tcp);
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        within(&node, secs(2_000), read(&mut tcp, 1)).await
    });
    sim.run_for(millis(3)).unwrap();
    sim.link(&b, &a, cut());
    sim.run().unwrap();
    assert_eq!(take(&client), Some(Err(Net::TimedOut { remote })));
}

#[test]
fn a_connect_whose_syn_ack_is_lost_times_out_after_its_listener_drops() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        node.clock().sleep(secs(10)).await;
        drop(listener);
    });
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run().unwrap();
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (after(secs(127)), Some(error)));
}

#[test]
fn a_connect_whose_syn_ack_is_lost_times_out_after_a_power_cut_of_its_server() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let _listener = listener;
        pending::<()>().await;
    });
    let client = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run_for(secs(10)).unwrap();
    sim.crash(&b, Crash::Power);
    sim.run().unwrap();
    let error = Net::TimedOut { remote };
    assert_eq!(take(&client), (after(secs(127)), Some(error)));
}

#[test]
fn a_writer_whose_ack_is_lost_times_out_after_a_power_cut_of_its_peer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let _tcp = accept(&mut listener).await;
        pending::<()>().await;
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, one_byte()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        write_all(&mut tcp, &[2]).await
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(secs(1)).unwrap();
    sim.crash(&b, Crash::Power);
    sim.run().unwrap();
    assert_eq!(take(&client), Err(Net::TimedOut { remote }));
}

/// The options of an end with a window of 1 byte that sends 1 byte at a time.
fn one_byte_window() -> tcp::Options {
    tcp::Options {
        recv_buffer_bytes: 1,
        ..one_byte()
    }
}

#[test]
fn a_writer_that_waits_for_room_is_reset_after_the_lost_rst_of_its_peer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen_on(&b, at(&b, 4433), one_byte_window());
    let _server = start(&b, "server", move |node| async move {
        let tcp = accept(&mut listener).await;
        node.clock().sleep(millis(5)).await;
        drop(tcp);
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, one_byte()).await.unwrap();
        within(&node, secs(10), write_all(&mut tcp, &[1; 8])).await
    });
    sim.run_for(millis(3)).unwrap();
    sim.link(&b, &a, cut());
    sim.run_for(millis(10)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    assert_eq!(take(&client), Some(Err(Net::Reset { remote })));
}

#[test]
fn a_writer_that_waits_for_room_is_reset_after_a_power_cut_of_its_peer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen_on(&b, at(&b, 4433), one_byte_window());
    let _server = start(&b, "server", move |_| async move {
        let _tcp = accept(&mut listener).await;
        pending::<()>().await;
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, one_byte()).await.unwrap();
        within(&node, secs(10), write_all(&mut tcp, &[1; 8])).await
    });
    sim.run_for(millis(5)).unwrap();
    sim.crash(&b, Crash::Power);
    sim.run().unwrap();
    assert_eq!(take(&client), Some(Err(Net::Reset { remote })));
}

#[test]
fn a_reader_of_a_peer_whose_power_is_cut_waits_until_it_writes() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |_| async move {
        let _tcp = accept(&mut listener).await;
        pending::<()>().await;
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        let idle = within(&node, secs(1_000), read(&mut tcp, 1)).await;
        write_all(&mut tcp, &[1]).await.unwrap();
        (idle, read(&mut tcp, 1).await)
    });
    sim.run_for(millis(5)).unwrap();
    sim.crash(&b, Crash::Power);
    sim.run().unwrap();
    assert_eq!(take(&client), (None, Err(Net::Reset { remote })));
}

#[test]
fn a_syn_ack_lost_five_times_connects_on_its_sixth_try_at_31_s() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let connected = connect(&node, remote, options()).await.is_ok();
        (node.clock().now(), connected)
    });
    sim.run_for(secs(20)).unwrap();
    sim.link(&b, &a, link::Config::default());
    sim.run().unwrap();
    // The SYN-ACK goes again 1, 3, 7, 15, and 31 s after the SYN arrives at 250 us.
    let at = after(Span::from_nanos(31_000_500_000));
    assert_eq!(take(&client), (at, true));
}

#[test]
fn a_connect_dropped_after_its_syn_ack_gave_up_leaves_nothing_to_expire() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    sim.link(&b, &a, cut());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let connect = connect(&node, remote, options());
        let connected = within(&node, secs(100), connect).await;
        (node.clock().now(), connected.is_some())
    });
    sim.run_for(secs(200)).unwrap();
    assert_eq!(take(&client), (after(secs(100)), false));
}

#[test]
fn a_reset_end_keeps_its_reset_after_its_lost_bytes_would_time_out() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(&b, "server", move |node| async move {
        let tcp = accept(&mut listener).await;
        node.clock().sleep(millis(50)).await;
        drop(tcp);
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, &[7]).await.unwrap();
        node.clock().sleep(millis(100)).await;
        let written = write_all(&mut tcp, &[8]).await;
        (tcp, written)
    });
    sim.run_for(millis(5)).unwrap();
    sim.link(&a, &b, cut());
    sim.run_for(secs(1)).unwrap();
    let digest = sim.digest();
    sim.run_for(secs(1_000)).unwrap();
    assert_eq!(sim.digest(), digest);
    let (_tcp, written) = take(&client);
    assert_eq!(written, Err(Net::Reset { remote }));
}

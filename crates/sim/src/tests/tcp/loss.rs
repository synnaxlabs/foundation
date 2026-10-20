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

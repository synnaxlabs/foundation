use crate::Settings;

use super::*;

/// A node on a new host of `sim` with `settings`, and its host.
fn start(sim: &mut sim::Sim, settings: Settings) -> (sim::node::Node, Node) {
    let host = keyed(sim, 2);
    let node = Node::start(Config {
        settings,
        ..config(&host, Size::MEBIBYTE, Box::new(heap))
    });
    (host, node)
}

/// Runs `act` on a shard of a new host of `sim`, and gives the host.
fn peer<F: Future<Output = ()> + 'static>(
    sim: &mut sim::Sim,
    act: impl FnOnce(sim::node::Node, env::tasks::Tasks) -> F + Send + 'static,
) -> sim::node::Node {
    let host = sim.node(sim::node::Config::default());
    shard(&host, act);
    host
}

/// Stops `node`, runs `sim` to its end, and checks that the node stopped cleanly.
fn stop(sim: &mut sim::Sim, node: Node) {
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(node.join(), Ok(()));
}

/// With a message limit of 2048 bytes, a program sees the limit on its stream, a send
/// of 2049 bytes fails, and one of 2048 bytes goes.
#[test]
fn a_message_over_the_setting_is_refused() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let settings = Settings {
        message_bytes_max: NonZeroUsize::new(2048).unwrap(),
        ..Settings::default()
    };
    let (host, node) = start(&mut sim, settings);
    let listen = listen(&host);
    let out = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&out);
    peer(&mut sim, move |own, tasks| async move {
        let (program, pool) = program(&own, tasks);
        let at = [Address::Udp(listen)];
        let session = program.dial(KEY.public(), &at).await.expect("a session");
        let (mut sender, _reply) =
            session.open(Class::Complete).await.expect("a stream");
        let over = sender.send(pool.copy(&[0; 2049]).unwrap()).await;
        let at = sender.send(pool.copy(&[0; 2048]).unwrap()).await;
        *seen.lock().unwrap() = Some((sender.bytes_max(), over, at));
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    stop(&mut sim, node);
    let over = transport::Error::TooLarge {
        bytes: 2049,
        bytes_max: 2048,
    };
    assert_eq!(*out.lock().unwrap(), Some((2048, Err(over), Ok(()))));
}

/// How long a program waits to open a third two-way stream, after it opened two and
/// sent one message on each, to a node of `streams_max` over links of 50 ms.
fn third_open(streams_max: u32) -> Span {
    let mut sim = sim::Sim::new(sim::Config::default());
    let settings = Settings {
        streams_max: NonZeroU32::new(streams_max).unwrap(),
        ..Settings::default()
    };
    let (host, node) = start(&mut sim, settings);
    let listen = listen(&host);
    let out = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&out);
    let program_host = peer(&mut sim, move |own, tasks| async move {
        let (program, pool) = program(&own, tasks);
        let at = [Address::Udp(listen)];
        let session = program.dial(KEY.public(), &at).await.expect("a session");
        let mut held = Vec::new();
        for _ in 0..2 {
            let (mut sender, reply) =
                session.open(Class::Complete).await.expect("open");
            sender
                .send(pool.copy(b"a").unwrap())
                .await
                .expect("it sends");
            held.push((sender, reply));
        }
        let start = own.clock().now();
        let third = session.open(Class::Complete).await.expect("a third stream");
        *seen.lock().unwrap() = Some(own.clock().now() - start);
        drop((third, held, session, program));
    });
    let link = sim::link::Config {
        delay: Span::from_nanos(50 * Span::MILLISECOND.nanos()),
        ..sim::link::Config::default()
    };
    sim.link(&program_host, &host, link);
    sim.link(&host, &program_host, link);
    assert_eq!(
        sim.run_for(Span::from_nanos(5 * Span::SECOND.nanos())),
        Ok(())
    );
    stop(&mut sim, node);
    out.lock().unwrap().take().expect("the third stream opens")
}

/// A program that opens one stream more than the setting allows waits for credit:
/// the node frees a stream that it rejects, at least one round trip of 100 ms later.
/// Under a setting that allows the third, it opens at once.
#[test]
fn a_stream_over_the_setting_waits_for_credit() {
    let waited = third_open(2);
    assert!(
        waited >= Span::from_nanos(100 * Span::MILLISECOND.nanos()),
        "{waited:?}"
    );
    assert_eq!(third_open(3), Span::ZERO);
}

/// Why and when, from the dial, the session of a program ends, if it does within
/// 10 s after the links between it and a node of `idle` are cut for `cut`, from 1 s
/// after the dial.
fn after_cut(idle: Span, cut: Span) -> Option<(transport::Error, Span)> {
    let mut sim = sim::Sim::new(sim::Config::default());
    let settings = Settings {
        idle,
        ..Settings::default()
    };
    let (host, node) = start(&mut sim, settings);
    let listen = listen(&host);
    let out = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&out);
    let program_host = peer(&mut sim, move |own, tasks| async move {
        let (program, _pool) = program(&own, tasks);
        let at = [Address::Udp(listen)];
        let start = own.clock().now();
        let session = program.dial(KEY.public(), &at).await.expect("a session");
        let error = session.closed().await;
        *seen.lock().unwrap() = Some((error, own.clock().now() - start));
        drop(program);
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    let cut_link = sim::link::Config {
        loss: 1.0,
        ..sim::link::Config::default()
    };
    sim.link(&program_host, &host, cut_link);
    sim.link(&host, &program_host, cut_link);
    assert_eq!(sim.run_for(cut), Ok(()));
    sim.link(&program_host, &host, sim::link::Config::default());
    sim.link(&host, &program_host, sim::link::Config::default());
    assert_eq!(
        sim.run_for(Span::from_nanos(10 * Span::SECOND.nanos())),
        Ok(())
    );
    let ended = out.lock().unwrap().take();
    stop(&mut sim, node);
    ended
}

/// Under an idle time of 2 s, a program whose links are cut for 3 s finds its session
/// timed out within 2 s of the cut. One cut for 1 s keeps its session. The program's
/// own idle time is 30 s, so the node's setting decides each.
#[test]
fn a_session_silent_past_the_idle_setting_closes() {
    let idle = Span::from_nanos(2 * Span::SECOND.nanos());
    let (error, at) = after_cut(idle, Span::from_nanos(3 * Span::SECOND.nanos()))
        .expect("the session ends");
    assert_eq!(error, transport::Error::TimedOut);
    let (from, to) = (Span::SECOND.nanos(), 3 * Span::SECOND.nanos());
    assert!((from..=to).contains(&at.nanos()), "ended at {at:?}");
    assert_eq!(after_cut(idle, Span::SECOND), None);
}

/// The error that `join` gives for a node with `settings`, which the transport
/// refuses after the claim.
fn refused(settings: Settings) -> Error {
    let mut sim = sim::Sim::new(sim::Config::default());
    let (host, node) = start(&mut sim, settings);
    assert_eq!(sim.run(), Ok(()));
    let e = node.join().expect_err("the transport refuses the settings");
    let made = [
        "budget", "lock", "name", "node.key", "shard-0", "shard-1", "shards-2",
    ];
    assert_eq!(listed(&mut sim, &host, ""), made.map(PathBuf::from), "{e}");
    e
}

/// An idle time of zero, a message limit under 1472 bytes, and a window smaller than
/// twice the message limit each stop the node: `Node::join` gives `Error::Transport`.
#[test]
fn a_value_that_the_transport_refuses_stops_the_node() {
    let cases = [
        (
            Settings {
                idle: Span::ZERO,
                ..Settings::default()
            },
            "idle",
            "must be positive",
        ),
        (
            Settings {
                message_bytes_max: NonZeroUsize::new(1471).unwrap(),
                ..Settings::default()
            },
            "message_bytes_max",
            "must be at least 1472",
        ),
        (
            Settings {
                window_bytes: (2 << 16) - 1,
                ..Settings::default()
            },
            "window_bytes",
            "must be at least twice the message limit",
        ),
    ];
    for (settings, field, rule) in cases {
        let error = transport::Error::Config { field, rule };
        assert_eq!(refused(settings), Error::Transport(error));
    }
    let error = transport::Error::Config {
        field: "window_bytes",
        rule: "must be at least twice the message limit",
    };
    assert_eq!(
        Error::Transport(error).to_string(),
        "the node's transport refused its settings: config window_bytes must be at \
         least twice the message limit"
    );
}

/// The defaults that `foundation` runs with.
#[test]
fn the_defaults_are_the_values_that_foundation_runs_with() {
    let settings = Settings {
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(64).unwrap(),
        idle: Span::from_nanos(30 * Span::SECOND.nanos()),
        message_bytes_max: NonZeroUsize::new(1 << 16).unwrap(),
        header_wait: Span::from_nanos(10 * Span::SECOND.nanos()),
        places_max: NonZeroUsize::new(256).unwrap(),
        floor_bytes: 1 << 30,
    };
    assert_eq!(Settings::default(), settings);
}

/// How long after its send a program waits for the reset of a stream whose header of
/// 60 KiB takes about 16 s on a link of 4 KB/s, to a node of `header_wait`.
fn late(header_wait: Span) -> Span {
    let mut sim = sim::Sim::new(sim::Config::default());
    let settings = Settings {
        header_wait,
        ..Settings::default()
    };
    let (host, node) = start(&mut sim, settings);
    let listen = listen(&host);
    let out = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&out);
    let program_host = peer(&mut sim, move |own, tasks| async move {
        let (transport, pool) = transport(&own, tasks, CLIENT);
        let at = [Address::Udp(listen)];
        let session = transport.dial(KEY.public(), &at).await.expect("a session");
        let mut late = wire::header::encode(wire::Protocol::Mesh).to_vec();
        late.resize(60 << 10, 0);
        let (mut sender, mut reply) =
            session.open(Class::Complete).await.expect("a stream");
        let start = own.clock().now();
        sender
            .send(pool.copy(&late).unwrap())
            .await
            .expect("it sends");
        let read = reply.recv().await.map(|m| m.map(|b| b.to_vec()));
        *seen.lock().unwrap() = Some((own.clock().now() - start, read));
        drop((sender, session, transport));
    });
    let slow = sim::link::Config {
        rate: Some(std::num::NonZeroU64::new(4_000).unwrap()),
        ..sim::link::Config::default()
    };
    sim.link(&program_host, &host, slow);
    assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
    stop(&mut sim, node);
    let (waited, read) = out.lock().unwrap().take().expect("the program reads");
    let code = Code(wire::header::REJECTED);
    assert_eq!(read, Err(transport::Error::Reset { code }));
    waited
}

/// The node rejects a stream whose header has not arrived within the header setting:
/// 3 s to 4 s after the send under 3 s, and within 1 s under zero.
#[test]
fn a_header_late_past_the_setting_is_rejected() {
    let waited = late(Span::from_nanos(3 * Span::SECOND.nanos()));
    let (from, to) = (3 * Span::SECOND.nanos(), 4 * Span::SECOND.nanos());
    assert!(
        (from..to).contains(&waited.nanos()),
        "reset after {waited:?}"
    );
    let waited = late(Span::ZERO);
    assert!(waited < Span::SECOND, "reset after {waited:?}");
}

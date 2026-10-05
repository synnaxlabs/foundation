//! Tests of the simulated serial lines through `env::serial`.

use std::collections::BTreeSet;
use std::future::{pending, poll_fn};
use std::num::NonZeroU32;
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use env::serial::{Config, Error, Parity, Settings, StopBits};
use env::thread::Handle;
use types::time::{Monotonic, Span};

use super::{millis, shard};
use crate::{Crash, Sim, line, node};

/// The path of the end of each line on node `a`.
const A: &str = "/dev/ttyS0";
/// The path of the end of each line on node `b`.
const B: &str = "/dev/ttyUSB0";

/// Reads: the reader's clock and the bytes of each read.
type Log = Arc<Mutex<Vec<(Monotonic, Vec<u8>)>>>;

/// The settings of the port at `path` at `baud`, with `parity` and one stop bit.
fn config(path: &str, baud: u32, parity: Option<Parity>) -> Config {
    Config {
        path: path.into(),
        settings: Settings {
            baud: NonZeroU32::new(baud).unwrap(),
            parity,
            stop_bits: StopBits::One,
        },
    }
}

/// Opens `config` on `node` outside the sim, where an open ends at its first poll.
fn open(node: &node::Node, config: &Config) -> Result<env::serial::Port, Error> {
    let serial = node.serial();
    let open = pin!(serial.open(config));
    let cx = &mut Context::from_waker(Waker::noop());
    let Poll::Ready(port) = open.poll(cx) else {
        panic!("an open of {} waits", config.path.display())
    };
    port
}

/// A run of two nodes, `a` and `b`, with `line` between [`A`] on `a` and [`B`] on
/// `b`.
fn pair(seed: u64, line: line::Config) -> (Sim, node::Node, node::Node) {
    let mut sim = Sim::new(crate::Config {
        seed,
        steps_max: 1_000_000,
        ..crate::Config::default()
    });
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    sim.line(&a, Path::new(A), &b, Path::new(B), line);
    (sim, a, b)
}

/// The node's clock `span` after the run starts.
fn after(span: Span) -> Monotonic {
    node::Config::default().monotonic + span
}

/// Writes all of `bytes`, a part at a time when the send queue fills.
async fn write(port: &mut env::serial::Port, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        sent += poll_fn(|cx| port.poll_write(cx, &bytes[sent..]))
            .await
            .unwrap();
    }
}

/// Starts a shard on `node` that opens `config`, writes `bytes`, and holds the
/// port open.
fn send(node: &node::Node, config: Config, bytes: Vec<u8>) -> Handle {
    let serial = node.serial();
    let handle = node.shards().start(shard("send"), move |_| async move {
        let mut port = serial.open(&config).await.unwrap();
        write(&mut port, &bytes).await;
        pending::<()>().await;
    });
    handle.unwrap()
}

/// Starts a shard on `node` that waits for `wait`, opens `config`, and reads into
/// `log` forever.
fn receive(node: &node::Node, config: Config, wait: Span, log: &Log) -> Handle {
    let (clock, serial, log) = (node.clock(), node.serial(), Arc::clone(log));
    let handle = node.shards().start(shard("receive"), move |_| async move {
        clock.sleep(wait).await;
        let mut port = serial.open(&config).await.unwrap();
        loop {
            let mut buffer = [0; 8_192];
            let n = poll_fn(|cx| port.poll_read(cx, &mut buffer)).await.unwrap();
            log.lock()
                .unwrap()
                .push((clock.now(), buffer[..n].to_vec()));
        }
    });
    handle.unwrap()
}

/// The bytes in `log`, in order.
fn received(log: &Log) -> Vec<u8> {
    let log = log.lock().unwrap();
    log.iter().flat_map(|(_, bytes)| bytes.clone()).collect()
}

/// Sends `bytes` from [`A`] at `writer` to [`B`] at `reader` on one line, runs for
/// a second, and gives the run and the log of `b`.
fn exchange(
    seed: u64,
    line: line::Config,
    writer: Config,
    reader: Config,
    bytes: Vec<u8>,
) -> (Sim, Log) {
    let (mut sim, a, b) = pair(seed, line);
    let log = Log::default();
    let _send = send(&a, writer, bytes);
    let _receive = receive(&b, reader, Span::ZERO, &log);
    sim.run_for(Span::SECOND).unwrap();
    (sim, log)
}

/// Sends the bytes 0 to 199 from [`A`] to [`B`], both at 9,600 baud with `parity`,
/// and gives the digest and the bytes that arrived.
fn noisy(seed: u64, line: line::Config, parity: Option<Parity>) -> (u64, Vec<u8>) {
    let (writer, reader) = (config(A, 9_600, parity), config(B, 9_600, parity));
    let (sim, log) = exchange(seed, line, writer, reader, (0..200).collect());
    (sim.digest(), received(&log))
}

/// Whether `part` is `whole` with some bytes taken out.
fn subsequence(part: &[u8], whole: &[u8]) -> bool {
    let mut whole = whole.iter();
    part.iter().all(|byte| whole.any(|other| other == byte))
}

#[test]
fn bytes_arrive_in_order_at_the_line_rate() {
    let cases = [
        (None, StopBits::One, 10),
        (Some(Parity::Even), StopBits::One, 11),
        (Some(Parity::Odd), StopBits::Two, 12),
    ];
    for (parity, stop_bits, bits) in cases {
        let settings = Settings {
            stop_bits,
            ..config(A, 9_600, parity).settings
        };
        let writer = Config {
            path: A.into(),
            settings,
        };
        let reader = Config {
            path: B.into(),
            settings,
        };
        let line = line::Config::default();
        let (_sim, log) = exchange(0, line, writer, reader, (0..100).collect());
        let expected: Vec<(Monotonic, Vec<u8>)> = (0..100u8)
            .map(|i| {
                let nanos = (i64::from(i) + 1) * bits * 1_000_000_000 / 9_600;
                (after(Span::from_nanos(nanos)), vec![i])
            })
            .collect();
        assert_eq!(*log.lock().unwrap(), expected, "{bits} bits");
    }
}

#[test]
fn a_long_run_of_bytes_keeps_the_exact_line_rate() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let bytes: Vec<u8> = (0..2_000u32).map(|i| i.to_le_bytes()[0]).collect();
    let log = Log::default();
    let _send = send(&a, config(A, 9_600, None), bytes.clone());
    let _receive = receive(&b, config(B, 9_600, None), Span::ZERO, &log);
    sim.run_for(Span::from_nanos(3 * Span::SECOND.nanos()))
        .unwrap();
    let expected: Vec<(Monotonic, Vec<u8>)> = (1..=2_000)
        .zip(bytes)
        .map(|(k, byte)| {
            let nanos = k * 10 * 1_000_000_000 / 9_600;
            (after(Span::from_nanos(nanos)), vec![byte])
        })
        .collect();
    assert_eq!(*log.lock().unwrap(), expected);
}

/// Starts a shard on `node` that opens `path` at 19,200 baud, writes `bytes`, and
/// reads into `log` forever.
fn duplex(node: &node::Node, path: &str, bytes: Vec<u8>, log: &Log) -> Handle {
    let (clock, serial, log) = (node.clock(), node.serial(), Arc::clone(log));
    let config = config(path, 19_200, None);
    let handle = node.shards().start(shard(path), move |_| async move {
        let mut port = serial.open(&config).await.unwrap();
        write(&mut port, &bytes).await;
        loop {
            let mut buffer = [0; 16];
            let n = poll_fn(|cx| port.poll_read(cx, &mut buffer)).await.unwrap();
            log.lock()
                .unwrap()
                .push((clock.now(), buffer[..n].to_vec()));
        }
    });
    handle.unwrap()
}

#[test]
fn a_line_carries_bytes_both_ways_at_once() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let (a_log, b_log) = (Log::default(), Log::default());
    let _a = duplex(&a, A, vec![1, 2, 3], &a_log);
    let _b = duplex(&b, B, vec![4, 5, 6], &b_log);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(received(&a_log), [4, 5, 6]);
    assert_eq!(received(&b_log), [1, 2, 3]);
}

#[test]
fn a_write_past_the_send_queue_waits_for_room() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let bytes: Vec<u8> = (0..5_000u32).map(|i| i.to_le_bytes()[0]).collect();
    let counts = Arc::new(Mutex::new(Vec::new()));
    let (serial, log, sent) = (a.serial(), Log::default(), Arc::clone(&counts));
    let all = bytes.clone();
    let _send = a.shards().start(shard("send"), move |_| async move {
        let mut port = serial.open(&config(A, 1_000_000, None)).await.unwrap();
        let mut done = 0;
        while done < all.len() {
            let n = poll_fn(|cx| port.poll_write(cx, &all[done..]))
                .await
                .unwrap();
            sent.lock().unwrap().push(n);
            done += n;
        }
        pending::<()>().await;
    });
    let _receive = receive(&b, config(B, 1_000_000, None), Span::ZERO, &log);
    sim.run_for(Span::SECOND).unwrap();
    let counts = counts.lock().unwrap().clone();
    assert_eq!(counts[0], 4_096, "the first write fills the send queue");
    assert_eq!(
        counts.len(),
        1 + 5_000 - 4_096,
        "each arrival makes room for one"
    );
    assert_eq!(received(&log), bytes);
    let last = log.lock().unwrap().last().unwrap().0;
    assert_eq!(
        last,
        after(Span::from_nanos(5_000 * 10_000)),
        "no gap in the line"
    );
}

#[test]
fn an_end_keeps_at_most_four_kib_not_read() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let bytes: Vec<u8> = (0..5_000u32).map(|i| i.to_le_bytes()[0]).collect();
    let _send = send(&a, config(A, 1_000_000, None), bytes.clone());
    let (clock, serial, out) =
        (b.clock(), b.serial(), Arc::new(Mutex::new(Vec::new())));
    let slot = Arc::clone(&out);
    let _receive = b.shards().start(shard("receive"), move |_| async move {
        let mut port = serial.open(&config(B, 1_000_000, None)).await.unwrap();
        clock.sleep(Span::SECOND).await;
        let mut buffer = vec![0; 8_192];
        let n = poll_fn(|cx| port.poll_read(cx, &mut buffer)).await.unwrap();
        slot.lock().unwrap().extend_from_slice(&buffer[..n]);
    });
    sim.run_for(Span::from_nanos(2 * Span::SECOND.nanos()))
        .unwrap();
    assert_eq!(*out.lock().unwrap(), bytes[..4_096]);
}

#[test]
fn a_lossy_line_loses_bytes_by_the_seed() {
    let loss = line::Config {
        loss: 0.1,
        ..line::Config::default()
    };
    let (digest, bytes) = noisy(1, loss, None);
    let all: Vec<u8> = (0..200).collect();
    assert!(subsequence(&bytes, &all), "{bytes:?}");
    assert!(
        (150..195).contains(&bytes.len()),
        "lost {}",
        200 - bytes.len()
    );
    assert_eq!(noisy(1, loss, None), (digest, bytes));
    let runs: BTreeSet<Vec<u8>> =
        (0..8).map(|seed| noisy(seed, loss, None).1).collect();
    assert!(runs.len() > 1, "each seed lost the same bytes");
}

#[test]
fn a_flip_changes_one_bit_with_no_parity() {
    let flip = line::Config {
        flip: 0.1,
        ..line::Config::default()
    };
    let (digest, bytes) = noisy(2, flip, None);
    assert_eq!(bytes.len(), 200);
    let flips: Vec<u32> = (bytes.iter().zip(0u8..))
        .map(|(byte, sent)| (byte ^ sent).count_ones())
        .collect();
    assert!(flips.iter().all(|&bits| bits <= 1), "{flips:?}");
    assert!(flips.contains(&1), "no byte changed");
    assert_eq!(noisy(2, flip, None), (digest, bytes));
}

#[test]
fn a_flip_loses_the_byte_with_parity() {
    let flip = line::Config {
        flip: 0.1,
        ..line::Config::default()
    };
    for parity in [Some(Parity::Even), Some(Parity::Odd)] {
        let (_, bytes) = noisy(2, flip, parity);
        let all: Vec<u8> = (0..200).collect();
        assert!(subsequence(&bytes, &all), "{bytes:?}");
        assert!(bytes.len() < 200, "no byte lost");
    }
}

#[test]
fn a_line_with_loss_one_is_cut_until_set_again() {
    let cut = line::Config {
        loss: 1.0,
        ..line::Config::default()
    };
    let (mut sim, a, b) = pair(0, cut);
    let (clock, serial, log) = (a.clock(), a.serial(), Log::default());
    let _send = a.shards().start(shard("send"), move |_| async move {
        let mut port = serial.open(&config(A, 9_600, None)).await.unwrap();
        write(&mut port, &[1, 2, 3]).await;
        clock.sleep(Span::SECOND).await;
        write(&mut port, &[4, 5, 6]).await;
        pending::<()>().await;
    });
    let _receive = receive(&b, config(B, 9_600, None), Span::ZERO, &log);
    sim.run_for(millis(500)).unwrap();
    assert!(received(&log).is_empty());
    sim.line(&a, Path::new(A), &b, Path::new(B), line::Config::default());
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(received(&log), [4, 5, 6]);
}

#[test]
fn a_line_set_again_keeps_the_fate_of_the_bytes_in_flight() {
    let lossy = line::Config {
        loss: 0.3,
        ..line::Config::default()
    };
    let run = |again: bool| {
        let (mut sim, a, b) = pair(0, lossy);
        let log = Log::default();
        let _send = send(&a, config(A, 9_600, None), (0..100).collect());
        let _receive = receive(&b, config(B, 19_200, None), Span::ZERO, &log);
        sim.run_for(millis(50)).unwrap();
        if again {
            sim.line(&a, Path::new(A), &b, Path::new(B), lossy);
        }
        sim.run_for(Span::SECOND).unwrap();
        received(&log)
    };
    let kept = run(false);
    assert!((1..100).contains(&kept.len()), "{} bytes", kept.len());
    assert_eq!(run(true), kept);
}

#[test]
fn ends_with_other_settings_get_other_bytes() {
    let (writer, reader) = (config(A, 9_600, None), config(B, 19_200, None));
    let sent: Vec<u8> = (0..100).collect();
    let (_sim, log) =
        exchange(0, line::Config::default(), writer, reader, sent.clone());
    let bytes = received(&log);
    assert_eq!(bytes.len(), 100);
    assert_ne!(bytes, sent);
    let last = log.lock().unwrap().last().unwrap().0;
    assert_eq!(
        last,
        after(Span::from_nanos(100 * 10 * 1_000_000_000 / 9_600))
    );
}

#[test]
fn bytes_that_arrive_at_a_closed_end_are_lost() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let (clock, serial, log) = (a.clock(), a.serial(), Log::default());
    let _send = a.shards().start(shard("send"), move |_| async move {
        let mut port = serial.open(&config(A, 9_600, None)).await.unwrap();
        write(&mut port, &[1, 2, 3]).await;
        clock.sleep(Span::SECOND).await;
        write(&mut port, &[4, 5, 6]).await;
        pending::<()>().await;
    });
    let _receive = receive(&b, config(B, 9_600, None), millis(500), &log);
    sim.run_for(Span::from_nanos(2 * Span::SECOND.nanos()))
        .unwrap();
    assert_eq!(received(&log), [4, 5, 6]);
}

#[test]
fn a_dropped_port_loses_the_bytes_not_yet_arrived() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    let (clock, serial, log) = (a.clock(), a.serial(), Log::default());
    let _send = a.shards().start(shard("send"), move |_| async move {
        let mut port = serial.open(&config(A, 9_600, None)).await.unwrap();
        write(&mut port, &(0..100).collect::<Vec<u8>>()).await;
        clock.sleep(millis(10)).await;
    });
    let _receive = receive(&b, config(B, 9_600, None), Span::ZERO, &log);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(received(&log), (0..9).collect::<Vec<u8>>());
}

#[test]
fn an_open_of_a_path_with_no_line_is_not_found() {
    let (_sim, a, b) = pair(0, line::Config::default());
    let missing = |node: &node::Node, path: &str| {
        open(node, &config(path, 9_600, None)).unwrap_err()
    };
    let path = "/dev/ttyS9";
    assert_eq!(missing(&a, path), Error::NotFound { path: path.into() });
    assert_eq!(missing(&b, A), Error::NotFound { path: A.into() });
}

#[test]
fn an_end_opens_once_until_its_port_drops() {
    let (_sim, a, _b) = pair(0, line::Config::default());
    let config = config(A, 9_600, None);
    let first = open(&a, &config).unwrap();
    let busy = open(&a, &config).unwrap_err();
    assert_eq!(busy, Error::Busy { path: A.into() });
    drop(first);
    open(&a, &config).unwrap();
}

#[test]
fn a_crash_closes_the_ports_of_its_node() {
    let (mut sim, a, _b) = pair(0, line::Config::default());
    let serial = a.serial();
    let held = a.shards().start(shard("hold"), move |_| async move {
        let _port = serial.open(&config(A, 9_600, None)).await.unwrap();
        pending::<()>().await;
    });
    drop(held.unwrap());
    sim.run_for(millis(1)).unwrap();
    sim.crash(&a, Crash::Process);
    open(&a, &config(A, 9_600, None)).unwrap();
}

#[test]
fn the_digest_holds_the_fate_of_each_byte() {
    let lost = line::Config {
        loss: 1.0,
        ..line::Config::default()
    };
    let (kept, _) = noisy(0, line::Config::default(), None);
    assert_ne!(noisy(0, lost, None).0, kept);
}

#[test]
#[should_panic(expected = "port /dev/ttyS0 of node 0 cannot be both ends of a line")]
fn a_line_from_an_end_to_itself_panics() {
    let mut sim = Sim::new(crate::Config::default());
    let a = sim.node(node::Config::default());
    sim.line(&a, Path::new(A), &a, Path::new(A), line::Config::default());
}

#[test]
#[should_panic(expected = "port /dev/ttyUSB0 of node 1 is on another line")]
fn a_line_to_an_end_of_another_line_panics() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    sim.line(
        &a,
        Path::new("/dev/ttyS1"),
        &b,
        Path::new(B),
        line::Config::default(),
    );
}

#[test]
fn a_line_set_again_from_its_other_end_keeps_its_ends() {
    let (mut sim, a, b) = pair(0, line::Config::default());
    sim.line(&b, Path::new(B), &a, Path::new(A), line::Config::default());
    open(&a, &config(A, 9_600, None)).unwrap();
}

#[test]
#[should_panic(expected = "has a chance outside 0 to 1")]
fn a_line_with_a_bad_chance_panics() {
    let flip = line::Config {
        flip: 1.5,
        ..line::Config::default()
    };
    pair(0, flip);
}

/// Reads once with a context that never wakes.
fn read_once(port: &mut env::serial::Port) {
    let cx = &mut Context::from_waker(Waker::noop());
    assert_eq!(port.poll_read(cx, &mut [0; 8]), Poll::Pending);
}

#[test]
fn a_port_polled_on_a_second_thread_panics() {
    let (mut sim, a, _b) = pair(0, line::Config::default());
    let mut port = open(&a, &config(A, 9_600, None)).unwrap();
    let slot = Arc::new(Mutex::new(None));
    let give = Arc::clone(&slot);
    let _first = a.shards().start(shard("first"), move |_| async move {
        read_once(&mut port);
        *give.lock().unwrap() = Some(port);
    });
    sim.run().unwrap();
    let mut port = slot.lock().unwrap().take().unwrap();
    let _second = a.shards().start(shard("second"), move |_| async move {
        read_once(&mut port);
    });
    let message = "a serial port polls only on thread \"first\" of its first poll";
    assert_eq!(
        sim.run().unwrap_err(),
        crate::Error::Panicked {
            thread: "second".into(),
            message: message.into(),
            seed: 0,
        }
    );
}

#[test]
#[should_panic(expected = "a serial port needs a thread that the sim started")]
fn a_port_polled_outside_the_sim_panics() {
    let (_sim, a, _b) = pair(0, line::Config::default());
    let mut port = open(&a, &config(A, 9_600, None)).unwrap();
    read_once(&mut port);
}

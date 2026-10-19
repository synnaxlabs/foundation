//! The Rust SDK against a node process.

use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use env::tasks::{Group, Tasks};
use sdk::write::{Config, Outcome, Refusal as Refused};
use sdk::{Client, Error, Node, Subject};
use types::ed25519::PrivateKey;
use types::frame::Range;
use types::name::Selector;
use types::sample::{Scalar, Type, Value};
use types::time::Stamp;
use wire::hub::client::{Mode, Refusal};

use crate::plant::{admin, planted, session};
use crate::rig::Rig;

/// The samples of one frame, by channel name, each as `i64`.
type Samples = BTreeMap<String, Vec<i64>>;

/// Opens a reader of each channel that `select` matches at `node` as `subject`, in a
/// session on a thread of its own, and gives the samples of each of the next `frames`
/// frames that it reads. The reader gets only the frames that commit after this call
/// returns.
fn reader(
    node: &Node,
    subject: &Subject,
    select: &str,
    frames: usize,
) -> mpsc::Receiver<Samples> {
    let (node, subject) = (node.clone(), subject.clone());
    let select = Selector::new([select]).unwrap();
    let (sender, opened) = mpsc::sync_channel(1);
    let (samples, read) = mpsc::channel();
    let config = env::shards::Config {
        name: "reader".into(),
        core: None,
    };
    let main = move |tasks: Tasks| async move {
        let group = Group::new(tasks);
        let session = session(node, subject, group.tasks().clone()).await;
        let mut reader = session.reader(select, Mode::Complete).await.unwrap();
        sender.send(()).unwrap();
        for _ in 0..frames {
            let frame = reader.next().await.unwrap();
            let count = usize::try_from(frame.range(0).unwrap().count).unwrap();
            let read = frame.iter().map(|(entry, bytes)| {
                let place = &reader.places()[entry];
                let mut out = Vec::new();
                codec::decode(place.data_type, count, bytes, &mut out).unwrap();
                let (chunks, _) = out.as_chunks::<8>();
                let values = chunks.iter().map(|chunk| i64::from_le_bytes(*chunk));
                (place.name.to_string(), values.collect())
            });
            samples.send(read.collect()).unwrap();
        }
        drop((reader, session));
        group.ended().await;
    };
    drop(os::shards().unwrap().start(config, main).unwrap());
    wait(&opened);
    read
}

/// The next of `answers`. Panics when its sender drops, such as at a panic of the
/// reader's thread, or when 30 s pass first.
#[expect(
    clippy::disallowed_methods,
    reason = "the test waits for the thread of a reader in real time"
)]
fn wait<T>(answers: &mpsc::Receiver<T>) -> T {
    answers
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the reader answers")
}

/// The wall time now, in ns since the Unix epoch.
#[expect(
    clippy::disallowed_methods,
    reason = "the test checks a stamp of a node process against the wall time"
)]
fn now() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    i64::try_from(since.unwrap().as_nanos()).unwrap()
}

/// The outcome of a frame of `count` samples that the node took at `seq`.
fn taken(seq: u64, count: u32) -> Outcome {
    Outcome::Taken {
        range: Range { seq, count },
    }
}

#[test]
fn a_client_connects_to_a_node_and_connects_again_after_its_close() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let client = Client::connect(&node, subject.clone()).unwrap();
    assert_eq!(client.close(), Ok(()));
    let client = Client::connect(&node, subject).unwrap();
    drop(client);
    rig.stop();
}

/// A UDP relay to `node` for one client. Gives its address and the count of the
/// datagrams that the client sent through it.
#[expect(
    clippy::disallowed_methods,
    reason = "a relay between a client and a node"
)]
fn relay(node: SocketAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let front = UdpSocket::bind((node.ip(), 0)).unwrap();
    let back = UdpSocket::bind((node.ip(), 0)).unwrap();
    back.connect(node).unwrap();
    let address = front.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let (to_client, from_node) =
        (front.try_clone().unwrap(), back.try_clone().unwrap());
    let (client, counted) = std::sync::mpsc::channel();
    let sent = Arc::clone(&count);
    std::thread::spawn(move || {
        let mut buffer = vec![0; 1 << 16];
        while let Ok((len, from)) = front.recv_from(&mut buffer) {
            if sent.fetch_add(1, Ordering::SeqCst) == 0 {
                client.send(from).unwrap();
            }
            back.send(&buffer[..len]).unwrap();
        }
    });
    std::thread::spawn(move || {
        let client = counted.recv().unwrap();
        let mut buffer = vec![0; 1 << 16];
        while let Ok(len) = from_node.recv(&mut buffer) {
            to_client.send_to(&buffer[..len], client).unwrap();
        }
    });
    (address, count)
}

#[test]
fn the_close_of_a_client_sends_the_close_to_the_node() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let (front, count) = relay(node.addresses[0]);
    let node = Node {
        addresses: vec![front],
        ..node
    };
    let client = Client::connect(&node, subject).unwrap();
    let before = count.load(Ordering::SeqCst);
    assert_eq!(client.close(), Ok(()));
    rig.wait("a datagram after the close", || {
        let after = count.load(Ordering::SeqCst);
        if after > before {
            Ok(())
        } else {
            Err(format!("{after} datagrams"))
        }
    });
    rig.stop();
}

/// The node sends its close before it exits. The client takes it in real time, so the
/// test opens writers until an open gives the end of the session.
#[test]
fn the_close_of_a_client_gives_the_cause_when_the_node_closed_the_session() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let client = Client::connect(&node, subject).unwrap();
    rig.stop();
    rig.wait("an open gives the end of the session", || {
        match client.open_writer(Config::new(vec!["plant.a".parse().unwrap()])) {
            Err(Error::Session(_)) => Ok(()),
            other => Err(format!("{:?}", other.map(drop))),
        }
    });
    let closed = transport::Error::PeerClosed {
        code: transport::Code(0),
    };
    assert_eq!(
        client.close(),
        Err(Error::Session(program::Error::Transport(closed)))
    );
}

#[test]
fn a_client_dials_the_next_address_when_the_first_does_not_answer() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    #[expect(clippy::disallowed_methods, reason = "a socket that never reads")]
    let silent = std::net::UdpSocket::bind((node.addresses[0].ip(), 0)).unwrap();
    let (front, count) = relay(node.addresses[0]);
    let node = Node {
        addresses: vec![silent.local_addr().unwrap(), front],
        ..node
    };
    let client = Client::connect(&node, subject).unwrap();
    assert_ne!(count.load(Ordering::SeqCst), 0);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

#[test]
fn a_client_with_a_wrong_public_key_gets_the_error_of_the_handshake() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        public_key: peer,
        ..node
    };
    let error = Client::connect(&node, subject).unwrap_err();
    let unreachable = transport::Error::Unreachable {
        peer,
        attempts: vec![(
            transport::Address::Udp(node.addresses[0]),
            transport::Error::Authentication { expected: peer },
        )],
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
    rig.stop();
}

#[test]
fn a_client_of_a_subject_that_the_spec_does_not_list_is_refused() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let subject = Subject {
        name: "plant.operator".parse().unwrap(),
        ..subject
    };
    let error = Client::connect(&node, subject).unwrap_err();
    assert_eq!(
        error,
        Error::Session(program::Error::Refused(Refusal::Refused))
    );
    assert_eq!(error.to_string(), Refusal::Refused.to_string());
    rig.stop();
}

#[test]
fn a_client_of_an_address_that_no_node_reads_is_unreachable() {
    #[expect(clippy::disallowed_methods, reason = "a socket that never reads")]
    let silent = std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let address = silent.local_addr().unwrap();
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        addresses: vec![address],
        key: types::node::Key::from_u128(1),
        public_key: peer,
    };
    let subject = Subject {
        name: spec::founding::admin(),
        private_key: PrivateKey([8; 32]),
    };
    let error = Client::connect(&node, subject).unwrap_err();
    let unreachable = transport::Error::Unreachable {
        peer,
        attempts: vec![(transport::Address::Udp(address), transport::Error::TimedOut)],
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
}

/// A writer of the defaults writes values with the node's stamp, values with their own
/// stamp, and series with their stamps, in any order of places, and a reader gets each
/// frame in order once the close returns. Its places are its channels in their order,
/// then the index.
#[test]
fn a_writer_writes_values_and_series_that_a_reader_gets_in_order() {
    let (mut rig, node, subject) = planted();
    let read = reader(&node, &subject, "plant.*", 3);
    let client = Client::connect(&node, subject).unwrap();
    let channels = ["plant.b".parse().unwrap(), "plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels.into())).unwrap();
    let names: Vec<String> =
        writer.places().iter().map(|p| p.name.to_string()).collect();
    assert_eq!(names, ["plant.b", "plant.a", "plant.time"]);
    let (b, a, time) = (0, 1, 2);
    assert_eq!(writer.index(), time);
    let values = [(b, Value::I64(2)), (a, Value::I64(1))];
    assert_eq!(writer.write(&values), Ok(taken(0, 1)));
    let first = wait(&read);
    let stamp = first["plant.time"][0];
    assert!(
        (now() - stamp).abs() < 60_000_000_000,
        "{stamp} is near now"
    );
    let samples = |frame: &Samples| {
        let without = frame.iter().filter(|(name, _)| *name != "plant.time");
        without
            .map(|(name, values)| (name.clone(), values.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        samples(&first),
        [("plant.a".into(), vec![1]), ("plant.b".into(), vec![2])]
    );
    let given = Value::Stamp(Stamp::from_nanos(stamp + 5));
    assert_eq!(
        writer.write(&[(a, Value::I64(3)), (time, given)]),
        Ok(taken(1, 1))
    );
    let second = wait(&read);
    assert_eq!(second["plant.time"], [stamp + 5]);
    assert_eq!(samples(&second), [("plant.a".into(), vec![3])]);
    let stamps = [stamp + 10, stamp + 20].map(i64::to_le_bytes).concat();
    let counts = [4_i64, 5].map(i64::to_le_bytes).concat();
    let series = [(b, counts.as_slice()), (time, stamps.as_slice())];
    assert_eq!(writer.write_series(2, &series), Ok(taken(2, 2)));
    assert_eq!(writer.close(), Ok(()));
    let third = wait(&read);
    assert_eq!(third["plant.time"], [stamp + 10, stamp + 20]);
    assert_eq!(samples(&third), [("plant.b".into(), vec![4, 5])]);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A value of another type than its channel gives the exact error with nothing sent,
/// and the writer goes on.
#[test]
fn a_write_of_a_value_of_another_type_sends_nothing_and_the_writer_goes_on() {
    let (mut rig, node, subject) = planted();
    let read = reader(&node, &subject, "plant.a", 1);
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    let a = 0;
    let error = writer.write(&[(a, Value::F64(1.5))]).unwrap_err();
    let value = Error::Value {
        place: a,
        scalar: Scalar::F64,
        data_type: Type::Scalar(Scalar::I64),
    };
    assert_eq!(error, value);
    assert_eq!(error.to_string(), format!("place {a} holds i64, not f64"));
    assert_eq!(writer.write(&[(a, Value::I64(7))]), Ok(taken(0, 1)));
    assert_eq!(wait(&read)["plant.a"], [7]);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

#[test]
fn an_open_of_a_writer_of_an_unknown_name_gives_unknown_and_the_client_goes_on() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.missing".parse().unwrap()];
    let opened = client.open_writer(Config::new(channels)).map(drop);
    let unknown = Error::Stream(program::Error::Refused(Refusal::Unknown));
    assert_eq!(opened, Err(unknown));
    let channels = vec!["plant.a".parse().unwrap()];
    let writer = client.open_writer(Config::new(channels)).unwrap();
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// The node refuses a frame whose series is not `count` samples, and the writer goes
/// on.
#[test]
fn a_write_of_series_whose_bytes_are_not_count_samples_is_refused() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    let (a, time) = (0, writer.index());
    let stamps = [1_i64, 2].map(i64::to_le_bytes).concat();
    let short = 4_i64.to_le_bytes();
    let written = writer.write_series(2, &[(time, &stamps), (a, &short)]);
    assert_eq!(written, Ok(Outcome::Refused(Refused::Codec)));
    assert_eq!(writer.write(&[(a, Value::I64(5))]), Ok(taken(0, 1)));
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// The close of a client with a live writer returns, and each later call of the
/// writer gives `Closed`.
#[test]
fn a_close_of_a_client_with_a_live_writer_returns_and_ends_the_writer() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    let a = 0;
    assert_eq!(writer.write(&[(a, Value::I64(1))]), Ok(taken(0, 1)));
    assert_eq!(client.close(), Ok(()));
    assert_eq!(writer.write(&[(a, Value::I64(2))]), Err(Error::Closed));
    assert_eq!(writer.write_series(1, &[]), Err(Error::Closed));
    assert_eq!(writer.close(), Err(Error::Closed));
    rig.stop();
}

/// A drop of a client does not wait, so a call of its writer gives `Closed` once the
/// client's thread ends the writer.
#[test]
fn a_call_of_a_writer_after_a_drop_of_its_client_gives_closed() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    let a = 0;
    drop(client);
    rig.wait("a write gives closed", || {
        match writer.write(&[(a, Value::I64(1))]) {
            Err(Error::Closed) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    });
    assert_eq!(writer.close(), Err(Error::Closed));
    rig.stop();
}

/// Each write of values with no stamp gets a stamp of the node after the stamp of the
/// write before it.
#[test]
fn the_node_stamps_each_write_of_values_after_the_one_before_it() {
    let (mut rig, node, subject) = planted();
    let read = reader(&node, &subject, "plant.*", 3);
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    for seq in 0..3 {
        assert_eq!(writer.write(&[(0, Value::I64(1))]), Ok(taken(seq, 1)));
    }
    let stamps: Vec<i64> = (0..3).map(|_| wait(&read)["plant.time"][0]).collect();
    assert!(stamps.is_sorted_by(|a, b| a < b), "{stamps:?} increase");
    assert!(
        (now() - stamps[2]).abs() < 60_000_000_000,
        "{stamps:?} are near now"
    );
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A channel given twice is at each of its places, and a write of one of them goes to
/// the channel.
#[test]
fn a_writer_of_a_channel_given_twice_writes_it_at_each_place() {
    let (mut rig, node, subject) = planted();
    let read = reader(&node, &subject, "plant.a", 2);
    let client = Client::connect(&node, subject).unwrap();
    let a: types::name::Name = "plant.a".parse().unwrap();
    let mut writer = client.open_writer(Config::new(vec![a.clone(), a])).unwrap();
    let names: Vec<String> =
        writer.places().iter().map(|p| p.name.to_string()).collect();
    assert_eq!(names, ["plant.a", "plant.a", "plant.time"]);
    assert_eq!(writer.index(), 2);
    let unordered = types::frame::Error::Unordered { entry: 1, last: 0 };
    assert_eq!(
        writer.write(&[(0, Value::I64(3)), (1, Value::I64(4))]),
        Err(Error::Stream(program::Error::Layout(unordered)))
    );
    assert_eq!(writer.write(&[(0, Value::I64(1))]), Ok(taken(0, 1)));
    assert_eq!(writer.write(&[(1, Value::I64(2))]), Ok(taken(1, 1)));
    assert_eq!(wait(&read)["plant.a"], [1]);
    assert_eq!(wait(&read)["plant.a"], [2]);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A writer that names the index gives it at its place.
#[test]
fn a_writer_that_names_the_index_writes_its_stamp_at_that_place() {
    let (mut rig, node, subject) = planted();
    let read = reader(&node, &subject, "plant.*", 1);
    let client = Client::connect(&node, subject).unwrap();
    let channels = ["plant.time".parse().unwrap(), "plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels.into())).unwrap();
    assert_eq!(writer.index(), 0);
    assert_eq!(writer.places().len(), 2);
    let stamp = now();
    let values = [
        (1, Value::I64(4)),
        (0, Value::Stamp(Stamp::from_nanos(stamp))),
    ];
    assert_eq!(writer.write(&values), Ok(taken(0, 1)));
    let frame = wait(&read);
    assert_eq!(
        (&frame["plant.time"], &frame["plant.a"]),
        (&vec![stamp], &vec![4])
    );
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A write of no values, a place past the places, a place given twice, series with
/// none of the index, and a series past the largest block of the frames each give
/// their exact error in the places of the writer with nothing sent, and the writer
/// goes on.
#[test]
fn a_call_that_the_writer_cannot_send_gives_its_error_and_the_writer_goes_on() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels)).unwrap();
    let unsent = program::Error::Unsent {
        count: 0,
        form: types::frame::Form::Raw,
    };
    assert_eq!(writer.write(&[]), Err(Error::Stream(unsent)));
    let range = types::frame::Error::OutOfRange {
        entry: 2,
        entries: 2,
    };
    let past = Err(Error::Stream(program::Error::Layout(range)));
    assert_eq!(writer.write(&[(2, Value::I64(1))]), past);
    assert_eq!(
        writer.write_series(1, &[(2, 1_i64.to_le_bytes().as_slice())]),
        past
    );
    let layout = |error| Err(Error::Stream(program::Error::Layout(error)));
    let twice = [(0, Value::I64(1)), (0, Value::I64(2))];
    let unordered = types::frame::Error::Unordered { entry: 0, last: 0 };
    assert_eq!(writer.write(&twice), layout(unordered));
    let absent = types::frame::Error::IndexAbsent { entry: 0, index: 1 };
    assert_eq!(
        writer.write_series(1, &[(0, 1_i64.to_le_bytes().as_slice())]),
        layout(absent)
    );
    let samples = 4096_u32;
    let stamps: Vec<u8> = (1..=i64::from(samples))
        .flat_map(i64::to_le_bytes)
        .collect();
    let values = vec![0; stamps.len()];
    let large = block::Error::TooLarge {
        requested: 65536,
        largest: 57344,
    };
    assert_eq!(
        writer.write_series(samples, &[(0, values.as_slice()), (1, stamps.as_slice())]),
        Err(Error::Stream(program::Error::Pool(large)))
    );
    assert_eq!(writer.write(&[(0, Value::I64(5))]), Ok(taken(0, 1)));
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A `Layout` error of a writer whose places are not in the order of the node's places
/// names the places of the writer.
#[test]
fn a_layout_error_names_the_places_of_the_writer() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = ["plant.b".parse().unwrap(), "plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels.into())).unwrap();
    let (b, a, time) = (0, 1, 2);
    assert_eq!(writer.index(), time);
    let one = 1_i64.to_le_bytes();
    let absent = writer.write_series(1, &[(a, one.as_slice())]);
    let twice = writer.write(&[(b, Value::I64(1)), (b, Value::I64(2))]);
    let layout = |error| Err(Error::Stream(program::Error::Layout(error)));
    assert_eq!(
        (absent, twice),
        (
            layout(types::frame::Error::IndexAbsent {
                entry: a,
                index: time
            }),
            layout(types::frame::Error::Unordered { entry: b, last: b }),
        )
    );
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// Once the node ends the session, each call of a writer and of its client gives the
/// cause. The client gets the close of the node in real time, so the test writes
/// until a write fails.
#[test]
fn each_call_after_the_node_ended_the_session_gives_the_cause() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = || vec!["plant.a".parse().unwrap()];
    let mut writer = client.open_writer(Config::new(channels())).unwrap();
    let other = client.open_writer(Config::new(channels())).unwrap();
    rig.stop();
    let failed = rig.wait("a write fails after the node stops", || {
        match writer.write(&[(0, Value::I64(1))]) {
            Ok(outcome) => Err(format!("{outcome:?}")),
            Err(error) => Ok(error),
        }
    });
    let closed = transport::Error::PeerClosed {
        code: transport::Code(0),
    };
    let ended = Error::Session(program::Error::Transport(closed));
    assert_eq!(failed, ended);
    assert_eq!(writer.close(), Err(ended.clone()));
    assert_eq!(other.close(), Err(ended.clone()));
    let opened = client.open_writer(Config::new(channels())).map(drop);
    assert_eq!(opened, Err(ended.clone()));
    assert_eq!(client.close(), Err(ended));
}

/// Only Unix: `counting::Local` names the thread with `pthread_self`. A warm `write`
/// and `write_series` make no allocation on the caller's thread.
#[cfg(unix)]
#[test]
fn a_warm_write_makes_no_allocation_on_the_thread_of_its_caller() {
    const WARMUP: usize = 100;
    const CALLS: usize = 1_000;
    const SAMPLES: u32 = 64;
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = ["plant.a", "plant.b"].map(|name| name.parse().unwrap());
    let config = || Config::new(channels.to_vec());
    let mut writer = client.open_writer(config()).unwrap();
    let values = [(0, Value::I64(1)), (1, Value::I64(2))];
    let mut write = || {
        let written = writer.write(&values);
        assert!(matches!(written, Ok(Outcome::Taken { .. })), "{written:?}");
    };
    (0..WARMUP).for_each(|_| write());
    let ((), allocations) = crate::ALLOCATOR.count(|| (0..CALLS).for_each(|_| write()));
    assert_eq!(allocations, 0, "{CALLS} warm writes of values allocate");
    assert_eq!(writer.close(), Ok(()));

    let mut writer = client.open_writer(config()).unwrap();
    let index = writer.index();
    let len = 8 * usize::try_from(SAMPLES).unwrap();
    let (values, mut stamps) = (vec![3_u8; len], vec![0_u8; len]);
    let mut next = now() + 1_000_000_000;
    let mut write = || {
        for stamp in stamps.as_chunks_mut::<8>().0 {
            stamp.copy_from_slice(&next.to_le_bytes());
            next += 1;
        }
        let series = [(0, values.as_slice()), (index, stamps.as_slice())];
        let written = writer.write_series(SAMPLES, &series);
        assert!(matches!(written, Ok(Outcome::Taken { .. })), "{written:?}");
    };
    (0..WARMUP).for_each(|_| write());
    let ((), allocations) = crate::ALLOCATOR.count(|| (0..CALLS).for_each(|_| write()));
    assert_eq!(allocations, 0, "{CALLS} warm writes of series allocate");
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

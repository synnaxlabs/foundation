//! The readers of the Rust SDK against a node process.

use env::tasks::{Group, Tasks};
use program::Refusal;
use sdk::read::{self, Frame, Mode, Reader};
use sdk::write::{Config, Writer};
use sdk::{Client, Error, Place};
use types::authority::Authority;
use types::frame::{Form, Path};
use types::sample::{Scalar, Type, Value};

use super::{planted, session, taken, wait};

/// A reader of `channels` of `mode` on `client`.
fn reader(client: &Client, channels: &[&str], mode: Mode) -> Reader {
    let channels = channels.iter().map(|name| name.parse().unwrap()).collect();
    let config = read::Config { channels, mode };
    client.open_reader(config).unwrap()
}

/// A writer of `channels` on `client`.
fn writer(client: &Client, channels: &[&str]) -> Writer {
    let channels = channels.iter().map(|name| name.parse().unwrap()).collect();
    client.open_writer(Config::new(channels)).unwrap()
}

/// The largest frame that a writer of `sdk` on `client` sends, from its refusal of a
/// larger one.
fn draft_max(client: &Client) -> usize {
    let mut writer = writer(client, &["plant.a"]);
    let (a, time) = (at(writer.places(), "plant.a"), writer.index());
    let bytes = vec![0; 1 << 20];
    let series = [(a, bytes.as_slice()), (time, bytes.as_slice())];
    match writer.write_series(1 << 17, &series) {
        Err(Error::Stream(program::Error::Pool(block::Error::TooLarge {
            largest,
            ..
        }))) => largest,
        other => panic!("{other:?}"),
    }
}

/// The names of the places of `reader`.
fn names(reader: &Reader) -> Vec<String> {
    reader.places().iter().map(|p| p.name.to_string()).collect()
}

/// The place of the channel `name` in `places`.
fn at(places: &[Place], name: &str) -> usize {
    let name = name.parse().unwrap();
    places.iter().position(|place| place.name == name).unwrap()
}

/// The samples of the series of `place` of `frame`, each as `i64`.
fn values(frame: &Frame, place: usize) -> Vec<i64> {
    let (chunks, rest) = frame.series(place).unwrap().as_chunks::<8>();
    assert!(rest.is_empty());
    chunks
        .iter()
        .map(|chunk| i64::from_le_bytes(*chunk))
        .collect()
}

/// The next of a splitmix64 sequence from `state`.
fn mixed(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `count` random samples from `state`, which no codec makes smaller.
fn random(count: usize, state: &mut u64) -> Vec<i64> {
    (0..count)
        .map(|_| i64::from_le_bytes(mixed(state).to_le_bytes()))
        .collect()
}

/// The raw bytes of `samples`.
fn raw(samples: &[i64]) -> Vec<u8> {
    samples.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The earliest stamp that the node takes, in 2000.
const EPOCH: i64 = 946_684_800_000_000_001;

/// A reader of one name gets each frame that a writer writes after the open, in
/// order, with the index as a place and nothing lost.
#[test]
fn a_reader_of_a_name_gets_the_frames_of_a_writer_in_order() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    assert_eq!(names(&reader), ["plant.a", "plant.time"]);
    let (time, a) = (
        at(reader.places(), "plant.time"),
        at(reader.places(), "plant.a"),
    );
    let mut writer = writer(&client, &["plant.a", "plant.b"]);
    let (wa, wb) = (
        at(writer.places(), "plant.a"),
        at(writer.places(), "plant.b"),
    );
    let values_ab = [(wa, Value::I64(1)), (wb, Value::I64(2))];
    assert_eq!(writer.write(&values_ab), Ok(taken(0, 1)));
    assert_eq!(writer.write(&[(wa, Value::I64(3))]), Ok(taken(1, 1)));
    let first = reader.next().unwrap();
    assert_eq!((first.count(), first.index()), (1, time));
    assert_eq!(values(first, a), [1]);
    assert_eq!(first.series(2), None);
    assert_eq!(first.lost(a), 0..0);
    let stamp = values(first, time)[0];
    let second = reader.next().unwrap();
    assert_eq!(values(second, a), [3]);
    assert!(values(second, time)[0] > stamp);
    assert_eq!(writer.close(), Ok(()));
    drop(reader);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// Place `n` of a reader is the channel of name `n`, also when it comes before
/// another of its index or comes twice. Then the index, when no name names it.
#[test]
fn a_place_of_a_reader_is_the_channel_of_its_name_then_its_index() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut swapped = reader(&client, &["plant.b", "plant.a"], Mode::Complete);
    assert_eq!(names(&swapped), ["plant.b", "plant.a", "plant.time"]);
    let mut twice = reader(&client, &["plant.a", "plant.a"], Mode::Complete);
    assert_eq!(names(&twice), ["plant.a", "plant.a", "plant.time"]);
    let mut named = reader(&client, &["plant.time", "plant.a"], Mode::Complete);
    assert_eq!(names(&named), ["plant.time", "plant.a"]);
    let mut plant = writer(&client, &["plant.a", "plant.b"]);
    let (wa, wb) = (at(plant.places(), "plant.a"), at(plant.places(), "plant.b"));
    let pair = [(wa, Value::I64(1)), (wb, Value::I64(2))];
    assert_eq!(plant.write(&pair), Ok(taken(0, 1)));
    let frame = swapped.next().unwrap();
    assert_eq!(
        (values(frame, 0), values(frame, 1), frame.index()),
        (vec![2], vec![1], 2)
    );
    let frame = twice.next().unwrap();
    assert_eq!(
        (values(frame, 0), values(frame, 1), frame.index()),
        (vec![1], vec![1], 2)
    );
    let frame = named.next().unwrap();
    assert_eq!((values(frame, 1), frame.index()), (vec![1], 0));
    assert_eq!(plant.close(), Ok(()));
    drop((swapped, twice, named));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// The node refuses a reader of channels of two indexes with `MALFORMED`, as it
/// refuses a writer, and the client goes on.
#[test]
fn an_open_of_a_reader_of_two_indexes_gives_malformed() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let channels = ["plant.a", "cell.c"].map(|name| name.parse().unwrap());
    let opened = client.open_reader(read::Config::new(channels.to_vec()));
    let malformed = Error::Stream(program::Error::Refused(Refusal::Malformed));
    assert_eq!(opened.map(drop), Err(malformed));
    drop(reader(&client, &["plant.a"], Mode::Complete));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A latest reader gets a frame that a writer writes, with nothing lost.
#[test]
fn a_latest_reader_gets_a_frame_that_a_writer_writes() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Latest);
    let a = at(reader.places(), "plant.a");
    let mut writer = writer(&client, &["plant.a"]);
    let wa = at(writer.places(), "plant.a");
    assert_eq!(writer.write(&[(wa, Value::I64(4))]), Ok(taken(0, 1)));
    let frame = reader.next().unwrap();
    assert_eq!(values(frame, a), [4]);
    assert_eq!(frame.lost(a), 0..0);
    assert_eq!(writer.close(), Ok(()));
    drop(reader);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A name of no channel gives `UNKNOWN`, alone and beside a name of a channel, and
/// the client goes on.
#[test]
fn an_open_of_a_reader_of_an_unknown_name_gives_unknown() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let unknown = Error::Stream(program::Error::Refused(Refusal::Unknown));
    for channels in [&["plant.missing"][..], &["plant.a", "plant.missing"]] {
        let channels = channels.iter().map(|name| name.parse().unwrap()).collect();
        let opened = client.open_reader(read::Config::new(channels)).map(drop);
        assert_eq!(opened, Err(unknown.clone()));
    }
    drop(reader(&client, &["plant.a"], Mode::Complete));
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// The close of a client with a live reader returns, and each later call of the
/// reader gives `Closed`.
#[test]
fn a_close_of_a_client_with_a_live_reader_returns_and_ends_the_reader() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    assert_eq!(client.close(), Ok(()));
    assert_eq!(reader.next().map(drop), Err(Error::Closed));
    assert_eq!(reader.next().map(drop), Err(Error::Closed));
    rig.stop();
}

/// A reader reads whole the largest frame that a writer of `sdk` sends, of samples
/// that no codec makes smaller, which is larger than the largest draft encoded.
#[test]
fn a_reader_reads_the_largest_frame_that_a_writer_of_the_sdk_writes() {
    const SAMPLES: usize = 3_581;
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let draft_max = draft_max(&client);
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    let a = at(reader.places(), "plant.a");
    let mut writer = writer(&client, &["plant.a"]);
    let (wa, time) = (at(writer.places(), "plant.a"), writer.index());
    let mut state = 7;
    let samples = random(SAMPLES, &mut state);
    // Each vector of 1,024 stamps has one step of 2^56, so no codec saves 1/8 of it.
    let mut stamp = EPOCH;
    let stamps: Vec<i64> = (0..SAMPLES)
        .map(|n| {
            stamp += if n % 1024 == 500 {
                1 << 56
            } else {
                i64::try_from(mixed(&mut state) >> 44).unwrap() + 1
            };
            stamp
        })
        .collect();
    let (sample_bytes, stamp_bytes) = (raw(&samples), raw(&stamps));
    assert!(sample_bytes.len() + stamp_bytes.len() <= draft_max);
    let count = u32::try_from(SAMPLES).unwrap();
    let series = [
        (wa, sample_bytes.as_slice()),
        (time, stamp_bytes.as_slice()),
    ];
    assert_eq!(writer.write_series(count, &series), Ok(taken(0, count)));
    let frame = reader.next().unwrap();
    assert_eq!(frame.count(), count);
    assert_eq!(values(frame, a), samples);
    assert_eq!(writer.close(), Ok(()));
    drop(reader);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A reader reads whole a frame larger, encoded, than the largest draft of a writer of
/// `sdk`, from a writer of a session with larger drafts.
#[test]
fn a_reader_reads_a_frame_larger_than_a_draft_of_the_sdk() {
    const SAMPLES: usize = 7_400;
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject.clone()).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    let a = at(reader.places(), "plant.a");
    let samples = random(SAMPLES, &mut 7);
    let stamps: Vec<i64> = (0..).take(SAMPLES).map(|n| EPOCH + n).collect();
    let draft_max = draft_max(&client);
    let (written, outcome) = std::sync::mpsc::sync_channel(1);
    let raws = [raw(&samples), raw(&stamps)];
    let main = move |tasks: Tasks| async move {
        let group = Group::new(tasks);
        let session = session(node, subject, group.tasks().clone(), 1 << 20).await;
        let config = program::write::Config {
            authority: Authority(0),
            lease: None,
            channels: vec!["plant.a".parse().unwrap()],
        };
        let mut writer = session.writer(config).await.unwrap();
        let time = writer.index();
        let encoded: Vec<Vec<u8>> = (0..2)
            .map(|place| {
                let raw = &raws[usize::from(place == time)];
                let data_type = Type::Scalar(Scalar::I64);
                let mut out = vec![0; codec::max_len(data_type, raw.len())];
                let len = codec::Encoder::new(data_type)
                    .encode(SAMPLES, raw, &mut out)
                    .unwrap();
                out.truncate(len);
                out
            })
            .collect();
        assert!(encoded[1 - time].len() > draft_max);
        let layout: Vec<_> = encoded.iter().map(Vec::len).enumerate().collect();
        let count = u32::try_from(SAMPLES).unwrap();
        let draft = writer.draft(Form::Encoded, count, &layout).await.unwrap();
        for ((_, to), bytes) in draft.iter_mut().zip(&encoded) {
            to.copy_from_slice(bytes);
        }
        let time = program::write::Time::Frame(Path::Live);
        written.send(writer.write(time).await.unwrap()).unwrap();
        writer.finish().await.unwrap();
        drop(session);
        group.ended().await;
    };
    let config = env::shards::Config {
        name: "writer".into(),
        core: None,
    };
    let handle = os::shards().unwrap().start(config, main).unwrap();
    let count = u32::try_from(SAMPLES).unwrap();
    assert_eq!(wait(&outcome), taken(0, count));
    handle.join().unwrap();
    let frame = reader.next().unwrap();
    assert_eq!(frame.count(), count);
    assert_eq!(values(frame, a), samples);
    drop(reader);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

/// A reader that waits for a frame when the node ends the session gives the cause at
/// each call, and so does the next open.
#[test]
fn a_reader_gives_the_cause_when_the_node_ends_the_session() {
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    rig.stop();
    let closed = transport::Error::PeerClosed {
        code: transport::Code(0),
    };
    let ended = Error::Session(program::Error::Transport(closed));
    assert_eq!(reader.next().map(drop), Err(ended.clone()));
    assert_eq!(reader.next().map(drop), Err(ended.clone()));
    let channels = vec!["plant.a".parse().unwrap()];
    let opened = client.open_reader(read::Config::new(channels)).map(drop);
    assert_eq!(opened, Err(ended.clone()));
    assert_eq!(client.close(), Err(ended));
}

/// Asserts that a warm `next` makes no allocation on the caller's thread. A complete
/// reader gets each frame after its commit, so each call is one record of the ring,
/// which the node never trims (#160): about 4,000 calls fill it.
#[cfg(unix)]
pub(super) fn a_warm_next_makes_no_allocation() {
    const WARMUP: usize = 100;
    const CALLS: usize = 300;
    let (mut rig, node, subject) = planted();
    let client = Client::connect(&node, subject).unwrap();
    let mut reader = reader(&client, &["plant.a"], Mode::Complete);
    let mut writer = writer(&client, &["plant.a"]);
    let mut call = || {
        let written = writer.write(&[(0, Value::I64(1))]);
        assert!(
            matches!(written, Ok(sdk::write::Outcome::Taken { .. })),
            "{written:?}"
        );
        let frame = reader.next().unwrap();
        assert_eq!(frame.count(), 1);
    };
    (0..WARMUP).for_each(|_| call());
    let ((), allocations) = crate::ALLOCATOR.count(|| (0..CALLS).for_each(|_| call()));
    assert_eq!(allocations, 0, "{CALLS} warm calls of next allocate");
    assert_eq!(writer.close(), Ok(()));
    drop(reader);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

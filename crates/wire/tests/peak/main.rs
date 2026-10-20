//! The peak heap of a `Start` at `PLACES_MAX` with names of 255 bytes, which CLIENT
//! HELLO bounds at 26 MB at the program, for each split of the run, and of the
//! `write::Frames` of a write stream at `PLACES_MAX`, and of a run of short names
//! against what `Opening::held` gives. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::sync::Arc;

use types::channel;
use types::frame::key_set::KeySet;
use types::sample::{Scalar, Type};
use wire::hub::client::{Error, Mode, PLACES_MAX, Place, Places, Reader, place};
use wire::hub::{client, write};

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The bound of CLIENT HELLO.
const BOUND: usize = 26_000_000;

fn main() {
    let places = places();
    let count = places.len();
    let splits: [(&str, Vec<usize>); 6] = [
        ("one message", vec![count]),
        ("one place in each", vec![1; count]),
        (
            "1,023, then one in each",
            std::iter::once(1023)
                .chain(std::iter::repeat_n(1, count - 1023))
                .collect(),
        ),
        ("all but two, then one in each", vec![count - 2, 1, 1]),
        ("two halves", vec![count / 2; 2]),
        ("one, then the rest", vec![1, count - 1]),
    ];
    for (name, split) in splits {
        let bytes = peak(&places, &split);
        assert!(bytes <= BOUND, "{name}: the start takes {bytes} bytes");
    }
    let bytes = write_frames(&places);
    assert_eq!(bytes, 20 * count, "20 bytes for each place of a write");
    held_over_a_run();
    held_over_a_refused_message();
    held_by_a_start();
}

/// Places of a name of one byte, the index at place 0.
fn tiny(count: usize) -> Vec<Place> {
    (0..count)
        .map(|n| Place {
            key: channel::Key::from_u128(u128::try_from(n).expect("fits") + 1),
            data_type: Type::Scalar(if n == 0 { Scalar::Stamp } else { Scalar::I64 }),
            name: "c".parse().expect("a name"),
            index: 0,
        })
        .collect()
}

/// `places` as one message of the run.
fn encode_one(places: &[Place]) -> Vec<u8> {
    let message = place::messages(places, usize::MAX).next().expect("one");
    let mut bytes = vec![0; message.encoded_len()];
    message.encode(&mut bytes);
    bytes
}

/// The first message of a run of `count` places.
fn first(count: u32) -> [u8; Places::LEN] {
    let mut bytes = [0; Places::LEN];
    Places { count }.encode(&mut bytes);
    bytes
}

/// The heap of a run in two messages never peaks over what the decoder holds after
/// the first: the room of the count and the bytes of the names.
fn held_over_a_run() {
    const COUNT: usize = 2600;
    let places = tiny(COUNT);
    let messages = [
        encode_one(&places[..COUNT - 1]),
        encode_one(&places[COUNT - 1..]),
    ];
    let before = ALLOCATOR.held();
    ALLOCATOR.reset_peak();
    let mut opening = client::write::Opening::default();
    let count = u32::try_from(COUNT).expect("fits");
    opening
        .decode(Some(&first(count)))
        .expect("the places decode");
    let decoded = opening
        .decode(Some(&messages[0]))
        .expect("a message of the run");
    assert!(decoded.is_none(), "one place remains");
    let peak = ALLOCATOR.peak() - before;
    let held = usize::try_from(opening.held()).expect("fits");
    assert_eq!(
        ALLOCATOR.held() - before,
        held,
        "the decoder holds what it counts"
    );
    assert!(
        peak <= held,
        "a run of {COUNT} places peaks at {peak}, over {held}"
    );
}

/// A message of more places than remain takes no heap past the room of the count, the
/// names of the places of that room, and one name of a place that drops.
fn held_over_a_refused_message() {
    let places = tiny(2600);
    let message = encode_one(&places);
    let before = ALLOCATOR.held();
    ALLOCATOR.reset_peak();
    let mut opening = client::write::Opening::new(2);
    opening.decode(Some(&first(2))).expect("the places decode");
    let refused = opening.decode(Some(&message)).map(|_| ());
    assert_eq!(
        refused,
        Err(Error::Run {
            items: 2600,
            remain: 2,
        }),
        "the message holds more places than remain"
    );
    let peak = ALLOCATOR.peak() - before;
    let most = usize::try_from(opening.held()).expect("fits") + 2 + 1;
    assert!(
        peak <= most,
        "a refused message peaks at {peak}, over {most}"
    );
}

/// The start of a run in two messages keeps what the decoder holds and its key set,
/// which `held` does not count: 32 bytes for each place and a few headers. Its last
/// decode peaks at no more than that and 48 bytes for each place.
fn held_by_a_start() {
    const COUNT: usize = 2600;
    let places = tiny(COUNT);
    let messages = [
        encode_one(&places[..COUNT - 1]),
        encode_one(&places[COUNT - 1..]),
    ];
    let before = ALLOCATOR.held();
    let mut opening = client::write::Opening::default();
    let count = u32::try_from(COUNT).expect("fits");
    opening
        .decode(Some(&first(count)))
        .expect("the places decode");
    opening
        .decode(Some(&messages[0]))
        .expect("a message of the run");
    ALLOCATOR.reset_peak();
    let start = opening
        .decode(Some(&messages[1]))
        .expect("the last message")
        .expect("the start");
    let peak = ALLOCATOR.peak() - before;
    let kept = ALLOCATOR.held() - before;
    let held = usize::try_from(opening.held()).expect("fits");
    drop(start);
    let set = held + 32 * COUNT + 128;
    assert!(kept <= set, "a start keeps {kept} bytes, over {set}");
    let most = set + 48 * COUNT;
    assert!(
        peak <= most,
        "the last message peaks at {peak}, over {most}"
    );
}

/// The places at the cap, each name 255 bytes, the index at place 0.
fn places() -> Vec<Place> {
    let count = usize::try_from(PLACES_MAX).expect("fits");
    (0..count)
        .map(|n| Place {
            key: channel::Key::from_u128(u128::try_from(n).expect("fits") + 1),
            data_type: Type::Scalar(if n == 0 { Scalar::Stamp } else { Scalar::I64 }),
            name: format!("c{n:0>254}").parse().expect("a name"),
            index: 0,
        })
        .collect()
}

/// The peak heap of [`write::Frames::new`] over the key set of `places`, built outside
/// the count.
fn write_frames(places: &[Place]) -> usize {
    let channels = places.iter().map(|place| (place.key, place.data_type, 0));
    let set = KeySet::ordered(channels).expect("no key repeats");
    let set = Arc::new(set);
    let before = ALLOCATOR.held();
    ALLOCATOR.reset_peak();
    let frames = write::Frames::new(set);
    let bytes = ALLOCATOR.peak() - before;
    drop(frames);
    bytes
}

/// The peak heap of the decoder over the run of `places`, with `split[i]` places in
/// message `i`, above what it held before.
fn peak(places: &[Place], split: &[usize]) -> usize {
    let mut rest = places;
    let messages: Vec<Vec<u8>> = split
        .iter()
        .map(|&n| {
            let (chunk, after) = rest.split_at(n);
            rest = after;
            let message = place::messages(chunk, usize::MAX).next().expect("one");
            let mut bytes = vec![0; message.encoded_len()];
            message.encode(&mut bytes);
            bytes
        })
        .collect();
    let mut first = [0; Places::LEN];
    Places { count: PLACES_MAX }.encode(&mut first);
    let before = ALLOCATOR.held();
    ALLOCATOR.reset_peak();
    let mut reader = Reader::new(Mode::Complete);
    reader.decode(Some(&first)).expect("the places decode");
    let mut start = None;
    for message in &messages {
        start = reader.decode(Some(message)).expect("a message of the run");
    }
    assert!(start.is_some(), "the last place gives the start");
    ALLOCATOR.peak() - before
}

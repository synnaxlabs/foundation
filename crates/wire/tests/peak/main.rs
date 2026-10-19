//! The peak heap of a `Start` at `PLACES_MAX` with names of 255 bytes, which CLIENT
//! HELLO bounds at 26 MB at the program, for each split of the run, and of the
//! `write::Frames` of a write stream at `PLACES_MAX`. This binary has no test harness:
//! the count covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::sync::Arc;

use types::channel;
use types::frame::key_set::KeySet;
use types::sample::{Scalar, Type};
use wire::hub::client::{Mode, PLACES_MAX, Place, Places, Reader, place};
use wire::hub::write;

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
    assert_eq!(bytes, 16 * count, "16 bytes for each place of a write");
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

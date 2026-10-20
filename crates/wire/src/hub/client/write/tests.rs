use proptest::prelude::*;
use types::name;

use super::*;
use crate::hub::client::{First, OPEN_BYTES_MAX};

fn write(length: u64) -> Write {
    Write {
        length,
        signature: [0x5e; 64],
    }
}

fn open(lease: Option<Span>, names: &[&str]) -> Open {
    Open {
        authority: Authority(3),
        lease,
        channels: names.iter().map(|name| name.parse().unwrap()).collect(),
    }
}

fn encode_write(write: Write) -> Vec<u8> {
    let mut out = vec![0xaa; Write::LEN];
    write.encode(&mut out);
    out
}

fn encode_open(open: &Open) -> Vec<u8> {
    let mut out = vec![0xaa; open.encoded_len()];
    open.encode(&mut out);
    out
}

fn not_utf8(bytes: &[u8]) -> text::Error<Box<name::Error>> {
    text::Error::Utf8(std::str::from_utf8(bytes).unwrap_err())
}

/// `kind`, then `len - 1` zeros.
fn zeros(kind: u8, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    bytes[0] = kind;
    bytes
}

mod pins {
    use super::*;

    #[test]
    fn the_write() {
        let mut bytes = vec![7, 0x03, 0x02, 0, 0, 0, 0, 0, 0];
        bytes.extend([0x5e; 64]);
        assert_eq!(encode_write(write(0x0203)), bytes);
    }

    #[test]
    fn the_open() {
        let mut bytes = vec![3, 0, 8];
        bytes.extend(b"site_a.x");
        bytes.push(1);
        bytes.extend(b"y");
        let none = open(None, &["site_a.x", "y"]);
        assert_eq!(none.encoded_len(), 13);
        assert_eq!(encode_open(&none), bytes);
        let lease = open(Some(Span::from_nanos(-0x0102)), &["y"]);
        let mut bytes = vec![3, 1];
        bytes.extend((-0x0102_i64).to_le_bytes());
        bytes.extend([1, b'y']);
        assert_eq!(lease.encoded_len(), 12);
        assert_eq!(encode_open(&lease), bytes);
    }
}

mod first {
    use super::*;

    #[test]
    fn decodes_a_write() {
        let bytes = encode_write(write(4));
        assert_eq!(First::decode(&bytes), Ok(First::Write(write(4))));
    }

    #[test]
    fn gives_the_errors_of_a_write() {
        assert_eq!(First::decode(&[7]), Err(Error::Length { len: 1 }));
        let mut over = encode_write(write(1));
        over[1..9].copy_from_slice(&(OPEN_BYTES_MAX + 1).to_le_bytes());
        assert_eq!(
            First::decode(&over),
            Err(Error::Oversize {
                length: OPEN_BYTES_MAX + 1,
                cap: OPEN_BYTES_MAX,
            })
        );
    }
}

mod write {
    use super::*;

    #[test]
    fn refuses_each_other_kind() {
        for kind in (0..=u8::MAX).filter(|&kind| kind != 7) {
            assert_eq!(
                Write::decode(&zeros(kind, Write::LEN)),
                Err(Error::Kind { kind })
            );
        }
    }

    #[test]
    fn refuses_an_empty_message() {
        assert_eq!(Write::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_each_wrong_length() {
        for len in [1, 9, 72, 74] {
            assert_eq!(Write::decode(&zeros(7, len)), Err(Error::Length { len }));
        }
    }

    #[test]
    fn checks_the_length_before_the_cap() {
        let mut long = zeros(7, Write::LEN + 1);
        long[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Write::decode(&long), Err(Error::Length { len: 74 }));
    }

    #[test]
    fn takes_an_open_at_the_cap_and_refuses_one_over_it() {
        let at = encode_write(write(OPEN_BYTES_MAX));
        assert_eq!(Write::decode(&at), Ok(write(OPEN_BYTES_MAX)));
        let mut over = at;
        over[1..9].copy_from_slice(&(OPEN_BYTES_MAX + 1).to_le_bytes());
        assert_eq!(
            Write::decode(&over),
            Err(Error::Oversize {
                length: OPEN_BYTES_MAX + 1,
                cap: OPEN_BYTES_MAX,
            })
        );
    }

    #[test]
    fn gives_its_body() {
        assert_eq!(write(5).body().remain(), 5);
        assert_eq!(write(0).body().remain(), 0);
    }

    #[test]
    #[should_panic(expected = "an open of 65537 bytes is over the cap of 65536")]
    fn panics_on_an_encode_over_the_cap() {
        encode_write(write(OPEN_BYTES_MAX + 1));
    }

    #[test]
    #[should_panic(expected = "an open of 65537 bytes is over the cap of 65536")]
    fn panics_on_a_body_over_the_cap() {
        drop(write(OPEN_BYTES_MAX + 1).body());
    }
}

mod open {
    use super::*;

    #[test]
    fn refuses_an_empty_open() {
        assert_eq!(Open::decode(&[]), Err(Error::Empty));
    }

    #[test]
    fn refuses_a_lease_byte_that_is_neither_0_nor_1() {
        for byte in 2..=u8::MAX {
            assert_eq!(
                Open::decode(&[3, byte, 1, b'a']),
                Err(Error::Lease { byte })
            );
        }
    }

    #[test]
    fn refuses_an_open_that_ends_inside_a_field() {
        assert_eq!(Open::decode(&[3]), Err(Error::Length { len: 1 }));
        let mut lease = vec![3, 1];
        lease.extend([0; 7]);
        assert_eq!(Open::decode(&lease), Err(Error::Length { len: 9 }));
        assert_eq!(
            Open::decode(&[3, 0, 2, b'a']),
            Err(Error::Length { len: 4 })
        );
    }

    #[test]
    fn refuses_an_open_that_names_no_channel() {
        assert_eq!(Open::decode(&[3, 0]), Err(Error::Unnamed));
        let mut lease = vec![3, 1];
        lease.extend(5_i64.to_le_bytes());
        assert_eq!(Open::decode(&lease), Err(Error::Unnamed));
    }

    #[test]
    fn refuses_a_name_that_does_not_read() {
        assert_eq!(
            Open::decode(&[3, 0, 1, 0xff]),
            Err(Error::Channel(not_utf8(&[0xff])))
        );
        assert_eq!(
            Open::decode(&[3, 0, 1, b'a', 0]),
            Err(Error::Channel(text::Error::Parse(Box::new(
                name::Error::Empty
            ))))
        );
    }

    #[test]
    fn gives_the_first_defect_in_the_order_of_the_bytes() {
        assert_eq!(
            Open::decode(&[3, 0, 1, 0xff, 2, b'a']),
            Err(Error::Channel(not_utf8(&[0xff])))
        );
    }

    #[test]
    fn decodes_a_name_of_255_bytes() {
        let long = "a".repeat(255);
        let open = open(None, &[&long]);
        assert_eq!(Open::decode(&encode_open(&open)), Ok(open));
    }

    #[test]
    #[should_panic(expected = "the open of a write names no channel")]
    fn panics_on_an_encode_with_no_channel() {
        encode_open(&open(None, &[]));
    }
}

mod opening {
    use types::channel;

    use super::*;
    use crate::hub::client::{PLACES_MAX, Place, Places, place};

    /// A place named `a`, on the index at place `index`.
    fn pt(key: u128, data_type: &str, index: usize) -> Place {
        Place {
            key: channel::Key::from_u128(key),
            data_type: data_type.parse().unwrap(),
            name: "a".parse().unwrap(),
            index,
        }
    }

    fn encode_places(places: Places) -> Vec<u8> {
        let mut out = vec![0xaa; Places::LEN];
        places.encode(&mut out);
        out
    }

    fn messages(places: &[Place], max: usize) -> Vec<Vec<u8>> {
        place::messages(places, max)
            .map(|message| {
                let mut out = vec![0xaa; message.encoded_len()];
                message.encode(&mut out);
                out
            })
            .collect()
    }

    /// The `(key, sample type, slot)` of each entry of `start`'s key set, and its
    /// places.
    fn home(start: Option<Start>) -> (Vec<Place>, Vec<(channel::Key, String, u32)>) {
        let Some(Start { places, set }) = start else {
            panic!("the write gave no start");
        };
        let entries = set
            .entries()
            .iter()
            .map(|entry| (entry.key, entry.data_type.to_string(), entry.slot.get()))
            .collect();
        (places, entries)
    }

    #[test]
    fn gives_the_places_and_their_key_set_after_the_run() {
        let run = [pt(1, "u8", 1), pt(2, "timestamp", 1), pt(3, "u8", 1)];
        let messages = messages(&run, 57);
        assert_eq!(messages.len(), 2);
        let mut opening = Opening::default();
        let places = Places { count: 3 };
        assert!(matches!(
            opening.decode(Some(&encode_places(places))),
            Ok(None)
        ));
        assert!(matches!(opening.decode(Some(&messages[0])), Ok(None)));
        let (places, entries) = home(opening.decode(Some(&messages[1])).unwrap());
        assert_eq!(places, run);
        assert_eq!(
            entries,
            [
                (channel::Key::from_u128(1), "u8".to_owned(), 0),
                (channel::Key::from_u128(2), "timestamp".to_owned(), 1),
                (channel::Key::from_u128(3), "u8".to_owned(), 2),
            ]
        );
    }

    /// Places on two indexes give a key set of two groups, numbered in the order of
    /// their index places, and the index of each place.
    #[test]
    fn gives_a_key_set_of_each_index_of_the_places() {
        let run = [
            pt(1, "u8", 3),
            pt(2, "timestamp", 1),
            pt(3, "f64", 3),
            pt(4, "timestamp", 3),
            pt(5, "u8", 1),
        ];
        let mut opening = Opening::default();
        let places = Places { count: 5 };
        opening.decode(Some(&encode_places(places))).unwrap();
        let Some(Start { places, set }) =
            opening.decode(Some(&messages(&run, 300)[0])).unwrap()
        else {
            panic!("the write gave no start");
        };
        assert_eq!(places, run);
        assert_eq!(set.groups(), [1, 3]);
        let indexes: Vec<_> = (0..5).map(|entry| set.index(entry)).collect();
        assert_eq!(indexes, [3, 1, 3, 3, 1]);
    }

    #[test]
    #[should_panic(expected = "a stream is decoded after the decoder gave its start")]
    fn panics_on_a_decode_after_the_places() {
        let mut opening = Opening::default();
        let places = Places { count: 1 };
        opening.decode(Some(&encode_places(places))).unwrap();
        opening
            .decode(Some(&messages(&[pt(1, "timestamp", 0)], 300)[0]))
            .unwrap();
        drop(opening.decode(None));
    }

    #[test]
    fn gives_finished_when_the_node_finishes_before_the_last_place() {
        assert_eq!(Opening::default().decode(None).err(), Some(Error::Finished));
        let mut opening = Opening::default();
        let places = Places { count: 2 };
        opening.decode(Some(&encode_places(places))).unwrap();
        opening
            .decode(Some(&messages(&[pt(1, "timestamp", 0)], 300)[0]))
            .unwrap();
        assert_eq!(opening.decode(None).err(), Some(Error::Finished));
    }

    #[test]
    fn gives_the_errors_of_a_first_message_and_stays_as_it_was() {
        let mut opening = Opening::default();
        assert_eq!(
            opening.decode(Some(&zeros(5, Places::LEN))).err(),
            Some(Error::Kind { kind: 5 })
        );
        assert_eq!(
            opening.decode(Some(&zeros(7, Places::LEN))).err(),
            Some(Error::Kind { kind: 7 })
        );
        assert_eq!(
            opening.decode(Some(&[6])).err(),
            Some(Error::Length { len: 1 })
        );
        assert_eq!(
            opening.decode(Some(&zeros(6, Places::LEN))).err(),
            Some(Error::Places {
                count: 0,
                cap: PLACES_MAX
            })
        );
        assert_eq!(opening.decode(Some(&[])).err(), Some(Error::Empty));
        let places = Places { count: 1 };
        assert!(matches!(
            opening.decode(Some(&encode_places(places))),
            Ok(None)
        ));
    }

    /// Each refused last message leaves the run as it was, so a later one gives the
    /// start.
    #[test]
    fn gives_the_errors_of_the_run() {
        let mut opening = Opening::default();
        let places = Places { count: 2 };
        opening.decode(Some(&encode_places(places))).unwrap();
        let cases = [
            (
                [pt(1, "u8", 0), pt(2, "u8", 0)],
                Error::Unstamped {
                    place: 0,
                    sample: "u8".parse().unwrap(),
                },
            ),
            (
                [pt(1, "timestamp", 0), pt(2, "u8", 2)],
                Error::Index { place: 1, index: 2 },
            ),
            (
                [pt(1, "timestamp", 0), pt(2, "u8", 1)],
                Error::Unstamped {
                    place: 1,
                    sample: "u8".parse().unwrap(),
                },
            ),
            (
                [pt(1, "timestamp", 0), pt(1, "u8", 0)],
                Error::Repeated {
                    key: channel::Key::from_u128(1),
                },
            ),
        ];
        for (run, error) in cases {
            assert_eq!(
                opening.decode(Some(&messages(&run, 300)[0])).err(),
                Some(error),
                "{run:?}"
            );
        }
        let run = [pt(1, "timestamp", 0), pt(2, "u8", 0)];
        let (places, _) = home(opening.decode(Some(&messages(&run, 300)[0])).unwrap());
        assert_eq!(places, run);
    }

    #[test]
    fn takes_a_count_at_its_bound_and_refuses_one_past_it() {
        assert!(matches!(
            Opening::new(3).decode(Some(&places_of(3))),
            Ok(None)
        ));
        assert_eq!(
            Opening::new(3).decode(Some(&places_of(4))).err(),
            Some(Error::Places { count: 4, cap: 3 })
        );
    }

    /// The first message of the places, with any `count`.
    fn places_of(count: u32) -> Vec<u8> {
        let mut out = vec![6];
        out.extend_from_slice(&count.to_le_bytes());
        out
    }

    proptest! {
        #[test]
        fn takes_each_count_up_to_its_bound_and_refuses_one_over_it(
            most in 0..=2 * PLACES_MAX,
            count in 0..=2 * PLACES_MAX,
        ) {
            let mut opening = Opening::new(most);
            let decoded = opening.decode(Some(&places_of(count)));
            let cap = if count == 0 || count > PLACES_MAX {
                PLACES_MAX
            } else {
                most
            };
            if count <= cap && count > 0 {
                prop_assert!(matches!(decoded, Ok(None)));
            } else {
                prop_assert_eq!(decoded.err(), Some(Error::Places { count, cap }));
                prop_assert_eq!(opening.held(), 0);
                if most > 0 {
                    let one = opening.decode(Some(&places_of(1)));
                    prop_assert!(matches!(one, Ok(None)));
                }
            }
        }

        #[test]
        fn holds_the_room_of_its_places_and_the_bytes_of_their_names(
            names in proptest::collection::vec(name(), 1..64),
            max in 304_usize..2048,
        ) {
            let run: Vec<Place> = names
                .into_iter()
                .zip(1..)
                .map(|(name, key)| {
                    let data_type = if key == 1 { "timestamp" } else { "u8" };
                    Place {
                        key: channel::Key::from_u128(key),
                        data_type: data_type.parse().unwrap(),
                        name,
                        index: 0,
                    }
                })
                .collect();
            let count = u32::try_from(run.len()).unwrap();
            let mut opening = Opening::new(count);
            prop_assert_eq!(opening.held(), 0);
            opening.decode(Some(&encode_places(Places { count }))).unwrap();
            let room = run.len().checked_mul(size_of::<Place>()).unwrap();
            prop_assert_eq!(opening.held(), u64::try_from(room).unwrap());
            let mut repeated = run.clone();
            let key = channel::Key::from_u128(1);
            repeated.last_mut().unwrap().key = key;
            let refused = messages(&repeated, max).pop().unwrap();
            let (mut arrived, mut start) = (0_usize, None);
            for message in messages(&run, max) {
                let before = opening.held();
                let (_, cut) = message.split_last().unwrap();
                let cut = opening.decode(Some(cut));
                prop_assert!(matches!(cut, Err(Error::Length { .. })), "{:?}", cut);
                prop_assert_eq!(opening.held(), before);
                let more = place::decode(&message).unwrap().len();
                if arrived.checked_add(more) == Some(run.len()) && run.len() > 1 {
                    let error = opening.decode(Some(&refused)).err();
                    prop_assert_eq!(error, Some(Error::Repeated { key }));
                    prop_assert_eq!(opening.held(), before);
                }
                start = opening.decode(Some(&message)).unwrap();
                arrived = arrived.checked_add(more).unwrap();
                let names: usize =
                    run[..arrived].iter().map(|place| place.name.as_str().len()).sum();
                let held = room.checked_add(names).unwrap();
                prop_assert_eq!(opening.held(), u64::try_from(held).unwrap());
            }
            prop_assert_eq!(home(start).0, run);
        }
    }
}

fn name() -> impl Strategy<Value = Name> {
    "[a-z][a-z0-9_]{0,6}(\\.[a-z0-9_]{1,6}){0,2}".prop_map(|text| text.parse().unwrap())
}

proptest! {
    #[test]
    fn round_trips_a_write(length in 0..=OPEN_BYTES_MAX, signature: [u8; 32]) {
        let mut write = write(length);
        write.signature[32..].copy_from_slice(&signature);
        prop_assert_eq!(Write::decode(&encode_write(write)), Ok(write));
        prop_assert_eq!(First::decode(&encode_write(write)), Ok(First::Write(write)));
    }

    #[test]
    fn round_trips_an_open(
        authority: u8,
        lease in proptest::option::of(any::<i64>()),
        channels in proptest::collection::vec(name(), 1..8),
    ) {
        let open = Open {
            authority: Authority(authority),
            lease: lease.map(Span::from_nanos),
            channels,
        };
        prop_assert_eq!(Open::decode(&encode_open(&open)), Ok(open));
    }

    /// A node that forwards a write sends the open it decoded, encoded again, under
    /// the program's signature of the bytes it read.
    #[test]
    fn encodes_each_open_that_decodes_to_the_same_bytes(
        authority: u8,
        lease in proptest::option::of(any::<i64>()),
        channels in proptest::collection::vec(name(), 1..8),
        at: prop::sample::Index,
        byte: u8,
    ) {
        let open = Open {
            authority: Authority(authority),
            lease: lease.map(Span::from_nanos),
            channels,
        };
        let mut bytes = encode_open(&open);
        let at = at.index(bytes.len());
        bytes[at] = byte;
        if let Ok(decoded) = Open::decode(&bytes) {
            prop_assert_eq!(encode_open(&decoded), bytes);
        }
    }
}
